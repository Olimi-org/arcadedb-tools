//! Per-override execution progress — the resume state that makes a failed
//! apply safe to re-run.
//!
//! ArcadeDB auto-commits DDL, so a swap (destructive) statement that fails
//! mid-override leaves the earlier statements durable with the override
//! unrecorded. Without progress state, the documented recovery — re-run —
//! would re-execute the *copy* phase too, duplicating every copied row (the
//! `>=` count check cannot see duplication).
//!
//! Progress rows make the re-run idempotent:
//!
//! - the copy phase writes its "done" marker **inside** the copy transaction
//!   ([`copy_marker_sql`]), so the marker commits atomically with the copied
//!   rows — a re-run after a committed copy skips it entirely;
//! - each swap statement is recorded *after* it executes ([`mark_swap`]),
//!   so a re-run skips already-applied swaps and resumes at the failure point.
//!
//! Rows are cleared once the override is fully recorded in
//! `schema_overrides_applied`. The residual window (crash between a swap
//! statement's execution and its marker) is unavoidable without engine-level
//! transactional DDL; it fails loudly on retry (e.g. `ALTER TYPE … NAME` on an
//! already-renamed type) instead of corrupting data.

use std::collections::BTreeSet;

use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::{tables::sql_escape, PROGRESS_TYPE};

/// The resume state of one override file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverrideProgress {
    /// The copy phase committed (marker was in the same transaction as the
    /// copied rows) — a re-run must NOT copy again.
    pub copy_done: bool,
    /// Swap-statement indexes already executed.
    pub swap_done: BTreeSet<i64>,
}

/// Read the progress rows for `filename` (empty state when none).
pub async fn read_progress(
    client: &ArcadeDbClient,
    db: &str,
    filename: &str,
) -> Result<OverrideProgress> {
    let res = client
        .query(
            db,
            &format!(
                "SELECT kind, ordinal FROM {PROGRESS_TYPE} WHERE filename = '{}'",
                sql_escape(filename)
            ),
        )
        .await
        .map_err(|e| MigrateError::Server {
            context: "reading override progress".into(),
            source: e,
        })?;
    let mut progress = OverrideProgress::default();
    for rec in res.records {
        let row = arcadedb_protocol::grpc_record_to_json(&rec);
        match row.get("kind").and_then(|v| v.as_str()) {
            Some("copy") => progress.copy_done = true,
            Some("swap") => {
                // The JSON bridge renders integer kinds as JSON numbers.
                let ordinal = row
                    .get("ordinal")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| MigrateError::Revision {
                        message: "progress swap row without integer ordinal".into(),
                    })?;
                progress.swap_done.insert(ordinal);
            }
            other => {
                return Err(MigrateError::Revision {
                    message: format!("unknown override progress kind {other:?}"),
                })
            }
        }
    }
    Ok(progress)
}

/// The `INSERT` that marks the copy phase done. **Must run inside the copy
/// transaction** (via `execute_in_transaction`) so the marker commits
/// atomically with the copied rows.
pub fn copy_marker_sql(filename: &str) -> String {
    format!(
        "INSERT INTO {PROGRESS_TYPE} SET filename = '{}', kind = 'copy'",
        sql_escape(filename)
    )
}

/// Record that swap statement `index` executed. Runs right after the statement
/// succeeds (auto-committed, like the statement it tracks).
pub async fn mark_swap(
    client: &ArcadeDbClient,
    db: &str,
    filename: &str,
    index: usize,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO {PROGRESS_TYPE} SET filename = '{}', kind = 'swap', ordinal = {index}",
        sql_escape(filename)
    );
    client
        .execute(db, &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "recording override swap progress".into(),
            source: e,
        })?;
    Ok(())
}

/// Delete the progress rows for `filename` — called once the override is fully
/// recorded in `schema_overrides_applied`. Best-effort: leftover rows are
/// harmless (the override never becomes pending again), so callers log-and-
/// continue on failure.
pub async fn clear_progress(client: &ArcadeDbClient, db: &str, filename: &str) -> Result<()> {
    let sql = format!(
        "BEGIN;\nDELETE FROM {PROGRESS_TYPE} WHERE filename = '{}';\nCOMMIT;",
        sql_escape(filename)
    );
    client
        .execute_language(db, "sqlscript", &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "clearing override progress".into(),
            source: e,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_marker_escapes_the_filename() {
        let sql = copy_marker_sql("weird'name.sql");
        assert!(sql.contains(r"'weird\'name.sql'"), "{sql}");
        assert!(sql.contains("kind = 'copy'"));
    }
}
