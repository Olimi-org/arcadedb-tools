//! The migration orchestrator — owns the DB connection + bookkeeping, computes
//! plans, applies them, and records state.
//!
//! The types it surfaces ([`ApplyReport`], [`Plan`], [`SyncDirError`]) live in
//! [`report`]; the filesystem I/O (reading schema/seed/override files, merging
//! schemas) lives in [`fs`]; statement semantics (copy vs swap classification,
//! phase splitting, verification pairs) live in [`classification`]. This
//! module is purely the orchestration logic that ties them together with the
//! engine (diff, introspect, revisions).
//!
//! ## Apply model
//!
//! A sync is *not* a single atomic batch — ArcadeDB auto-commits DDL as it
//! runs, so no wrapper script can make a migration atomic. Instead each
//! override applies in two phases:
//!
//! 1. **Copy phase** — the additive statements run inside one *explicit*
//!    server-side transaction, with every copied type's row count verified
//!    against its source *before* anything destructive runs. A "copy
//!    done" marker is written **inside the same transaction**, so a committed
//!    copy is never re-executed on retry;
//! 2. **Swap phase** — the destructive statements run individually (they
//!    commit instantly); each executed swap is recorded right after it runs,
//!    so a re-run resumes at the failure point instead of repeating durable
//!    work.
//!
//! Overrides are tracked per-file with their content checksum: an override
//! that was edited *after* being applied is a hard error (applied migrations
//! are immutable — create a new file). Overrides whose authored order the
//! phase split would reorder are rejected at plan time (see
//! [`classification::validate_phase_ordering`]).
//!
//! Only after all of that succeeds is the snapshot/bookkeeping written.

pub mod classification;
pub mod fs;
pub mod manifest;
pub mod report;
pub mod rollout;

pub use fs::{OverrideFile, SeedFile};
pub use report::{render_batch, ApplyReport, Plan, SyncDirError};
pub use rollout::{parse_rollout, render_rollout, Rollout};

use std::path::Path;

use arcadedb_protocol::proto::com::arcadedb::grpc::grpc_value::Kind;
use arcadedb_protocol::proto::com::arcadedb::grpc::TransactionIsolation;
use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};
use crate::schema::diff::{diff_with_snapshot, DiffAction, DropStrategy};
use crate::schema::introspect;
use crate::schema::model::{Schema, SchemaSnapshot, TimeseriesColumn, TypeKind};
use crate::schema::revisions;
use crate::schema::revisions::checksum;
use crate::schema::revisions::{
    acquire_lock, clear_progress, copy_marker_sql, mark_swap, read_progress, release_lock,
    LOCK_LEASE_MS,
};

use classification::{split_override_phases, validate_phase_ordering};
use fs::read_overrides_checked;

/// The migration manager. Owns the DB connection + bookkeeping.
pub struct Migrator {
    client: ArcadeDbClient,
    db: String,
    /// Identity used for the advisory migration lock (`pid:nanos`) — stable
    /// for this instance, unique across concurrent runs.
    holder: String,
}

