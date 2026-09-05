//! The schema engine + migration orchestrator for ArcadeDB.
//!
//! This module is the public façade: it declares the engine sub-modules
//! ([`ddl`], [`model`], [`parser`], `diff`, [`introspect`], [`revisions`])
//! and re-exports their public items, plus the [`migrator`](crate::migrator)
//! orchestrator ([`Migrator`], [`ApplyReport`], [`Plan`], [`SyncDirError`]).
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
//! // Parse database/schema_arcadedb/*.sql, diff against the live DB, apply.
//! let report = migrator.sync_dir("database/schema_arcadedb", DropStrategy::Never).await?;
//! println!("{report}");
//! # Ok(()) }
//! ```

// --- Engine sub-modules ---

pub mod ddl;
pub mod diff;
pub mod dto_drift;
pub mod introspect;
pub mod model;
pub mod parser;
pub mod revisions;

// The migration orchestrator lives at the crate root (`crate::migrator`),
// declared in `lib.rs`. Re-exported here for the stable `schema::*` surface.

// --- Engine re-exports (stable public surface) ---

pub use ddl::Tail;
pub use diff::{diff, diff_with_snapshot, DiffAction, DropStrategy};
pub use model::{
    duration_ms, Constraints, DefaultExpr, Index, IndexKind, Property, Schema, SchemaSnapshot,
    TimeseriesColumn, TimeseriesSpec, TsRole, Type, TypeKind,
};
pub use parser::parse;
pub use revisions::checksum;

// --- Migrator re-exports (the orchestrator lives at `crate::migrator`) ---

pub use crate::migrator::fs::{OverrideFile, SeedFile};
pub use crate::migrator::report::{render_batch, ApplyReport, Plan, SyncDirError};
pub use crate::migrator::Migrator;
