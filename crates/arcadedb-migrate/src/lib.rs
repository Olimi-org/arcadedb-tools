//! Schema migration manager for ArcadeDB. Declarative sync
//! (diff desired `.sql` schema vs. live `schema:types`, apply reconciliation
//! DDL) + sequential override migrations for renames/backfills the diff can't
//! infer + a seed phase. State recorded in the database.
//!
//! The complete model — directory layout, two-phase apply, the four integrity
//! layers, baseline rules, and the engine realities — lives in
//! one place: `docs/migration-model.md` (repo root). The module docs below
//! cover their own slice and link outward rather than re-explaining.
//!
//! Overrides apply in two phases (copy-in-explicit-transaction with in-tx
//! verification, then journaled destructive/swap statements) because ArcadeDB
//! auto-commits DDL — see [`migrator`] for the model.
//!
//! This crate depends on [`arcadedb_protocol`] for the gRPC wire layer. The
//! `migrate` binary (`cargo run -p arcadedb-migrate --bin arcadedb-migrate`)
//! is a generic entry point any app can invoke with its schema directory.
//!
//! ## Quick start
//!
//! ```no_run
//! # async fn run() -> arcadedb_migrate::Result<()> {
//! use arcadedb_migrate::schema::{Migrator, DropStrategy};
//! use arcadedb_protocol::ArcadeDbClient;
//!
//! let client = ArcadeDbClient::connect("127.0.0.1:50051", "root", "password", "mydb").await?;
//! let migrator = Migrator::new(client, "mydb").await?;
//!
//! // Parse database/schema_arcadedb/*.sql + seed/*.sql, diff against the live
//! // DB, apply.
//! let report = migrator.sync_dir("database/schema_arcadedb", DropStrategy::Never).await?;
//! println!("{report}");
//! # Ok(()) }
//! ```

pub mod cli;
pub mod error;
pub mod migrator;
pub mod schema;

pub use error::{MigrateError, Result};