impl Migrator {
    /// Construct a migrator and ensure its bookkeeping tables exist.
    pub async fn new(client: ArcadeDbClient, db: impl Into<String>) -> Result<Self> {
        let db = db.into();
        revisions::ensure_tables(&client, &db).await?;
        let holder = format!(
            "pid:{}:{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        Ok(Self { client, db, holder })
    }

    /// Parse a schema directory's `.sql` files, diff against the live DB, apply
    /// the reconciliation DDL + any unapplied override migrations, then run the
    /// seed phase.
    ///
    /// - `schema_dir/*.sql` — `CREATE TYPE/PROPERTY/INDEX` DDL (parsed strictly).
    /// - `schema_dir/overrides/*.sql` — sequential one-shot migrations (ALTER/
    ///   DROP/backfills), applied in filename order, tracked in
    ///   `schema_overrides_applied`.
    /// - `schema_dir/seed/*.sql` — idempotent seed DML (`UPDATE ... UPSERT`),
    ///   run in filename order *after* the schema is confirmed up-to-date.
    ///   Re-run every migrate; idempotency is the SQL's responsibility.
    ///
    /// The whole run is serialized by the advisory schema lock: a concurrent
    /// sync errors out until the other run finishes or its lease expires.
    // The fat error variant (`SyncDirError::Seed` carrying the successful
    // schema report) is deliberate — partial-success reporting is the feature.
    #[allow(clippy::result_large_err)]
    pub async fn sync_dir(
        &self,
        schema_dir: &str,
        drop_strategy: DropStrategy,
    ) -> Result<ApplyReport, SyncDirError> {
        acquire_lock(&self.client, &self.db, &self.holder, LOCK_LEASE_MS)
            .await
            .map_err(SyncDirError::Schema)?;
        let result = self.sync_dir_inner(schema_dir, drop_strategy).await;
        if let Err(e) = release_lock(&self.client, &self.db, &self.holder).await {
            tracing::warn!("could not release the migration lock (expires on its own): {e}");
        }
        result
    }

    #[allow(clippy::result_large_err)]
    async fn sync_dir_inner(
        &self,
        schema_dir: &str,
        drop_strategy: DropStrategy,
    ) -> Result<ApplyReport, SyncDirError> {
        let desired = fs::read_schema_dir(schema_dir).map_err(SyncDirError::ReadSchema)?;
        let overrides = read_overrides_checked(schema_dir).map_err(SyncDirError::ReadOverrides)?;

        // 1. Schema sync (overrides, then the recomputed diff). Override copy
        //    phases run inside explicit transactions that are verified and
        //    committed individually; swap (destructive) statements auto-commit
        //    as they run. Failures are honest: they name the failed statement
        //    and what already committed instead of claiming a rollback.
        let mut report = self
            .sync(desired, &overrides, drop_strategy)
            .await
            .map_err(SyncDirError::Schema)?;

        // The schema report is *complete* at this point — surface it before
        // the seed phase runs, so a seed failure (below) can't hide the fact
        // that the schema migration already succeeded.
        tracing::info!("schema phase complete:\n{}", format!("{report}").trim_end());

        // 2. Seed phase. A failure here returns a [`SyncDirError::Seed`] that
        //    carries the successful schema report.
        let seed_dir = Path::new(schema_dir).join("seed");
        if seed_dir.is_dir() {
            let seeds = fs::read_seeds(&seed_dir).map_err(SyncDirError::ReadSeeds)?;
            let mut applied_seeds = Vec::new();
            for seed in &seeds {
                if let Err(source) = self
                    .client
                    .execute_language(&self.db, "sqlscript", &seed.body)
                    .await
                    .map_err(|e| MigrateError::Server {
                        context: "applying seed script".into(),
                        source: e,
                    })
                {
                    report.applied_seeds = applied_seeds;
                    return Err(SyncDirError::Seed {
                        filename: seed.filename.clone(),
                        source,
                        schema_report: report,
                    });
                }
                applied_seeds.push(seed.filename.clone());
            }
            report.applied_seeds = applied_seeds;
        }

        Ok(report)
    }

    /// The underlying sync, taking already-parsed desired state. Useful for
    /// tests and programmatic callers.
    pub async fn sync(
        &self,
        desired: Schema,
        overrides: &[OverrideFile],
        drop_strategy: DropStrategy,
    ) -> Result<ApplyReport> {
        let plan = self.build_plan(&desired, overrides, drop_strategy).await?;

        // True no-op: nothing to apply, nothing to record.
        if plan.no_op {
            return Ok(ApplyReport {
                no_op: true,
                checksum: plan.checksum,
                applied_statements: vec![],
                warnings: plan.warnings,
                applied_overrides: vec![],
                applied_seeds: vec![],
            });
        }

        let default_exprs = fs::collect_default_exprs(&desired);

        if plan.statements.is_empty() {
            // No batch to execute, so it's safe to record pending overrides here.
            // (An override whose only effect is the checksum change still needs
            // to be recorded, otherwise it shows up as pending forever.)
            for o in &plan.pending_overrides {
                revisions::record_override(
                    &self.client,
                    &self.db,
                    &o.filename,
                    &o.checksum,
                    &o.statements,
                )
                .await?;
            }

            revisions::write_snapshot(
                &self.client,
                &self.db,
                &plan.checksum,
                &plan.actual,
                &default_exprs,
            )
            .await?;
            revisions::record_revision(&self.client, &self.db, &plan.checksum, "[]").await?;
            return Ok(ApplyReport {
                no_op: false,
                checksum: plan.checksum,
                applied_statements: vec![],
                warnings: plan.warnings,
                applied_overrides: plan
                    .pending_overrides
                    .iter()
                    .map(|o| o.filename.clone())
                    .collect(),
                applied_seeds: vec![],
            });
        }

        // Phase 1 — overrides, each applied as an explicit copy phase (begin ->
        // copy statements -> in-transaction verification -> commit | rollback)
        // followed by the swap phase (destructive/irreversible statements, which
        // ArcadeDB auto-commits regardless). This ordering is what makes the
        // rebuild safe: the data copy is durable *and verified* before anything
        // that drops or renames a type runs.
        //
        // Every durable step is journaled (see [`progress`]): the copy-phase
        // marker commits inside the copy transaction; each swap statement is
        // recorded right after it executes. A failed override can therefore be
        // re-run safely — durable work is skipped, not repeated.
        //
        // Baseline mode: a database with no recorded snapshot and no user
        // types is brand new — it is born at the CURRENT schema. Historical
        // overrides migrate pre-existing state a fresh database never had
        // (and their statements typically assume types that only the schema
        // phase creates), so on a fresh database they are recorded as
        // consumed and never executed. Only the schema diff applies.
        let mut applied_overrides = Vec::new();
        let baseline = plan.actual.types.is_empty()
            && revisions::read_snapshot(&self.client, &self.db)
                .await?
                .is_none();
        for o in &plan.pending_overrides {
            if baseline {
                revisions::record_override(
                    &self.client,
                    &self.db,
                    &o.filename,
                    &o.checksum,
                    &o.statements,
                )
                .await?;
                applied_overrides.push(o.filename.clone());
                tracing::info!(
                    override = %o.filename,
                    "baseline: fresh database — override recorded without executing"
                );
                continue;
            }
            self.apply_override(&o.filename, &o.checksum, &o.statements)
                .await?;
            applied_overrides.push(o.filename.clone());
        }

        // Phase 2 — reconcile the *post-override* state. The plan's diff was
        // computed against the pre-override DB; overrides can rename/retype
        // types, so the diff must be recomputed against what the overrides
        // actually left behind (regression: an override that rebuilt a type
        // invalidated the pre-computed diff's `ALTER PROPERTY` on it). The
        // snapshot is unchanged by the override phases (only the final
        // `write_snapshot` rewrites it), so re-reading it gives the same
        // three-way default reconciliation `build_plan` used — now against the
        // post-override state.
        //
        // Override ownership is re-derived AFTER the pending overrides ran:
        // freshly applied files contribute their created types too, so a
        // destructive sync never drops what an override just built.
        let applied_now = revisions::applied_overrides(&self.client, &self.db).await?;
        let owned =
            classification::owned_types(applied_now.values().map(|r| r.statements.as_slice()));
        let post = introspect::fetch_actual(&self.client, &self.db).await?;
        let snap = revisions::read_snapshot(&self.client, &self.db).await?;
        let (diff_stmts, diff_warnings) =
            diff_statements(&desired, &post, snap.as_ref(), drop_strategy, &owned)?;
        let mut warnings = plan.warnings;
        warnings.extend(diff_warnings);

        if !diff_stmts.is_empty() {
            // CREATE TIMESERIES TYPE cannot ride the sqlscript batch: the
            // script's transaction context counts against the TS engine's
            // nested-transaction limit while it initializes one transaction
            // per shard, so any SHARDS > ~1 fails with "Exceeded number of 3
            // nested transactions". Run TS DDL as individual auto-committed
            // commands; everything else stays batched.
            let (ts_ddl, batched): (Vec<_>, Vec<_>) = diff_stmts.iter().cloned().partition(|s| {
                s.trim_start()
                    .to_ascii_uppercase()
                    .starts_with("CREATE TIMESERIES")
            });
            for stmt in &ts_ddl {
                self.client
                    .execute(&self.db, stmt)
                    .await
                    .map_err(|e| MigrateError::Server {
                        context: format!("diff phase failed on `{stmt}`"),
                        source: e,
                    })?;
            }
            if !batched.is_empty() {
                let batch = render_batch(&batched);
                self.client
                    .execute_language(&self.db, "sqlscript", &batch)
                    .await
                    .map_err(|e| MigrateError::Server {
                        context: "diff phase failed".into(),
                        source: e,
                    })?;
            }
        }

        // The snapshot must record the state *after* the diff applied — the
        // `post` fetch above happened before the diff batch ran (an override
        // path may not even have created the diffed types yet). Recording
        // pre-diff state would make the next sync detect false drift forever.
        let final_state = introspect::fetch_actual(&self.client, &self.db).await?;
        revisions::write_snapshot(
            &self.client,
            &self.db,
            &plan.checksum,
            &final_state,
            &default_exprs,
        )
        .await?;
        let actions_json = serde_json::to_string(&diff_stmts).unwrap_or_else(|_| "[]".into());
        revisions::record_revision(&self.client, &self.db, &plan.checksum, &actions_json).await?;

        Ok(ApplyReport {
            no_op: false,
            checksum: plan.checksum,
            applied_statements: diff_stmts,
            warnings,
            applied_overrides,
            applied_seeds: vec![],
        })
    }

    /// Apply ONE override through the phase engine — the shared body used by
    /// both live sync and reviewed-rollout apply:
    ///
    /// 1. read any journaled progress for this filename;
    /// 2. run the copy phase inside an explicit transaction with in-tx
    ///    verification, unless its done-marker shows it already committed;
    /// 3. execute swap statements individually (ArcadeDB auto-commits them),
    ///    journaling each executed index so a re-run resumes at the failure
    ///    point instead of repeating durable work;
    /// 4. record the override as applied and clear the progress rows.
    pub(crate) async fn apply_override(
        &self,
        filename: &str,
        checksum: &str,
        statements: &[String],
    ) -> Result<()> {
        let resume = read_progress(&self.client, &self.db, filename).await?;
        let (copy, swap) = split_override_phases(statements);

        if !copy.is_empty() {
            if resume.copy_done {
                tracing::info!(
                    override = %filename,
                    "copy phase already committed (progress marker) — skipping"
                );
            } else {
                self.apply_copy_phase(filename, &copy).await?;
            }
        }

        // Swap statements are DDL (or destructive DML): ArcadeDB commits
        // them the moment they run, so they cannot be grouped into a
        // rollback-able unit. Execute each individually so a failure reports
        // exactly which statement ran and which didn't.
        let mut executed_swap = resume.swap_done.len();
        for (i, stmt) in swap.iter().enumerate() {
            if resume.swap_done.contains(&(i as i64)) {
                continue;
            }
            self.client
                .execute(&self.db, stmt)
                .await
                .map_err(|e| MigrateError::Server {
                    context: format!(
                        "override {filename}: swap statement #{} failed after {} already executed \
                         (each swap/DDL statement auto-commits, so the copy phase is already \
                         durable and verified — the remaining swap statements were NOT run; \
                         re-running the migration resumes safely at this statement)",
                        i + 1,
                        executed_swap
                    ),
                    source: e,
                })?;
            mark_swap(&self.client, &self.db, filename, i).await?;
            executed_swap += 1;
        }

        // Only after the whole override succeeded is it recorded. The
        // statements are stored too — they drive override-object ownership.
        revisions::record_override(&self.client, &self.db, filename, checksum, statements).await?;
        // Progress rows are superseded by the applied-override record;
        // leftovers would be harmless, but keep the bookkeeping tidy.
        if let Err(e) = clear_progress(&self.client, &self.db, filename).await {
            tracing::warn!(
                override = %filename,
                "applied, but could not clear progress rows (harmless): {e}"
            );
        }
        Ok(())
    }

    /// Run an override's copy phase inside an explicit server-side transaction:
    /// begin -> run the additive statements -> verify each copied type's row
    /// count against its source (in the same transaction) -> mark the copy
    /// done -> commit, or roll back with an honest error.
    ///
    /// The engine contract:
    /// - record DML joins the transaction and rolls back cleanly;
    /// - schema DDL and `MOVE VERTEX` commit as they run, so the
    ///   *verification* — not the rollback — is what protects those copies:
    ///   it runs before any drop/rename can happen.
    ///
    /// The done-marker is written inside this transaction (it's DML, so it
    /// joins it): a committed copy is atomically marked as such, so a re-run
    /// after a partial swap-phase failure skips the copy instead of repeating
    /// it (which would duplicate every row).
    async fn apply_copy_phase(&self, filename: &str, copy: &[String]) -> Result<()> {
        let pairs = classification::copy_verification_pairs(copy);

        let tx = self
            .client
            .begin_transaction(&self.db, TransactionIsolation::ReadCommitted)
            .await
            .map_err(|e| MigrateError::Server {
                context: format!("override {filename}: begin copy transaction"),
                source: e,
            })?;

        // Source counts must be captured *before* the copy runs: a MOVE
        // drains the source, so its count can't be read after the fact.
        let mut source_counts: Vec<(String, String, bool, i64)> = Vec::new();
        for (src, tgt, is_move) in &pairs {
            let n = match self.count_in_transaction(&tx, src).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = self.client.rollback_transaction(&self.db, &tx).await;
                    return Err(wrap_count_error(
                        e,
                        format!(
                            "override {filename}: could not count source type {src} before the \
                             copy phase (rolled back)"
                        ),
                    ));
                }
            };
            source_counts.push((src.clone(), tgt.clone(), *is_move, n));
        }

