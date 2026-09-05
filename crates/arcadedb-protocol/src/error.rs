//! The client-facing error type: every fallible [`ArcadeDbClient`](crate::ArcadeDbClient)
//! / [`DatabaseClient`](crate::DatabaseClient) call returns
//! [`Result<T, ArcadeDbError>`](Result) instead of an opaque error.
//! Matching on the failure category distinguishes transport vs.
//! server-rejected vs. decode vs. client-side validation errors, and the
//! gRPC [`tonic::Code`] stays inspectable.

use crate::record::RecordDecodeError;

/// The error type for all client operations.
///
/// Variants are grouped by failure category; see each variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArcadeDbError {
    /// The connect retry loop exhausted its attempts (cold server that never
    /// came up, wrong address, …). `source` is the last transport error when
    /// at least one attempt got that far.
    #[error("failed to connect to ArcadeDB gRPC at {addr} after {attempts} attempts")]
    Connect {
        /// The `host:port` that was retried.
        addr: String,
        /// How many connect attempts were made.
        attempts: u32,
        /// The last underlying transport error, if any attempt produced one.
        #[source]
        source: Option<tonic::transport::Error>,
    },

    /// The endpoint string could not be parsed into a gRPC endpoint.
    #[error("invalid gRPC endpoint {addr}")]
    InvalidEndpoint {
        /// The rejected endpoint string.
        addr: String,
        /// Why the URI parser rejected it.
        #[source]
        source: http::uri::InvalidUri,
    },

    /// A gRPC call failed at the transport/status layer. The full
    /// [`tonic::Status`] is preserved — use [`ArcadeDbError::code`] to branch
    /// on the gRPC code (e.g. retry on `Unavailable`, fail fast on
    /// `Unauthenticated`).
    #[error("{operation} failed: {status} (in: {detail})")]
    Rpc {
        /// What the client was doing, e.g. `"query"`, `"commit transaction"`.
        operation: String,
        /// The operation's subject — the SQL text, rid, database name, … —
        /// empty when the operation has no natural subject.
        detail: String,
        /// The underlying gRPC status (code, message, metadata).
        #[source]
        status: tonic::Status,
    },

    /// The server rejected the operation because the target already exists
    /// (database, type, property, index). Normalized from both transport
    /// failures and `success = false` responses — the engine reports this
    /// only through the error *message* (no structured gRPC code yet), so the
    /// client detects it once at its boundary; matching this variant is the
    /// stable consumer-facing contract.
    #[error("{what}: {message}")]
    AlreadyExists {
        /// What the client was doing, e.g. `"create database"`.
        what: String,
        /// The server's own explanation.
        message: String,
    },

    /// The server processed the request but answered `success = false`.
    #[error("command failed: {message}")]
    CommandFailed {
        /// What the client was doing, e.g. `"commit transaction"`.
        operation: String,
        /// The server's own explanation.
        message: String,
        /// Records the server reported as affected, when it reports a count.
        affected: Option<i64>,
    },

    /// A typed decode of a returned record failed (e.g. the `TryFrom<&GrpcRecord>`
    /// in [`lookup`](crate::ArcadeDbClient::lookup)).
    #[error("decode {what}")]
    Decode {
        /// What was being decoded, e.g. `"lookup record #12:0"`.
        what: String,
        /// The typed decode failure — missing field, type mismatch, ….
        #[source]
        source: RecordDecodeError,
    },

    /// A read that was expected to return at least one row returned none
    /// ([`fetch_one`](crate::ArcadeDbClient::fetch_one),
    /// [`QueryResult::one`](crate::QueryResult::one),
    /// [`create_and_return`](crate::ArcadeDbClient::create_and_return)).
    /// Distinct from `Rpc` — the transport and the command both succeeded;
    /// the empty result *is* the answer.
    #[error("no rows returned by {operation} (expected at least one: {detail})")]
    NotFound {
        /// What was reading, e.g. `"fetch_one"`.
        operation: String,
        /// The query or subject that came up empty.
        detail: String,
    },

    /// Client-side validation rejected the call before it hit the wire
    /// (e.g. a `bulk_upsert_dtos` DTO without `#[record(key)]` columns).
    #[error("{message}")]
    InvalidInput {
        /// What was rejected and why.
        message: String,
    },
}

impl ArcadeDbError {
    /// The gRPC status code, when this error carries a [`tonic::Status`]
    /// (only [`Rpc`](ArcadeDbError::Rpc) does). Useful for retry/fail-fast
    /// branching without matching the variant structure.
    pub fn code(&self) -> Option<tonic::Code> {
        match self {
            ArcadeDbError::Rpc { status, .. } => Some(status.code()),
            _ => None,
        }
    }

    /// The server's own error message, when there is one
    /// ([`ArcadeDbError::Rpc`], [`ArcadeDbError::CommandFailed`], and
    /// [`ArcadeDbError::AlreadyExists`] carry it).
    pub fn server_message(&self) -> Option<&str> {
        match self {
            ArcadeDbError::Rpc { status, .. } => Some(status.message()),
            ArcadeDbError::CommandFailed { message, .. } => Some(message),
            ArcadeDbError::AlreadyExists { message, .. } => Some(message),
            _ => None,
        }
    }
}

/// The crate-wide result alias: every fallible client operation returns it.
pub type Result<T, E = ArcadeDbError> = std::result::Result<T, E>;
