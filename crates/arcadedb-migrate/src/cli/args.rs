use crate::error::{MigrateError, Result};

/// Wrap a message as the [`MigrateError::Usage`] variant — argument/env
/// problems are the only errors argument parsing produces.
fn usage_error(message: String) -> MigrateError {
    MigrateError::Usage { message }
}

// ---------------------------------------------------------------------------
// Modes / arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(super) enum Mode {
    Apply,
    DryRun,
    /// Write the rollout. `None` = default location (`<schema_dir>/rollout/<timestamp>.sql`).
    WriteRollout(Option<String>),
    /// Apply a rollout. `None` = latest unapplied file in the default location.
    ApplyRollout(Option<String>),
    /// (Re)generate `<schema_dir>/overrides/overrides.sum`.
    WriteManifest,
}

#[derive(Debug)]
pub(super) struct Args {
    pub schema_dir: String,
    pub db: String,
    pub mode: Mode,
    pub apply_destructive: bool,
    pub yes: bool,
}

pub(super) fn parse_args() -> Result<Args> {
    let mut schema_dir: Option<String> = None;
    let mut mode: Option<Mode> = None;
    let mut apply_destructive = false;
    let mut yes = false;

    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--dry-run" => mode = Some(Mode::DryRun),
            "--apply-destructive" => apply_destructive = true,
            "--yes" => yes = true,
            "--write-manifest" => mode = Some(Mode::WriteManifest),
            "--schema-dir" => {
                let val = it
                    .next()
                    .ok_or_else(|| usage_error("--schema-dir requires a value".into()))?;
                if val.starts_with("--") {
                    return Err(usage_error(format!(
                        "--schema-dir expects a path, got flag-like value {val:?}"
                    )));
                }
                schema_dir = Some(val);
            }
            "--write-rollout" => {
                // Optional path: if the next token is absent or flag-like,
                // default to `<schema_dir>/rollout/<timestamp>.sql`.
                let val = it.peek().and_then(|p| {
                    if p.starts_with("--") {
                        None
                    } else {
                        Some(p.clone())
                    }
                });
                if val.is_some() {
                    it.next();
                }
                mode = Some(Mode::WriteRollout(val));
            }
            "--apply-rollout" => {
                // Optional path: if absent or flag-like, default to the latest
                // unapplied rollout in `<schema_dir>/rollout/`.
                let val = it.peek().and_then(|p| {
                    if p.starts_with("--") {
                        None
                    } else {
                        Some(p.clone())
                    }
                });
                if val.is_some() {
                    it.next();
                }
                mode = Some(Mode::ApplyRollout(val));
            }
            other if other.starts_with("--") => {
                return Err(usage_error(format!("unknown flag {other:?} (see --help)")));
            }
            other => {
                // Positional: treat as schema-dir if not set, else error.
                if schema_dir.is_none() {
                    schema_dir = Some(other.to_string());
                } else {
                    return Err(usage_error(format!(
                        "unexpected positional argument {other:?}"
                    )));
                }
            }
        }
    }

    let schema_dir = schema_dir.ok_or_else(|| {
        print_usage_to_stderr();
        usage_error("missing --schema-dir <path>".into())
    })?;

    // Mode precedence: --write-rollout / --apply-rollout / --dry-run / apply.
    // Only one of these is sensible; when several are passed, the LAST one
    // parsed wins (each assignment overwrites the previous mode).
    let mode = mode.unwrap_or(Mode::Apply);

    let db = std::env::var("ARCADEDB_DB")
        .map_err(|_| usage_error("ARCADEDB_DB must be set (the target database name)".into()))?;

    Ok(Args {
        schema_dir,
        db,
        mode,
        apply_destructive,
        yes,
    })
}

pub(super) fn print_usage() {
    println!("arcadedb-migrate — ArcadeDB schema migration manager");
    println!();
    println!("USAGE:");
    println!("  arcadedb-migrate --schema-dir <dir> [MODE] [OPTIONS]");
    println!();
    println!("MODES (mutually exclusive; default is apply):");
    println!("  --dry-run                  Print the plan (overrides + diff), no writes");
    println!("  --write-rollout <path>     Write the reviewed rollout artifact, no apply");
    println!("  --apply-rollout <path>     Apply a previously-written rollout file");
    println!(
        "  --write-manifest           Regenerate overrides/overrides.sum (sync verifies it when present)"
    );
    println!();
    println!("OPTIONS:");
    println!(
        "  --apply-destructive        Allow drops/type-changes (interactive confirm, see --yes)"
    );
    println!("  --yes                      Skip the interactive confirmation (for CI)");
    println!("  -h, --help                 Show this message");
    println!();
    println!("ENV:");
    println!("  ARCADEDB_ADDR              (default 127.0.0.1:50051)");
    println!("  ARCADEDB_USER              (default root)");
    println!("  ARCADEDB_PASS              (default password)");
    println!("  ARCADEDB_DB                (required)");
    println!("  RUST_LOG                   (tracing filter)");
}

pub(super) fn print_usage_to_stderr() {
    eprintln!("arcadedb-migrate — see --help for usage");
}