        for stmt in copy {
            if let Err(e) = self
                .client
                .execute_in_transaction(&self.db, "sql", stmt, &tx)
                .await
            {
                let _ = self.client.rollback_transaction(&self.db, &tx).await;
                return Err(MigrateError::Server {
                    context: format!(
                        "override {filename}: copy statement failed (rolled back; the original \
                         types are untouched)"
                    ),
                    source: e,
                });
            }
        }

        // In-transaction verification: every copied type must hold at least as
        // many rows as its source held before the copy. Roll back (and fail
        // loudly) on any shortfall instead of committing a partial copy.
        for (src, tgt, _, expected) in &source_counts {
            let actual = match self.count_in_transaction(&tx, tgt).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = self.client.rollback_transaction(&self.db, &tx).await;
                    return Err(wrap_count_error(
                        e,
                        format!(
                            "override {filename}: could not verify copy of {tgt} (rolled back)"
                        ),
                    ));
                }
            };
            if actual < *expected {
                let _ = self.client.rollback_transaction(&self.db, &tx).await;
                return Err(MigrateError::Revision {
                    message: format!(
                        "override {filename}: copy verification FAILED for {tgt} (rolled back): \
                         expected at least {expected} rows from {src}, got {actual}. The original \
                         types are intact — fix the override and retry."
                    ),
                });
            }
        }

        // Journal the committed-copy state INSIDE the transaction: the marker
        // commits exactly when the copied rows do.
        let marker = copy_marker_sql(filename);
        if let Err(e) = self
            .client
            .execute_in_transaction(&self.db, "sql", &marker, &tx)
            .await
        {
            let _ = self.client.rollback_transaction(&self.db, &tx).await;
            return Err(MigrateError::Server {
                context: format!(
                    "override {filename}: could not journal the copy phase (rolled back)"
                ),
                source: e,
            });
        }

        self.client
            .commit_transaction(&self.db, &tx)
            .await
            .map_err(|e| MigrateError::Server {
                context: format!("override {filename}: commit copy transaction"),
                source: e,
            })?;
        Ok(())
    }

    /// Count rows of `type_name` inside transaction `tx`.
    async fn count_in_transaction(&self, tx: &str, type_name: &str) -> Result<i64> {
        let res = self
            .client
            .query_in_transaction(
                &self.db,
                &format!("SELECT count(*) AS n FROM {type_name}"),
                tx,
            )
            .await
            .map_err(|e| MigrateError::Server {
                context: format!("count {type_name} in tx"),
                source: e,
            })?;
        let rec = res
            .records
            .first()
            .ok_or_else(|| MigrateError::Introspect {
                message: format!("count of {type_name} returned no record"),
            })?;
        match rec.properties.get("n").and_then(|v| v.kind.clone()) {
            Some(Kind::Int64Value(n)) => Ok(n),
            Some(Kind::Int32Value(n)) => Ok(n as i64),
            Some(Kind::DoubleValue(n)) => Ok(n as i64),
            other => Err(MigrateError::Introspect {
                message: format!("count of {type_name} returned unexpected value kind: {other:?}"),
            }),
        }
    }

    /// Compute the plan for a sync without touching the DB.
    ///
    /// Also enforces the two override-integrity invariants at plan time (so
    /// `--dry-run` catches them before anything runs):
    /// - an override that was **edited after being applied** is a hard error
    ///   (the recorded checksum no longer matches the file);
    /// - an override whose authored order the copy/swap split would reorder
    ///   is rejected ([`validate_phase_ordering`]).
    async fn build_plan(
        &self,
        desired: &Schema,
        overrides: &[OverrideFile],
        drop_strategy: DropStrategy,
    ) -> Result<Plan> {
        let desired_checksum = checksum(desired);

        let snapshot = revisions::read_snapshot(&self.client, &self.db).await?;
        let actual = introspect::fetch_actual(&self.client, &self.db).await?;
        let in_sync = snapshot
            .as_ref()
            .is_some_and(|s| s.checksum == desired_checksum && s.state == actual);

        // Applied-override integrity (immutability + rename guard) — pure
        // logic, unit-tested in `fs::compute_pending`. The registry's stored
        // statements also yield the override-owned types, which survive
        // DropStrategy::Explicit.
        let applied = revisions::applied_overrides(&self.client, &self.db).await?;
        let owned = classification::owned_types(applied.values().map(|r| r.statements.as_slice()));
        let pending = fs::compute_pending(&applied, overrides)?;

        // Authored-order guard: reject files the phase split would silently
        // reorder (additive statement after a destructive one). Baseline-mode
        // databases (no user types, no snapshot) consume every historical
        // override WITHOUT executing it — their statement order is irrelevant,
        // and legacy rebuild recipes legitimately interleave the phases — so
        // only files that will actually run are validated.
        let baseline = actual.types.is_empty() && snapshot.is_none();
        if !baseline {
            for o in &pending {
                validate_phase_ordering(&o.filename, &o.statements)?;
            }
        }

        if in_sync && pending.is_empty() {
            return Ok(Plan {
                no_op: true,
                checksum: desired_checksum,
                statements: vec![],
                pending_overrides: vec![],
                warnings: vec![],
                actual,
            });
        }

        let (diff_stmts, warnings) = if in_sync {
            (Vec::new(), Vec::new())
        } else {
            diff_statements(desired, &actual, snapshot.as_ref(), drop_strategy, &owned)?
        };

        let mut statements: Vec<String> = Vec::new();
        for o in &pending {
            statements.extend(o.statements.iter().cloned());
        }
        statements.extend(diff_stmts);

        Ok(Plan {
            no_op: false,
            checksum: desired_checksum,
            statements,
            pending_overrides: pending,
            warnings,
            actual,
        })
    }

    /// Dry-run: return the statements that *would* be applied (overrides +
    /// diff, in apply order), without touching the DB.
    pub async fn plan(&self, schema_dir: &str, drop_strategy: DropStrategy) -> Result<Vec<String>> {
        let desired = fs::read_schema_dir(schema_dir)?;
        let overrides = read_overrides_checked(schema_dir)?;
        let plan = self.build_plan(&desired, &overrides, drop_strategy).await?;
        Ok(plan.statements)
    }

    /// Dry-run that returns the full plan (overrides + statements), so the
    /// binary can surface override filenames separately from diff DDL and the
    /// rollout writer can record override metadata.
    pub async fn plan_full(&self, schema_dir: &str, drop_strategy: DropStrategy) -> Result<Plan> {
        let desired = fs::read_schema_dir(schema_dir)?;
        let overrides = read_overrides_checked(schema_dir)?;
        self.build_plan(&desired, &overrides, drop_strategy).await
    }

    /// The last recorded desired-schema checksum, or `None` if no sync has run.
    /// Used by the binary to decide whether a rollout file has been applied.
    pub async fn last_recorded_checksum(&self) -> Result<Option<String>> {
        let snap = revisions::read_snapshot(&self.client, &self.db).await?;
        Ok(snap.map(|s| s.checksum))
    }
}

