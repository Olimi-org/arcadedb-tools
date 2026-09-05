//! The crate-facing error type: every fallible [`schema`](crate::schema) /
//! [`migrator`](crate::migrator) / [`cli`](crate::cli) call returns
//! [`Result<T, MigrateError>`](Result) instead of an opaque error.
//! Matching on the failure category distinguishes I/O, parse, server, lock,
//! and integrity errors, and the typed [`ArcadeDbError`] source of any
//! server-side failure stays inspectable.

use std::path::PathBuf;

use arcadedb_protocol::ArcadeDbError;

use crate::migrator::SyncDirError;

/// The error type for the migration engine.
///
/// Variants are grouped by failure category; see each variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    /// Filesystem I/O on the schema directory.
    #[error("{path}: {source}")]
    Io {
        /// The path being read/written/scanned.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A schema `.sql` statement, rollout artifact, or manifest line failed to
    /// parse. The message embeds the offending text.
    #[error("{message}")]
    Parse {
        /// What was rejected and why (includes the statement/line).
        message: String,
    },

    /// JSON (de)serialization of bookkeeping state (snapshot, progress).
    #[error("{context}: {source}")]
    Serde {
        /// What was being (de)serialized.
        context: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// A server call failed. Every client interaction goes through this —
    /// `context` preserves the operational detail (which phase, which
    /// statement) the message carried before the typed-error refactor.
    #[error("{context}: {source}")]
    Server {
        /// What the migrator was doing when the call failed.
        context: String,
        /// The typed client error (transport status, server rejection, …).
        #[source]
        source: ArcadeDbError,
    },

    /// A `schema:types` introspection row could not be parsed.
    #[error("{message}")]
    Introspect {
        /// What was rejected and why (includes the row).
        message: String,
    },

    /// Migration bookkeeping state is malformed (progress rows, snapshot).
    #[error("{message}")]
    Revision {
        /// What is wrong with the state.
        message: String,
    },

    /// Override integrity violation: an applied override was edited or
    /// renamed, or the `overrides.sum` manifest doesn't match the directory.
    #[error("{message}")]
    Integrity {
        /// The violation and how to resolve it.
        message: String,
    },

    /// The advisory migration lock is held by another run, or a claim lost
    /// the race to a concurrent claimant.
    #[error("{message}")]
    Lock {
        /// The lock holder / expiry details and what to do.
        message: String,
    },

    /// A DTO ↔ schema drift assertion failed (`assert_dto_wires` /
    /// `assert_dto_kinds`).
    #[error("{message}")]
    Drift {
        /// The missing columns / kind mismatches.
        message: String,
    },

    /// A [`Migrator::sync_dir`](crate::migrator::Migrator::sync_dir) run
    /// failed. Transparent so the phase-specific display (including the
    /// seed-phase partial-success note) survives. Boxed because
    /// [`SyncDirError`] itself wraps [`MigrateError`] for its other variants.
    #[error(transparent)]
    SyncDir(Box<SyncDirError>),

    /// Caller/operator input was rejected: CLI arguments, required
    /// environment variables, or a destructive action attempted under
    /// `DropStrategy::Never`.
    #[error("{message}")]
    Usage {
        /// What was rejected and why.
        message: String,
    },
}

/// The crate-wide result alias: every fallible migration call returns it.
pub type Result<T, E = MigrateError> = std::result::Result<T, E>;

/// Bare propagation for client errors: [`MigrateError::Server`] with a neutral
/// context, so `?` converts [`ArcadeDbError`] automatically. Prefer wrapping
/// with an explicit context (`.map_err`) at call sites that know what phase
/// they were in — the internal engine calls all do.
impl From<ArcadeDbError> for MigrateError {
    fn from(source: ArcadeDbError) -> Self {
        MigrateError::Server {
            context: "server call".into(),
            source,
        }
    }
}

impl From<SyncDirError> for MigrateError {
    fn from(e: SyncDirError) -> Self {
        MigrateError::SyncDir(Box::new(e))
    }
}
