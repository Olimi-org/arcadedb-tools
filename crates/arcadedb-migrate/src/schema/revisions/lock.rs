//! The advisory migration lock — a singleton row in `schema_migrate_lock`
//! claimed per sync/rollout-apply so two concurrent migrators cannot
//! interleave DDL against an auto-committing engine.
//!
//! Design is doc-grounded on the engine's own concurrency guidance (SQL
//! reference, CREATE VERTEX page): *"the lookup must be backed by a UNIQUE
//! index on the property used in the WHERE clause; this is what makes the
//! operation atomic under concurrent writers."* The lock row's `key` carries
//! that UNIQUE index; claims are optimistic compare-and-set on the current
//! holder, with the index as the race backstop.
//!
//! Lease semantics: expiry stamps are epoch-millis written by the CLIENT —
//! acquiring machines must have roughly synchronized clocks (NTP). A crashed
//! runner's lock auto-expires after [`LOCK_LEASE_MS`]; before that, the error
//! names the holder and expiry so an operator can act. Re-acquiring with the
//! same holder id refreshes the lease (safe reentrancy for one process).

use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::tables::sql_escape;
use super::LOCK_TYPE;

/// How long a lock claim stays valid without renewal. Generous enough for
/// large copy phases; a crashed holder blocks new runs only until expiry.
pub const LOCK_LEASE_MS: i64 = 15 * 60 * 1000;

/// The singleton row's key.
const LOCK_KEY: &str = "migrate";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The acquire decision, pure so it can be unit-tested without a server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Claim {
    /// Nobody holds it → INSERT (UNIQUE index backstops concurrent inserts).
    Insert,
    /// CAS-refresh or steal: UPDATE the existing row, matching on its CURRENT
    /// holder so a concurrent fresh claim can't be clobbered.
    Cas { current_holder: String },
    /// Someone else holds a live lease.
    Held { holder: String, expires_ms: i64 },
}

fn decide(current: Option<(String, i64)>, now: i64, holder: &str) -> Claim {
    match current {
        None => Claim::Insert,
        Some((current_holder, expires_ms)) => {
            if expires_ms > now && current_holder != holder {
                Claim::Held {
                    holder: current_holder,
                    expires_ms,
                }
            } else {
                // Ours (refresh) or expired-other (steal): CAS on the holder.
                Claim::Cas { current_holder }
            }
        }
    }
}

/// Try to acquire the migration lease for `holder`. Errors when another
/// holder's lease is still live; steals silently once it has expired;
/// refreshing with the same holder id always succeeds.
pub async fn acquire_lock(
    client: &ArcadeDbClient,
    db: &str,
    holder: &str,
    lease_ms: i64,
) -> Result<()> {
    let res = client
        .query(
            db,
            &format!("SELECT holder, expires_ms FROM {LOCK_TYPE} WHERE key = '{LOCK_KEY}'"),
        )
        .await
        .map_err(|e| MigrateError::Server {
            context: "reading migration lock".into(),
            source: e,
        })?;

    let current = res.records.first().map(|rec| {
        let row = arcadedb_protocol::grpc_record_to_json(rec);
        let h = row
            .get("holder")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let e = row.get("expires_ms").and_then(serde_json::Value::as_i64);
        (h, e.unwrap_or(0))
    });

    let now = now_ms();
    let expires_at = now + lease_ms;
    let esc_holder = sql_escape(holder);

    match decide(current, now, holder) {
        Claim::Held {
            holder: held_by,
            expires_ms,
        } => {
            let expires_in = (expires_ms - now) / 1000;
            Err(MigrateError::Lock {
                message: format!(
                    "another migration run holds the schema lock ({held_by}, expires in \
                     ~{expires_in}s). Concurrent syncs would interleave auto-committed DDL. \
                     If this is a crashed run, wait for the lease to expire."
                ),
            })
        }
        Claim::Insert => {
            let sql = format!(
                "INSERT INTO {LOCK_TYPE} SET key = '{LOCK_KEY}', holder = '{esc_holder}', \
                 acquired_ms = {now}, expires_ms = {expires_at}"
            );
            if let Err(e) = client.execute(db, &sql).await {
                // Lost an insert race against a concurrent claimant (UNIQUE
                // backstop fired).
                if e.to_string().to_lowercase().contains("duplicat") {
                    return Err(MigrateError::Lock {
                        message: "lost the migration-lock race to a concurrent run — retry".into(),
                    });
                }
                return Err(MigrateError::Server {
                    context: "inserting migration lock".into(),
                    source: e,
                });
            }
            Ok(())
        }
        Claim::Cas { current_holder } => {
            let sql = format!(
                "UPDATE {LOCK_TYPE} SET holder = '{esc_holder}', acquired_ms = {now}, \
                 expires_ms = {expires_at} WHERE key = '{LOCK_KEY}' AND holder = '{}'",
                sql_escape(&current_holder)
            );
            let res = client
                .execute(db, &sql)
                .await
                .map_err(|e| MigrateError::Server {
                    context: "claiming migration lock".into(),
                    source: e,
                })?;
            if res.affected_records == 0 {
                // A concurrent claimant replaced the holder between our read
                // and write.
                return Err(MigrateError::Lock {
                    message: "lost the migration-lock race to a concurrent run — retry".into(),
                });
            }
            Ok(())
        }
    }
}

/// Release the lock if we still hold it (best-effort: a lost/stolen lock must
/// not fail the completed migration).
pub async fn release_lock(client: &ArcadeDbClient, db: &str, holder: &str) -> Result<()> {
    let sql = format!(
        "DELETE FROM {LOCK_TYPE} WHERE key = '{LOCK_KEY}' AND holder = '{}'",
        sql_escape(holder)
    );
    client
        .execute(db, &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "releasing migration lock".into(),
            source: e,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_row_inserts() {
        assert_eq!(decide(None, 1_000, "a"), Claim::Insert);
    }

    #[test]
    fn fresh_foreign_lease_is_held() {
        assert_eq!(
            decide(Some(("other".into(), 2_000)), 1_000, "a"),
            Claim::Held {
                holder: "other".into(),
                expires_ms: 2_000
            }
        );
    }

    #[test]
    fn own_lease_refreshes_even_when_fresh() {
        assert_eq!(
            decide(Some(("a".into(), 2_000)), 1_000, "a"),
            Claim::Cas {
                current_holder: "a".into()
            }
        );
    }

    #[test]
    fn expired_lease_is_stealable_via_cas() {
        assert_eq!(
            decide(Some(("other".into(), 500)), 1_000, "a"),
            Claim::Cas {
                current_holder: "other".into()
            }
        );
        // Boundary: expiry at exactly now is stealable.
        assert_eq!(
            decide(Some(("other".into(), 1_000)), 1_000, "a"),
            Claim::Cas {
                current_holder: "other".into()
            }
        );
    }
}
