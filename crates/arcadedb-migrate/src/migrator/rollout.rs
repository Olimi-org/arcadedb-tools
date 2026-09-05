//! Reviewed rollouts: the write/parse/apply engine for migration review
//! artifacts.
//!
//! A rollout is generated from a [`Plan`] (`--write-rollout`), reviewed by a
//! human, then applied **exactly as reviewed** via
//! [`Migrator::apply_rollout`]. The v2 format carries per-override statement
//! sections so the apply path runs them through the SAME phase engine as
//! live sync (copy transaction with in-tx verification, journaled swaps).
//!
//! ```text
//! -- arcadedb-migrate rollout
//! -- checksum: <desired-schema checksum>
//! -- override: 001_rebuild.sql (<file checksum>)
//! -- override-begin: 001_rebuild.sql
//! CREATE DOCUMENT TYPE t_lng IF NOT EXISTS;
//! INSERT INTO t_lng FROM SELECT * FROM t;
//! -- override-end
//! -- diff-begin
//! CREATE PROPERTY t.new_col IF NOT EXISTS STRING;
//! -- diff-end
//! ```
//!
//! Deliberate semantics (documented divergence from live sync):
//! - **Baseline does not apply.** A rollout executes what it contains, even
//!   on a fresh database — the review is the authority. Prefer direct sync on
//!   fresh databases so baseline rules decide.
//! - **Seeds are not run.** They are idempotent and re-run on every sync; the
//!   next direct sync picks them up.
//! - Destructive-statement confirmation is the caller's (CLI's) job, done on
//!   the full parsed statement list before [`Migrator::apply_rollout`] runs.

use std::collections::BTreeMap;

use super::report::{render_batch, ApplyReport, Plan};
use super::Migrator;
use crate::error::{MigrateError, Result};
use crate::schema::ddl::split_sql_statements;
use crate::schema::introspect;
use crate::schema::revisions::{self, acquire_lock, release_lock, LOCK_LEASE_MS};

/// One override's reviewed content inside a rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideSection {
    pub filename: String,
    pub checksum: String,
    pub statements: Vec<String>,
}

/// A parsed rollout artifact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rollout {
    /// The desired-schema checksum this rollout was generated from (`None`
    /// for hand-written files — recorded as `"manual-rollout"`).
    pub checksum: Option<String>,
    /// Overrides to execute (in file order), through the phase engine.
    pub overrides: Vec<OverrideSection>,
    /// Declarative-diff statements, executed as one sqlscript batch after the
    /// overrides (mirroring live-sync ordering).
    pub diff: Vec<String>,
}

/// Render a [`Plan`] as a v2 rollout artifact: metadata header, one statement
/// section per pending override, then the declarative diff.
pub fn render_rollout(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str("-- arcadedb-migrate rollout\n");
    out.push_str(&format!("-- checksum: {}\n", plan.checksum));
    for o in &plan.pending_overrides {
        out.push_str(&format!("-- override: {} ({})\n", o.filename, o.checksum));
    }

    // `plan.statements` is overrides-then-diff; walk it attributing each
    // statement to its section (same contract as the dry-run printer).
    let mut idx = 0usize;
    for o in &plan.pending_overrides {
        out.push_str(&format!("\n-- override-begin: {}\n", o.filename));
        for _ in 0..o.statements.len() {
            if idx < plan.statements.len() {
                out.push_str(&format!("{};\n", plan.statements[idx]));
                idx += 1;
            }
        }
        out.push_str("-- override-end\n");
    }
    out.push_str("\n-- diff-begin\n");
    while idx < plan.statements.len() {
        out.push_str(&format!("{};\n", plan.statements[idx]));
        idx += 1;
    }
    out.push_str("-- diff-end\n");
    out
}