/// Re-attach phase context to a [`Migrator::count_in_transaction`] failure:
/// a server error keeps its typed `source` with the new context, anything
/// else is flattened into a message-carrying error (the count result itself
/// was malformed — the flattened message is the whole story).
fn wrap_count_error(e: MigrateError, context: String) -> MigrateError {
    match e {
        MigrateError::Server { source, .. } => MigrateError::Server { context, source },
        other => MigrateError::Introspect {
            message: format!("{context}: {other}"),
        },
    }
}

/// Reconcile `actual` toward `desired`: map the diff actions to ordered SQL
/// statements plus warnings, applying the migrator's guard rails:
///
/// - Property-retype drift is not auto-able (ArcadeDB cannot retype a property
///   in place) — it is surfaced as a warning, never DDL; a sanctioned rebuild
///   override is the fix.
/// - Type-KIND drift (`DOCUMENT` ↔ `VERTEX` ↔ `EDGE`) has no ALTER form at all
///   and the diff engine doesn't compare kinds for existing types — detected
///   here so it can't stay invisible; like retypes, the fix is a rebuild
///   override.
/// - Destructive actions are gated by `drop_strategy` (they require
///   `DropStrategy::Explicit`, i.e. `--apply-destructive`).
///
/// Pure — unit-testable without a DB.
pub(crate) fn diff_statements(
    desired: &Schema,
    actual: &Schema,
    snapshot: Option<&SchemaSnapshot>,
    drop_strategy: DropStrategy,
    ignored_types: &std::collections::BTreeSet<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut kind_warnings = Vec::new();
    for (name, desired_ty) in &desired.types {
        if let Some(actual_ty) = actual.types.get(name) {
            if desired_ty.kind != actual_ty.kind {
                kind_warnings.push(format!(
                    "type kind drift: {name} is a {} in the DB but declared {} in the \
                     schema — ArcadeDB has no ALTER for the type kind, so this is skipped. \
                     Add an override that rebuilds the type (create a <t>_lng of the right \
                     kind, move/copy the records, drop the old type, then \
                     ALTER TYPE <t>_lng NAME <t>).",
                    actual_ty.kind.ddl_keyword(),
                    desired_ty.kind.ddl_keyword(),
                ));
            }
        }
    }
    // TimeSeries spec drift: the engine has NO ALTER for any part of a TS
    // declaration, so a mismatch between the authored spec and live state is
    // unfixable in place — surface it with rebuild guidance. Only explicitly
    // authored fields are compared: omitted SHARDS/RETENTION are engine- or
    // machine-managed defaults, and comparing those would false-positive on
    // every machine with a different CPU count.
    for (name, desired_ty) in &desired.types {
        let Some(des) = &desired_ty.timeseries else {
            continue;
        };
        let Some(actual_ty) = actual.types.get(name) else {
            continue;
        };
        if actual_ty.kind != TypeKind::Timeseries {
            continue; // kind drift already warned above
        }
        let Some(act) = &actual_ty.timeseries else {
            continue;
        };

        let mut diffs = Vec::new();
        if des.timestamp_column != act.timestamp_column {
            diffs.push(format!(
                "timestamp column {} -> {}",
                act.timestamp_column, des.timestamp_column
            ));
        }
        let key = |cols: &[TimeseriesColumn]| -> std::collections::BTreeSet<String> {
            cols.iter()
                .map(|c| format!("{} {}", c.name, c.data_type))
                .collect()
        };
        for (role, d, a) in [
            ("TAGS", des.tags.as_slice(), act.tags.as_slice()),
            ("FIELDS", des.fields.as_slice(), act.fields.as_slice()),
        ] {
            let (dk, ak) = (key(d), key(a));
            if dk != ak {
                let added: Vec<_> = dk.difference(&ak).cloned().collect();
                let removed: Vec<_> = ak.difference(&dk).cloned().collect();
                diffs.push(format!(
                    "{role} columns changed (removed {removed:?}, added {added:?})"
                ));
            }
        }
        if let Some(shards) = des.shards {
            if Some(u64::from(shards)) != act.shards.map(u64::from) {
                diffs.push(format!(
                    "SHARDS {} -> {}",
                    act.shards.map(|s| s.to_string()).unwrap_or_default(),
                    shards
                ));
            }
        }
        if des.retention.is_some() && des.retention_ms() != act.retention_ms() {
            diffs.push(format!(
                "RETENTION {:?} -> {:?}",
                act.retention, des.retention
            ));
        }
        if des.compaction_interval.is_some() && des.compaction_ms() != act.compaction_ms() {
            diffs.push(format!(
                "COMPACTION_INTERVAL {:?} -> {:?}",
                act.compaction_interval, des.compaction_interval
            ));
        }

        if !diffs.is_empty() {
            kind_warnings.push(format!(
                "timeseries spec drift on {name}: {}. ArcadeDB has no ALTER for \
                 TimeSeries declarations — drop and recreate the type via an \
                 override migration.",
                diffs.join("; ")
            ));
        }
    }

    let actions = diff_with_snapshot(desired, actual, snapshot, drop_strategy, ignored_types);
    actions_to_statements(&actions, actual, drop_strategy, kind_warnings)
}

