use crate::error::{MigrateError, Result};
use crate::migrator::classification::is_destructive_statement;
use crate::migrator::fs::{collect_default_exprs, read_schema_dir};
use crate::migrator::manifest;
use crate::migrator::rollout::{parse_rollout, peek_rollout_checksum};
use crate::schema::Migrator;

// ---------------------------------------------------------------------------
// Rollout (write / apply) + manifest — default location is `<schema_dir>/rollout/`
// ---------------------------------------------------------------------------

/// The default rollout directory for a schema dir.
pub(super) fn rollout_dir(schema_dir: &str) -> std::path::PathBuf {
    std::path::Path::new(schema_dir).join("rollout")
}

/// Build the default write path: `<schema_dir>/rollout/<timestamp>.sql`, where
/// the timestamp is UTC second-precision (zero-padded, filename-safe). Creates
/// the rollout dir if absent. (Second precision: two rollouts written within
/// the same minute must not overwrite each other.)
pub(super) fn default_rollout_write_path(schema_dir: &str) -> Result<String> {
    let dir = rollout_dir(schema_dir);
    std::fs::create_dir_all(&dir).map_err(|e| MigrateError::Io {
        path: dir.clone(),
        source: e,
    })?;
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H%M%S");
    let path = dir.join(format!("{ts}.sql"));
    Ok(path.to_string_lossy().into_owned())
}

/// Find the latest (highest-named) rollout file in `<schema_dir>/rollout/` that
/// hasn't been applied yet. A rollout is "applied" when its checksum matches
/// the last recorded revision checksum. Returns the path + body of the newest
/// unapplied one.
pub(super) async fn latest_unapplied_rollout(
    migrator: &Migrator,
    schema_dir: &str,
) -> Result<Option<(String, String)>> {
    let dir = rollout_dir(schema_dir);
    let mut files: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(it) => it.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(None), // no rollout dir → nothing to apply
    };
    // Sort descending by filename (timestamped → newest first).
    files.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    let last = migrator.last_recorded_checksum().await.unwrap_or(None);
    for entry in files {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let already_applied = peek_rollout_checksum(&body)
            .as_deref()
            .is_some_and(|c| Some(c) == last.as_deref());
        if !already_applied {
            let p = path.to_string_lossy().into_owned();
            return Ok(Some((p, body)));
        }
    }
    Ok(None)
}

/// Apply a rollout body through the library engine:
///
/// 1. parse (v2 format; legacy single-batch files are rejected with a
///    regenerate hint);
/// 2. destructive-statement gate on the FULL statement list (overrides +
///    diff) — interactive y/N unless `--yes`;
/// 3. [`Migrator::apply_rollout`] with `default_exprs` drawn from the desired
///    schema, so three-way DEFAULT reconciliation keeps working afterwards.
pub(super) async fn apply(
    migrator: &Migrator,
    schema_dir: &str,
    body: &str,
    yes: bool,
) -> Result<()> {
    let rollout = parse_rollout(body).map_err(|e| MigrateError::Parse {
        message: format!("parsing rollout: {e}"),
    })?;

    let all_statements: Vec<String> = rollout
        .overrides
        .iter()
        .flat_map(|o| o.statements.iter().cloned())
        .chain(rollout.diff.iter().cloned())
        .collect();
    let destructive: Vec<String> = all_statements
        .iter()
        .filter(|s| is_destructive_statement(s))
        .cloned()
        .collect();
    if !destructive.is_empty() && !yes {
        // Reviewed or not, destructive statements get an explicit gate —
        // same classifier the live apply path uses.
        if !super::output::confirm_destructive(&destructive)? {
            println!("aborted: no changes applied");
            return Ok(());
        }
    }

    let desired = read_schema_dir(schema_dir)?;
    let default_exprs = collect_default_exprs(&desired);
    let report = migrator.apply_rollout(&rollout, &default_exprs).await?;

    println!(
        "applied rollout: {} override(s), {} diff statement(s)",
        report.applied_overrides.len(),
        report.applied_statements.len()
    );
    for o in &report.applied_overrides {
        println!("  ~ override {o}");
    }
    Ok(())
}

/// Regenerate `<schema_dir>/overrides/overrides.sum`. Returns the listed count.
pub(super) fn write_manifest(schema_dir: &str) -> Result<usize> {
    let dir = std::path::Path::new(schema_dir).join("overrides");
    if !dir.is_dir() {
        return Err(MigrateError::Usage {
            message: format!(
                "no overrides directory at {} — nothing to manifest",
                dir.display()
            ),
        });
    }
    manifest::write_manifest(&dir)
}