/// Parse a v2 rollout artifact. Rejects the legacy single-batch format (no
/// section markers) with instructions to regenerate — applying it could not
/// preserve phase safety anyway.
pub fn parse_rollout(body: &str) -> Result<Rollout> {
    if !body.contains("-- diff-begin") {
        return Err(MigrateError::Parse {
            message: "legacy rollout format (a single BEGIN/COMMIT batch without per-override \
                 sections) cannot be applied safely — regenerate it with \
                 `arcadedb-migrate --write-rollout`"
                .into(),
        });
    }

    enum Section {
        None,
        Override(String),
        Diff,
    }

    let mut rollout = Rollout::default();
    let mut header_overrides: Vec<(String, String)> = Vec::new();
    let mut current = Section::None;
    let mut buf = String::new();

    macro_rules! flush {
        () => {
            match std::mem::replace(&mut current, Section::None) {
                Section::None => {}
                Section::Diff => {
                    rollout.diff.extend(
                        split_sql_statements(&buf)
                            .into_iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
                Section::Override(name) => {
                    let statements = split_sql_statements(&buf)
                        .into_iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    rollout.overrides.push(OverrideSection {
                        filename: name,
                        // Checksum filled from the header below (the begin
                        // marker intentionally doesn't repeat it).
                        checksum: String::new(),
                        statements,
                    });
                }
            }
            buf.clear();
        };
    }

    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("-- override-begin:") {
            flush!();
            current = Section::Override(name.trim().to_string());
        } else if trimmed == "-- override-end" || trimmed == "-- diff-begin" {
            flush!();
            if trimmed == "-- diff-begin" {
                current = Section::Diff;
            }
        } else if trimmed == "-- diff-end" {
            flush!();
        } else {
            match &current {
                Section::None => {
                    if let Some(rest) = trimmed.strip_prefix("-- checksum:") {
                        rollout.checksum = Some(rest.trim().to_string());
                    } else if let Some(rest) = trimmed.strip_prefix("-- override:") {
                        // Form: "<filename> (<checksum>)"
                        if let Some((name, hash)) = rest.split_once(" (") {
                            header_overrides.push((
                                name.trim().to_string(),
                                hash.trim_end_matches(')').trim().to_string(),
                            ));
                        }
                    } else if !trimmed.is_empty() && !trimmed.starts_with("--") {
                        return Err(MigrateError::Parse {
                            message: format!(
                                "rollout: statement outside a section (expected --override-begin \
                                 or --diff-begin first): {trimmed:?}"
                            ),
                        });
                    }
                }
                Section::Override(_) | Section::Diff => {
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
    }
    flush!();

    // Fill section checksums from the header (by filename), and append
    // header-listed overrides that have no section (e.g. comment-only files)
    // as empty sections — they must still be recorded as consumed.
    for (name, checksum) in &header_overrides {
        if let Some(section) = rollout.overrides.iter_mut().find(|s| &s.filename == name) {
            section.checksum = checksum.clone();
        } else {
            rollout.overrides.push(OverrideSection {
                filename: name.clone(),
                checksum: checksum.clone(),
                statements: Vec::new(),
            });
        }
    }

    // Duplicate sections would collide with the UNIQUE filename index in
    // schema_overrides_applied mid-apply — reject up front.
    let mut seen = std::collections::BTreeSet::new();
    for s in &rollout.overrides {
        if !seen.insert(&s.filename) {
            return Err(MigrateError::Parse {
                message: format!("rollout lists override `{}` more than once", s.filename),
            });
        }
    }

    Ok(rollout)
}

impl Migrator {
    /// Apply a reviewed rollout exactly as parsed: each override section
    /// through the shared phase engine (`apply_override` — copy
    /// transaction, in-tx verification, journaled swaps, resume-safe), then
    /// the diff batch, then bookkeeping (snapshot WITH the desired
    /// `default_exprs`, revision row).
    ///
    /// `default_exprs` is drawn from the desired schema by the caller (CLI:
    /// parse the schema dir) — recording it keeps three-way DEFAULT
    /// reconciliation working after a rollout, instead of wiping it and
    /// re-firing every default on the next sync.
    ///
    /// Like [`sync`](super::Migrator::sync_dir), the run holds the advisory
    /// schema lock: concurrent applies error out instead of interleaving.
    ///
    /// Not atomic (ArcadeDB auto-commits DDL); a failure leaves durable work
    /// journaled, so re-applying the same rollout resumes safely.
    pub async fn apply_rollout(
        &self,
        rollout: &Rollout,
        default_exprs: &BTreeMap<String, String>,
    ) -> Result<ApplyReport> {
        acquire_lock(&self.client, &self.db, &self.holder, LOCK_LEASE_MS).await?;
        let result = self.apply_rollout_inner(rollout, default_exprs).await;
        if let Err(e) = release_lock(&self.client, &self.db, &self.holder).await {
            tracing::warn!("could not release the migration lock (expires on its own): {e}");
        }
        result
    }

    async fn apply_rollout_inner(
        &self,
        rollout: &Rollout,
        default_exprs: &BTreeMap<String, String>,
    ) -> Result<ApplyReport> {
        let mut applied_overrides = Vec::new();
        for o in &rollout.overrides {
            self.apply_override(&o.filename, &o.checksum, &o.statements)
                .await?;
            applied_overrides.push(o.filename.clone());
        }

        if !rollout.diff.is_empty() {
            let batch = render_batch(&rollout.diff);
            self.client
                .execute_language(&self.db, "sqlscript", &batch)
                .await
                .map_err(|e| MigrateError::Server {
                    context: "rollout diff phase failed (DDL auto-commits — statements before the \
                         failure already ran; re-applying resumes past the journaled overrides)"
                        .into(),
                    source: e,
                })?;
        }

        // Bookkeeping: post-apply introspected state + the desired default
        // expressions + a revision row. Writing the snapshot (not just the
        // revision) lets a subsequent run recognize the rollout as applied.
        let final_state = introspect::fetch_actual(&self.client, &self.db).await?;
        let cs = rollout
            .checksum
            .clone()
            .unwrap_or_else(|| "manual-rollout".to_string());
        revisions::write_snapshot(&self.client, &self.db, &cs, &final_state, default_exprs).await?;
        let mut actions: Vec<String> = rollout
            .overrides
            .iter()
            .flat_map(|o| o.statements.iter().cloned())
            .collect();
        actions.extend(rollout.diff.iter().cloned());
        let actions_json = serde_json::to_string(&actions).unwrap_or_else(|_| "[]".into());
        revisions::record_revision(&self.client, &self.db, &cs, &actions_json).await?;

        Ok(ApplyReport {
            no_op: false,
            checksum: cs,
            applied_statements: rollout.diff.clone(),
            warnings: Vec::new(),
            applied_overrides,
            applied_seeds: vec![],
        })
    }
}

/// Best-effort checksum scan for "is this rollout already applied?" checks —
/// tolerant of legacy/hand-edited files, unlike [`parse_rollout`].
pub(crate) fn peek_rollout_checksum(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        line.trim()
            .strip_prefix("-- checksum:")
            .map(str::trim)
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrator::fs::OverrideFile;
    use crate::schema::model::Schema;

    fn plan_with(overrides: &[(&str, &[&str])], diff: &[&str]) -> Plan {
        let mut statements = Vec::new();
        let pending: Vec<OverrideFile> = overrides
            .iter()
            .map(|(name, stmts)| {
                let owned: Vec<String> = stmts.iter().map(|s| s.to_string()).collect();
                statements.extend(owned.iter().cloned());
                OverrideFile {
                    filename: name.to_string(),
                    checksum: format!("cs-{name}"),
                    statements: owned,
                }
            })
            .collect();
        statements.extend(diff.iter().map(|s| s.to_string()));
        Plan {
            no_op: false,
            checksum: "deadbeef1234".into(),
            statements,
            pending_overrides: pending,
            warnings: vec![],
            actual: Schema::new(),
        }
    }

    #[test]
    fn render_parse_roundtrips_attribution() {
        let plan = plan_with(
            &[
                (
                    "001_copy.sql",
                    &[
                        "CREATE DOCUMENT TYPE b",
                        "INSERT INTO b FROM SELECT * FROM a",
                    ],
                ),
                ("002_drop.sql", &["DROP TYPE a"]),
            ],
            &["CREATE PROPERTY a.x IF NOT EXISTS STRING"],
        );

        let rendered = render_rollout(&plan);
        assert!(rendered.contains("-- checksum: deadbeef1234"));
        assert!(rendered.contains("-- override: 001_copy.sql (cs-001_copy.sql)"));
        assert!(rendered.contains("-- override-begin: 002_drop.sql"));

        let parsed = parse_rollout(&rendered).expect("v2 parses");
        assert_eq!(parsed.checksum.as_deref(), Some("deadbeef1234"));
        assert_eq!(parsed.overrides.len(), 2);
        assert_eq!(parsed.overrides[0].filename, "001_copy.sql");
        assert_eq!(
            parsed.overrides[0].statements,
            vec![
                "CREATE DOCUMENT TYPE b",
                "INSERT INTO b FROM SELECT * FROM a",
            ]
        );
        assert_eq!(parsed.overrides[1].filename, "002_drop.sql");
        assert_eq!(parsed.overrides[1].statements, vec!["DROP TYPE a"]);
        assert_eq!(
            parsed.diff,
            vec!["CREATE PROPERTY a.x IF NOT EXISTS STRING"]
        );
    }

    #[test]
    fn semicolons_inside_strings_survive_the_roundtrip() {
        let plan = plan_with(&[], &["INSERT INTO t SET x = 'a;b'"]);
        let parsed = parse_rollout(&render_rollout(&plan)).expect("parses");
        assert_eq!(parsed.diff, vec!["INSERT INTO t SET x = 'a;b'"]);
    }

    #[test]
    fn legacy_single_batch_format_is_rejected() {
        let legacy =
            "-- arcadedb-migrate rollout\n-- checksum: abc\n\nBEGIN;\n  CREATE TYPE x;\nCOMMIT;\n";
        let err = parse_rollout(legacy).unwrap_err().to_string();
        assert!(err.contains("legacy rollout format"), "{err}");
        assert!(err.contains("--write-rollout"), "{err}");
    }

    #[test]
    fn statement_outside_a_section_is_rejected() {
        let body = "-- checksum: abc\nCREATE TYPE sneaky;\n-- diff-begin\n-- diff-end\n";
        let err = parse_rollout(body).unwrap_err().to_string();
        assert!(err.contains("outside a section"), "{err}");
    }

    #[test]
    fn header_override_without_section_records_as_consumed() {
        let body = "\
-- arcadedb-migrate rollout
-- checksum: abc
-- override: 000_comment_only.sql (cs0)

-- diff-begin
-- diff-end
";
        let parsed = parse_rollout(body).expect("parses");
        assert_eq!(parsed.overrides.len(), 1);
        assert_eq!(parsed.overrides[0].filename, "000_comment_only.sql");
        assert_eq!(parsed.overrides[0].checksum, "cs0");
        assert!(parsed.overrides[0].statements.is_empty());
    }

    #[test]
    fn duplicate_sections_are_rejected() {
        let body = "\
-- checksum: abc
-- override-begin: 001_x.sql
CREATE TYPE a;
-- override-end
-- override-begin: 001_x.sql
DROP TYPE a;
-- override-end
-- diff-begin
-- diff-end
";
        let err = parse_rollout(body).unwrap_err().to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn peek_finds_checksum_in_any_format() {
        assert_eq!(
            peek_rollout_checksum("-- x\n-- checksum: abc123\n"),
            Some("abc123".to_string())
        );
        assert_eq!(peek_rollout_checksum("-- nothing\n"), None);
    }
}