/// Map diff actions to ordered SQL statements plus warnings — the migrator's
/// guard rails, separated from emission so the Never-strategy guard is
/// directly testable (through public emission paths, destructive actions
/// never coexist with `DropStrategy::Never`; this function is the
/// defence-in-depth backstop if that invariant ever changes). `warnings` are
/// pre-computed warnings (kind drift) carried through.
fn actions_to_statements(
    actions: &[DiffAction],
    actual: &Schema,
    drop_strategy: DropStrategy,
    mut warnings: Vec<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut stmts: Vec<String> = Vec::new();
    for a in actions {
        if let DiffAction::AlterPropertyType { type_name, prop } = a {
            let live = actual
                .types
                .get(type_name)
                .and_then(|t| t.properties.get(&prop.name))
                .map(|p| p.type_name.to_owned())
                .unwrap_or_else(|| "?".to_owned());
            warnings.push(format!(
                "property type drift: {type_name}.{} is {live} in the DB but {} in \
                 the schema — ArcadeDB cannot retype a property in place, so this is \
                 skipped. Add an override that rebuilds the type (create a <t>_lng \
                 copy with the new property type, move/copy the records converting \
                 values with convert.*, drop the old type, then ALTER TYPE <t>_lng NAME <t>).",
                prop.name, prop.type_name
            ));
            continue;
        }
        if a.is_destructive() && !matches!(drop_strategy, DropStrategy::Explicit) {
            return Err(MigrateError::Usage {
                message: format!(
                    "destructive action {} not allowed under DropStrategy::Never \
                     (use an override or --apply-destructive)",
                    a.to_sql()
                ),
            });
        }
        stmts.push(a.to_sql());
    }
    Ok((stmts, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::model::{Property, TypeKind};
    use crate::schema::parser;

    #[test]
    fn kind_drift_is_surfaced_as_a_warning_not_ddl() {
        // F7 regression: the structural diff never compared `kind` for
        // existing types — a DOCUMENT→VERTEX flip applied zero DDL and
        // rewrote the snapshot as if nothing happened.
        let desired = parser::parse("CREATE VERTEX TYPE game IF NOT EXISTS;").unwrap();
        let mut actual = Schema::new();
        actual.type_or_insert("game", TypeKind::Document);

        let (stmts, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(stmts.is_empty(), "kind drift must not emit DDL: {stmts:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("type kind drift")
                && warnings[0].contains("DOCUMENT")
                && warnings[0].contains("VERTEX"),
            "{warnings:?}"
        );
    }

    #[test]
    fn matching_kinds_produce_no_kind_warning() {
        let desired = parser::parse(
            "CREATE VERTEX TYPE game IF NOT EXISTS;
             CREATE PROPERTY game.name IF NOT EXISTS STRING;",
        )
        .unwrap();
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("game", TypeKind::Vertex);
        ty.properties.insert(
            "name".into(),
            Property {
                name: "name".into(),
                type_name: "STRING".into(),
                constraints: Default::default(),
            },
        );
        let (stmts, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(
            stmts.is_empty() && warnings.is_empty(),
            "{stmts:?} {warnings:?}"
        );
    }

    #[test]
    fn property_retype_still_warns_with_rebuild_guidance() {
        let desired = parser::parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS LONG;",
        )
        .unwrap();
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("t", TypeKind::Document);
        ty.properties.insert(
            "x".into(),
            Property {
                name: "x".into(),
                type_name: "STRING".into(),
                constraints: Default::default(),
            },
        );
        let (stmts, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(stmts.is_empty(), "retype is never auto-applied: {stmts:?}");
        assert!(
            warnings.len() == 1 && warnings[0].contains("property type drift"),
            "{warnings:?}"
        );
    }

    #[test]
    fn destructive_actions_bail_under_never() {
        // Through public emission paths, DropType only exists under Explicit —
        // so the Never guard is exercised directly with a synthetic action.
        let actions = [DiffAction::DropType {
            name: "leftover".into(),
            kind: TypeKind::Document,
        }];
        let actual = Schema::new();
        let err = actions_to_statements(&actions, &actual, DropStrategy::Never, Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not allowed under DropStrategy::Never"),
            "{err}"
        );

        // Under Explicit the drop renders.
        let (stmts, _) =
            actions_to_statements(&actions, &actual, DropStrategy::Explicit, Vec::new()).unwrap();
        assert_eq!(stmts, vec!["DROP TYPE leftover IF EXISTS UNSAFE"]);
    }

    #[test]
    fn ignored_types_survive_explicit_drops() {
        // F6: an override-owned leftover type must not be dropped by a
        // destructive sync.
        let desired = Schema::new();
        let mut actual = Schema::new();
        actual.type_or_insert("helper_scratch", TypeKind::Document);
        let owned: std::collections::BTreeSet<String> =
            ["helper_scratch".to_string()].into_iter().collect();

        let (stmts, _) =
            diff_statements(&desired, &actual, None, DropStrategy::Explicit, &owned).unwrap();
        assert!(stmts.is_empty(), "owned type must survive: {stmts:?}");

        // Without ownership the same state yields the DROP.
        let (stmts, _) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Explicit,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(stmts, vec!["DROP TYPE helper_scratch IF EXISTS UNSAFE"]);
    }

    #[test]
    fn owned_types_nets_creates_against_drops() {
        use classification::{created_type_name, dropped_type_name, owned_types};
        let stmts: Vec<String> = [
            "CREATE DOCUMENT TYPE scratch IF NOT EXISTS",
            "CREATE VERTEX TYPE t_lng IF NOT EXISTS",
            "INSERT INTO t_lng FROM SELECT * FROM t", // no type create
            "CREATE VERTEX t_lng SET x = 1",          // record DML, NOT a type create
            "DROP TYPE scratch IF EXISTS",
            "ALTER TYPE t_lng NAME t",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(created_type_name("CREATE VERTEX t SET x = 1"), None);
        assert_eq!(
            created_type_name("create edge type link"),
            Some("link".to_string()),
            "authored case kept"
        );
        assert_eq!(
            dropped_type_name("DROP TYPE scratch IF EXISTS"),
            Some("scratch".to_string())
        );

        let owned = owned_types([stmts.as_slice()].into_iter());
        // The dropped scratch type AND the renamed-away name must not be
        // owned. The NEW name (`t`) may appear — ownership is a conservative
        // superset there, and harmless: the diff never drops a type the
        // desired schema declares, regardless of ownership.
        assert!(!owned.contains("scratch"), "{owned:?}");
        assert!(
            !owned.contains("t_lng"),
            "rename moves ownership: {owned:?}"
        );
    }

    // --- TimeSeries spec drift -------------------------------------------

    use crate::schema::model::{TimeseriesColumn, TimeseriesSpec, TsRole};

    fn ts_spec(retention: Option<&str>) -> TimeseriesSpec {
        TimeseriesSpec {
            timestamp_column: "ts".into(),
            tags: vec![TimeseriesColumn {
                name: "sensor_id".into(),
                data_type: "LONG".into(),
                role: TsRole::Tag,
            }],
            fields: vec![TimeseriesColumn {
                name: "temperature".into(),
                data_type: "INTEGER".into(),
                role: TsRole::Field,
            }],
            shards: Some(2),
            retention: retention.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn timeseries_spec_drift_warns_with_rebuild_guidance() {
        let mut desired = Schema::new();
        let ty = desired.type_or_insert("metrics", TypeKind::Timeseries);
        ty.timeseries = Some(ts_spec(Some("90 DAYS")));

        let mut actual = Schema::new();
        let ty = actual.type_or_insert("metrics", TypeKind::Timeseries);
        ty.timeseries = Some(ts_spec(Some("21 DAYS"))); // drifted

        let (stmts, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(stmts.is_empty(), "no ALTER exists — never DDL: {stmts:?}");
        assert!(
            warnings.len() == 1
                && warnings[0].contains("timeseries spec drift")
                && warnings[0].contains("RETENTION"),
            "{warnings:?}"
        );
    }

    #[test]
    fn timeseries_unauthored_defaults_never_false_positive() {
        // Authored WITHOUT retention/shards: engine- and machine-managed
        // defaults (e.g. retentionMs=21d, shardCount=cpu_count) must NOT
        // trigger perpetual rebuild warnings.
        let mut desired = Schema::new();
        let ty = desired.type_or_insert("metrics", TypeKind::Timeseries);
        ty.timeseries = Some(ts_spec(None));
        ty.timeseries.as_mut().unwrap().shards = None;

        let mut actual = Schema::new();
        let ty = actual.type_or_insert("metrics", TypeKind::Timeseries);
        ty.timeseries = Some(ts_spec(Some("21 DAYS")));
        ty.timeseries.as_mut().unwrap().shards = Some(64);

        let (_, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(warnings.is_empty(), "defaults must settle: {warnings:?}");

        // But an explicitly authored mismatch still warns.
        actual
            .types
            .get_mut("metrics")
            .unwrap()
            .timeseries
            .as_mut()
            .unwrap()
            .timestamp_column
            .push('_');
        let (_, warnings) = diff_statements(
            &desired,
            &actual,
            None,
            DropStrategy::Never,
            &Default::default(),
        )
        .unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("timestamp column")),
            "{warnings:?}"
        );
    }
}
