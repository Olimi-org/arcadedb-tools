//! Report + plan types surfaced by the migrator to callers (and printed by
//! the `migrate` binary).

use super::classification::is_destructive_statement;
use super::fs::OverrideFile;
use crate::error::MigrateError;
use crate::schema::model::Schema;

/// What a sync run produced — surfaced to the caller (and printed by the
/// `migrate` binary) so operators can see what changed.
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    /// The desired-schema checksum after this run.
    pub checksum: String,
    /// True if nothing needed doing: the desired checksum matched the last
    /// sync **and** the live DB still matches the recorded state snapshot
    /// (i.e. no manual/override drift since we last applied).
    pub no_op: bool,
    /// The DDL statements applied (empty if `no_op`).
    pub applied_statements: Vec<String>,
    /// Drift the engine cannot auto-fix, surfaced as warnings (e.g.
    /// property retypes, which have no in-place engine form).
    pub warnings: Vec<String>,
    /// Override migrations applied this run (filenames).
    pub applied_overrides: Vec<String>,
    /// Seed files applied this run (filenames). Seeds re-run every sync;
    /// idempotency comes from the seed SQL itself (`UPDATE ... UPSERT`).
    pub applied_seeds: Vec<String>,
}

impl std::fmt::Display for ApplyReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.no_op && self.applied_overrides.is_empty() && self.applied_seeds.is_empty() {
            return writeln!(f, "schema up to date (checksum {})", short(&self.checksum));
        }
        writeln!(
            f,
            "applied {} statement(s); checksum {}",
            self.applied_statements.len(),
            short(&self.checksum)
        )?;
        for s in &self.applied_statements {
            writeln!(f, "  + {s}")?;
        }
        for o in &self.applied_overrides {
            writeln!(f, "  ~ override {o}")?;
        }
        for w in &self.warnings {
            writeln!(f, "  ! {w}")?;
        }
        for s in &self.applied_seeds {
            writeln!(f, "  * seed {s}")?;
        }
        Ok(())
    }
}

/// Errors from [`super::Migrator::sync_dir`]. The seed-phase failure carries
/// the successful schema `ApplyReport` so the caller can surface partial
/// success (schema migrated, seed X failed) instead of hiding it.
#[derive(Debug, thiserror::Error)]
pub enum SyncDirError {
    /// Reading/parsing the schema `*.sql` files failed.
    #[error("reading schema files: {0}")]
    ReadSchema(#[source] MigrateError),
    /// Reading the `overrides/*.sql` files failed.
    #[error("reading overrides: {0}")]
    ReadOverrides(#[source] MigrateError),
    /// Reading the `seed/*.sql` files failed.
    #[error("reading seeds: {0}")]
    ReadSeeds(#[source] MigrateError),
    /// The schema migration (diff + overrides batch) failed. Nothing committed.
    #[error("schema migration failed: {0}")]
    Schema(#[source] MigrateError),
    /// A seed file failed to apply — the schema migration already succeeded
    /// and committed; `schema_report` records what ran.
    #[error("seed {filename} failed (schema migration already succeeded): {source}")]
    Seed {
        /// The seed file that failed.
        filename: String,
        /// Why it failed.
        #[source]
        source: MigrateError,
        /// What the schema phase already applied (partial success).
        schema_report: ApplyReport,
    },
}

/// The computed plan for a sync run — what [`sync`](super::Migrator::sync)
/// will apply, or what a preview produces without applying. Overrides run
/// first (so the diff sees post-override state); `statements` is the flat
/// override++diff listing for preview/rollout purposes.
#[derive(Debug, Clone)]
pub struct Plan {
    /// True when there's nothing to do: the desired checksum matches the last
    /// sync AND the live DB matches the snapshot AND no overrides are pending.
    pub no_op: bool,
    /// The desired-schema checksum.
    pub checksum: String,
    /// The ordered DDL statements that would run (overrides ++ diff).
    pub statements: Vec<String>,
    /// Overrides that would be recorded as applied (filename + checksum). On
    /// apply, these are recorded regardless of whether `statements` is empty —
    /// a comment-only override still counts as run.
    pub pending_overrides: Vec<OverrideFile>,
    /// Drift the engine cannot auto-fix, surfaced as warnings.
    pub warnings: Vec<String>,
    /// The introspected live state at plan time (used by `sync` to write the
    /// pre-apply snapshot in the empty-batch path).
    pub actual: Schema,
}

impl Plan {
    /// The destructive statements in this plan, per the
    /// [`classification`](super::classification) classifier — surfaced so
    /// the binary can require explicit confirmation before applying.
    pub fn destructive_statements(&self) -> Vec<String> {
        self.statements
            .iter()
            .filter(|s| is_destructive_statement(s))
            .cloned()
            .collect()
    }
}

/// Render an ordered statement list as a `BEGIN; … COMMIT;`-wrapped
/// `sqlscript` batch.
///
/// Note: in ArcadeDB this wrapper is best-effort, not atomic — DDL auto-commits
/// as it runs, so only the DML joins the transaction. It's used for the diff /
/// seed / rollout paths; the override copy phase instead uses the explicit
/// gRPC transaction methods (see `Migrator::apply_copy_phase`).
pub fn render_batch(statements: &[String]) -> String {
    format!(
        "BEGIN;\n{}\nCOMMIT;",
        statements
            .iter()
            .map(|s| format!("  {s};"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Truncate a checksum string to 12 chars for display.
pub(super) fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}
