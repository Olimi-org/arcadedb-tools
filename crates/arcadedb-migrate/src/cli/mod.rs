//! Generic schema-migration entry point for any app using ArcadeDB.
//!
//! Apps invoke this binary with their schema directory; it reads the DB
//! connection from the standard `ARCADEDB_*` env vars. The schema dir layout:
//!
//! - `<dir>/*.sql` — `CREATE TYPE/PROPERTY/INDEX` DDL (declarative base).
//! - `<dir>/overrides/*.sql` — sequential one-shot migrations.
//! - `<dir>/seed/*.sql` — idempotent seed DML (`UPDATE ... UPSERT`).
//!
//! ## Usage
//!
//! ```sh
//! # Dry-run: show what would change (overrides + diff), no writes.
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb --dry-run
//!
//! # Write a rollout file (reviewed artifact) for review, without applying.
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb --write-rollout rollout.sql
//!
//! # Apply a previously-written rollout file (same phase engine as sync).
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb --apply-rollout rollout.sql
//!
//! # Apply directly (default: DropStrategy::Never — never drops).
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb
//!
//! # Allow destructive drops (objects in DB but not in desired schema).
//! # Interactive y/N confirmation unless --yes is passed (CI).
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb --apply-destructive --yes
//!
//! # (Re)generate overrides/overrides.sum — the directory-integrity manifest.
//! # Sync verifies the manifest whenever it exists.
//! arcadedb-migrate --schema-dir path/to/schema_arcadedb --write-manifest
//! ```
//!
//! ## Env
//!
//! - `ARCADEDB_ADDR` (default `127.0.0.1:50051`)
//! - `ARCADEDB_USER` (default `root`)
//! - `ARCADEDB_PASS` (default `password`)
//! - `ARCADEDB_DB` (required)
//! - `RUST_LOG`

use crate::error::{MigrateError, Result};
use crate::migrator::rollout::render_rollout;
use crate::schema::{DropStrategy, Migrator, SyncDirError};

use args::*;
use db::*;
use output::*;
use rollout::*;

mod args;
mod db;
mod output;
mod rollout;

pub async fn run() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;
    let drop_strategy = if args.apply_destructive {
        DropStrategy::Explicit
    } else {
        DropStrategy::Never
    };

    let client = connect(&args).await?;
    // Self-provision the DB if absent (tolerant — see BUG F note).
    ensure_database(&client, &args.db).await?;

    let migrator = Migrator::new(client.clone(), &args.db).await?;

    match args.mode {
        Mode::DryRun => {
            let plan = migrator.plan_full(&args.schema_dir, drop_strategy).await?;
            print_plan(&plan);
            Ok(())
        }
        Mode::WriteRollout(ref path) => {
            let plan = migrator.plan_full(&args.schema_dir, drop_strategy).await?;
            let path = match path {
                Some(p) => p.clone(),
                None => default_rollout_write_path(&args.schema_dir)?,
            };
            if plan.no_op {
                println!("schema up to date — nothing to roll out");
                return Ok(());
            }
            let body = render_rollout(&plan);
            std::fs::write(&path, &body).map_err(|e| MigrateError::Io {
                path: path.clone().into(),
                source: e,
            })?;
            println!("wrote rollout to {path}");
            println!(
                "review it, then apply with: arcadedb-migrate --schema-dir {} --apply-rollout",
                args.schema_dir
            );
            Ok(())
        }
        Mode::ApplyRollout(ref path) => {
            let (path, body) = match path {
                Some(p) => {
                    let b = std::fs::read_to_string(p).map_err(|e| MigrateError::Io {
                        path: p.clone().into(),
                        source: e,
                    })?;
                    (p.clone(), b)
                }
                None => {
                    // Default: the latest unapplied rollout in `<schema_dir>/rollout/`.
                    let (p, b) = latest_unapplied_rollout(&migrator, &args.schema_dir)
                        .await?
                        .ok_or_else(|| MigrateError::Usage {
                            message: format!(
                                "no unapplied rollout found in {}/rollout/ — run with \
                                 --write-rollout first",
                                args.schema_dir.trim_end_matches('/')
                            ),
                        })?;
                    (p, b)
                }
            };
            println!("applying rollout {path}");
            apply(&migrator, &args.schema_dir, &body, args.yes).await
        }
        Mode::WriteManifest => {
            let n = write_manifest(&args.schema_dir)?;
            let dir = std::path::Path::new(&args.schema_dir).join("overrides");
            println!(
                "wrote {} ({n} override file(s)) — sync now verifies the manifest whenever it exists",
                dir.join("overrides.sum").display()
            );
            Ok(())
        }
        Mode::Apply => {
            // Preview destructive actions and require confirmation before sync.
            if matches!(drop_strategy, DropStrategy::Explicit) && !args.yes {
                let plan = migrator.plan_full(&args.schema_dir, drop_strategy).await?;
                let destructive = plan.destructive_statements();
                if !destructive.is_empty() && !confirm_destructive(&destructive)? {
                    println!("aborted: no changes applied");
                    return Ok(());
                }
            }
            match migrator.sync_dir(&args.schema_dir, drop_strategy).await {
                Ok(report) => {
                    print!("{report}");
                    Ok(())
                }
                Err(SyncDirError::Seed {
                    filename,
                    source,
                    schema_report,
                }) => {
                    // The schema migration succeeded; surface that fact before
                    // the seed error so the operator isn't blind to it.
                    eprintln!("schema migration succeeded:\n{schema_report}");
                    eprintln!();
                    eprintln!("seed {filename} failed: {source}");
                    eprintln!("schema is committed and recorded; re-run to retry the seed.");
                    Err(MigrateError::SyncDir(Box::new(SyncDirError::Seed {
                        filename,
                        source,
                        schema_report,
                    })))
                }
                Err(e) => Err(e.into()),
            }
        }
    }
}
