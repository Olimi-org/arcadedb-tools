//! ArcadeDB gRPC client — wraps the generated tonic service stubs.
//!
//! The server-side `GrpcAuthInterceptor` checks gRPC metadata headers
//! (`x-arcade-user`, `x-arcade-password`, `x-arcade-database`) for DATA service
//! calls, so each data request attaches these headers via `add_auth_headers`.
//! Admin service calls are skipped by the interceptor and use body credentials.

use std::ops::ControlFlow;
use std::ops::Deref;
use std::slice::Iter;
use std::time::Duration;
use std::vec::IntoIter;

use tokio_stream::{Stream, StreamExt};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::error::{ArcadeDbError, Result};
use crate::options::ClientOptions;

// This file wraps the entire gRPC surface, so take the proto module as a
// whole instead of maintaining a hand-curated list. Local definitions (e.g.
// the owned `QueryResult` below) shadow their glob-imported names; nested
// modules (`insert_options::ConflictMode`, `update_record_request::Payload`,
// the service clients) resolve through the same glob.
pub use crate::proto::com::arcadedb::grpc::*;
use crate::proto::com::arcadedb::grpc::{
    arcade_db_admin_service_client::ArcadeDbAdminServiceClient,
    arcade_db_service_client::ArcadeDbServiceClient, insert_options::ConflictMode,
    update_record_request::Payload,
};
use crate::record::RecordDecodeError;

// Capability groups — one file per method family. `ArcadeDbClient` method
// blocks live with their group; shared helpers and the connect/auth core
// stay here. Each file opens with `use super::*;` and inherits this
// module's imports.
mod database;
mod execute;
mod query;
mod records;
mod results;
mod streaming;
mod transaction;

// Re-exported for `crate::client::` users (facades) and the crate root.
pub use database::DatabaseClient;
pub use results::{QueryResult, QueryResultBatch, StreamSummary};
pub use streaming::consume_stream;
pub use transaction::{Transaction, TxCommands};

/// Surface a gRPC RPC failure as an [`ArcadeDbError`] that includes the
/// ArcadeDB server's own message (tonic carries it in the `Status`). Without
/// this, a bare `.context("query failed")?` hides why ArcadeDB rejected the
    /// request — e.g. "Index 'item[title]' was not found" — making debugging
/// near-impossible. Unlike stringifying, the typed variant keeps the tonic
/// [`tonic::Code`] and metadata inspectable via [`ArcadeDbError::code`].
///
/// The engine's "already exists" rejection is normalized here to the
/// dedicated [`ArcadeDbError::AlreadyExists`] variant: ArcadeDB reports it
/// only through the error message (or, when it does set a code, gRPC's
/// `AlreadyExists`), never a structured field, so the client detects it once
/// at this single funnel every RPC failure passes through.
pub(crate) fn map_rpc_error(what: &str, detail: &str, err: tonic::Status) -> ArcadeDbError {
    let message = err.message();
    if err.code() == tonic::Code::AlreadyExists
        || message.to_ascii_lowercase().contains("already exist")
    {
        return ArcadeDbError::AlreadyExists {
            what: what.into(),
            message: message.into(),
        };
    }
    ArcadeDbError::Rpc {
        operation: what.into(),
        detail: detail.into(),
        status: err,
    }
}

/// Construct the `success = false` error for an `ExecuteCommand` response,
/// applying the same "already exists" normalization as [`map_rpc_error`] so
/// both transport failures and rejected commands surface the same variant.
fn command_failed(operation: &str, message: String, affected: i64) -> ArcadeDbError {
    if message.to_ascii_lowercase().contains("already exist") {
        return ArcadeDbError::AlreadyExists {
            what: operation.into(),
            message,
        };
    }
    ArcadeDbError::CommandFailed {
        operation: operation.into(),
        message,
        affected: Some(affected),
    }
}

/// Auth state resolved ONCE at connect: metadata headers are re-attached to
/// every DATA request and the credentials template re-cloned into every
/// request body — pre-parsing the header values and caching the template
/// removes the per-RPC string parses/allocations.
struct AuthState {
    user_val: MetadataValue<tonic::metadata::Ascii>,
    pass_val: MetadataValue<tonic::metadata::Ascii>,
    db_val: MetadataValue<tonic::metadata::Ascii>,
    /// Database name (also surfaced via [`ArcadeDbClient::database`]).
    database: String,
    /// Body-credential template for ADMIN/body-credential requests (prost
    /// needs owned `String`s per message, so a struct clone is the floor).
    creds: DatabaseCredentials,
    /// Default per-request gRPC deadline (from
    /// [`ClientOptions::request_timeout`](crate::ClientOptions::
    /// request_timeout)); applied by [`ArcadeDbClient::add_auth_headers`].
    request_timeout: Option<Duration>,
}

/// A lightweight gRPC client for ArcadeDB.
///
/// Wraps both the data and admin service clients with convenience methods
/// for SQL commands and queries. `Clone` (cheap — shares the underlying
/// tonic `Channel`) and all methods take `&self`, so it can be held by a
/// `Clone` repo and shared across tasks.
#[derive(Clone)]
pub struct ArcadeDbClient {
    data: ArcadeDbServiceClient<Channel>,
    admin: ArcadeDbAdminServiceClient<Channel>,
    auth: std::sync::Arc<AuthState>,
}

impl ArcadeDbClient {
    /// Connect to an ArcadeDB server over gRPC.
    ///
    /// `addr` should be `host:port` (e.g. `127.0.0.1:50051`).
    /// Uses an insecure (plaintext) channel — the standard for dev setups.
    /// For TLS, deadlines, or a configurable connect/retry policy, build a
    /// [`ClientOptions`] and call [`connect_with`](Self::connect_with).
    ///
    /// The `database` parameter is used for the `x-arcade-database` metadata header,
    /// which the server-side `GrpcAuthInterceptor` checks to determine which
    /// database's user list to validate against.
    pub async fn connect(addr: &str, user: &str, pass: &str, database: &str) -> Result<Self> {
        Self::connect_with(ClientOptions::new(addr, user, pass, database)).await
    }

    /// [`connect`](Self::connect) with explicit
    /// [`ClientOptions`]: TLS, connect timeout/attempts/backoff, and the
    /// default per-request gRPC deadline.
    ///
    /// Credential validation is up front and typed: metadata-invalid
    /// credentials (e.g. non-ASCII) are an [`ArcadeDbError::InvalidInput`],
    /// never a panic.
    pub async fn connect_with(options: ClientOptions) -> Result<Self> {
        options.validate()?;
        let addr = options.addr.clone();

        // Parse the endpoint URI once — the retry loop below does not
        // rebuild (and re-parse) it per attempt.
        let scheme = if options.tls.is_some() {
            "https"
        } else {
            "http"
        };
        let mut endpoint = Channel::from_shared(format!("{scheme}://{addr}")).map_err(|e| {
            ArcadeDbError::InvalidEndpoint {
                addr: addr.clone(),
                source: e,
            }
        })?;
        if let Some(timeout) = options.connect_timeout {
            endpoint = endpoint.connect_timeout(timeout);
        }
        if let Some(tls) = options.tls {
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|e| ArcadeDbError::InvalidInput {
                    message: format!("ClientOptions: TLS config rejected by the transport: {e}"),
                })?;
        }

        // Retry the TCP connect so a cold container (JVM still warming up) is
        // waited for instead of failing the caller immediately.
        let attempts = options.connect_attempts;
        let backoff = options.connect_backoff;
        let channel = {
            let mut last_err: Option<tonic::transport::Error> = None;
            let mut ch: Option<Channel> = None;
            for attempt in 0..attempts {
                match endpoint.clone().connect().await {
                    Ok(c) => {
                        ch = Some(c);
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(attempt, %addr, "arcade connect retry: {e}");
                        last_err = Some(e);
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
            ch.ok_or_else(|| ArcadeDbError::Connect {
                addr: addr.clone(),
                attempts,
                source: last_err,
            })?
        };

        tracing::info!(%addr, "connected to ArcadeDB gRPC");

        // open_or_create (J7): create the database if absent, tolerating the
        // create race between concurrent bootstrappers.
        if options.open_or_create {
            let mut admin = ArcadeDbAdminServiceClient::new(channel.clone());
            let creds = DatabaseCredentials {
                username: options.user.clone(),
                password: options.pass.clone(),
            };
            let exists = admin
                .clone()
                .exists_database(ExistsDatabaseRequest {
                    credentials: Some(creds.clone()),
                    name: options.database.clone(),
                })
                .await
                .map_err(|e| {
                    map_rpc_error("exists database (open_or_create)", &options.database, e)
                })?
                .into_inner()
                .exists;
            if !exists {
                match admin
                    .create_database(CreateDatabaseRequest {
                        credentials: Some(creds),
                        name: options.database.clone(),
                        r#type: "graph".into(),
                    })
                    .await
                {
                    Ok(_) => {
                        tracing::info!(db = %options.database, "database created (open_or_create)")
                    }
                    // Normalized by the map_rpc_error funnel — tolerate the
                    // concurrent-create race.
                    Err(e) => {
                        let e =
                            map_rpc_error("create database (open_or_create)", &options.database, e);
                        if !matches!(e, ArcadeDbError::AlreadyExists { .. }) {
                            return Err(e);
                        }
                    }
                }
            }
        }

        let parse_meta = |value: &str,
                          what: &str|
         -> Result<MetadataValue<tonic::metadata::Ascii>> {
            value.parse().map_err(|_| ArcadeDbError::InvalidInput {
                message: format!(
                    "ClientOptions: {what} contains characters invalid in a gRPC metadata value (visible ASCII only)"
                ),
            })
        };

        Ok(Self {
            data: ArcadeDbServiceClient::new(channel.clone()),
            admin: ArcadeDbAdminServiceClient::new(channel),
            auth: std::sync::Arc::new(AuthState {
                user_val: parse_meta(&options.user, "user")?,
                pass_val: parse_meta(&options.pass, "password")?,
                db_val: parse_meta(&options.database, "database")?,
                database: options.database.clone(),
                creds: DatabaseCredentials {
                    username: options.user,
                    password: options.pass,
                },
                request_timeout: options.request_timeout,
            }),
        })
    }

    /// The database name this client is authenticated against.
    pub fn database(&self) -> &str {
        &self.auth.database
    }

    /// Attach ArcadeDB auth metadata headers (and the default request
    /// deadline, when configured) to a gRPC request.
    ///
    /// The server-side `GrpcAuthInterceptor` reads credentials from metadata
    /// headers (`x-arcade-user`, `x-arcade-password`, `x-arcade-database`)
    /// for DATA service calls. Admin service is skipped by the interceptor
    /// and uses body credentials directly.
    pub(crate) fn add_auth_headers<T>(&self, mut req: tonic::Request<T>) -> tonic::Request<T> {
        let auth = &self.auth;
        req.metadata_mut()
            .insert("x-arcade-user", auth.user_val.clone());
        req.metadata_mut()
            .insert("x-arcade-password", auth.pass_val.clone());
        req.metadata_mut()
            .insert("x-arcade-database", auth.db_val.clone());
        if let Some(timeout) = auth.request_timeout {
            req.set_timeout(timeout);
        }

        req
    }

    /// The admin-service stub — for the [`admin`](crate::admin) facade.
    pub(crate) fn admin_stub(&self) -> ArcadeDbAdminServiceClient<Channel> {
        self.admin.clone()
    }

    /// The body-credential template — for the [`admin`](crate::admin) facade.
    pub(crate) fn creds(&self) -> DatabaseCredentials {
        self.auth.creds.clone()
    }

    /// Domain facades ([`admin`](crate::admin) / [`scan`](crate::scan) /
    /// [`batch`](crate::batch) / [`cache`](crate::cache)) — capability
    /// groups off the root client. Cheap: each wraps a clone of this
    /// client's shared channel.
    pub fn admin(&self) -> crate::admin::Admin {
        crate::admin::Admin::new(self.clone())
    }

    /// [`scan`](crate::scan) facade — full-type/bucket iteration.
    pub fn scan(&self) -> crate::scan::Scan {
        crate::scan::Scan::new(self.clone())
    }

    /// [`batch`](crate::batch) facade — `sqlscript` batches, edge upserts,
    /// idempotent DDL apply.
    pub fn batch(&self) -> crate::batch::Batch {
        crate::batch::Batch::new(self.clone())
    }

    /// [`cache`](crate::cache) facade (`cache` feature) — transient KV over
    /// the Redis plugin: GET/SET/GETDEL with hex-safe values and a
    /// client-enforced TTL.
    #[cfg(feature = "cache")]
    pub fn cache(&self) -> crate::cache::Cache {
        crate::cache::Cache::new(self.clone())
    }
}

/// Property/type names interpolated into generated SQL (lookup_by_key,
/// count_type, scan, batch) must be plain identifiers — anything else is a
/// client-side [`ArcadeDbError::InvalidInput`], never a quoting game.
pub(crate) fn validate_identifier(name: &str, what: &str) -> Result<String> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name.to_string())
    } else {
        Err(ArcadeDbError::InvalidInput {
            message: format!(
                "invalid {what} `{name}`: expected non-empty ASCII letters, digits, or `_`"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{query::single_column_value, transaction::is_retryable_conflict};

    #[test]
    fn already_exists_rejections_normalize_to_the_dedicated_variant() {
        // By message (the engine's actual shape today — no structured code):
        // both the "already exists" and "already existing" spellings.
        for msg in [
            "Database 'oak' already exists",
            "Type 'item' already existing in database",
        ] {
            let err = map_rpc_error("create database", "oak", tonic::Status::internal(msg));
            assert!(
                matches!(err, ArcadeDbError::AlreadyExists { .. }),
                "message {msg:?} should normalize"
            );
            assert_eq!(err.server_message(), Some(msg));
        }

        // By gRPC code, when the server does set it.
        let err = map_rpc_error(
            "create database",
            "oak",
            tonic::Status::already_exists("Database 'oak' already exists"),
        );
        assert!(matches!(err, ArcadeDbError::AlreadyExists { .. }));

        // Everything else stays a plain Rpc error.
        let err = map_rpc_error("query", "SELECT 1", tonic::Status::unavailable("boom"));
        assert!(matches!(err, ArcadeDbError::Rpc { .. }));

        // And the success=false path normalizes identically.
        let err = command_failed(
            "execute command",
            "Error: Type 'item' already exists".into(),
            0,
        );
        assert!(matches!(err, ArcadeDbError::AlreadyExists { .. }));
        let err = command_failed("execute command", "syntax error".into(), 0);
        assert!(matches!(err, ArcadeDbError::CommandFailed { .. }));
    }

    #[test]
    fn identifier_validation_accepts_only_plain_names() {
        assert_eq!(validate_identifier("item", "type name").unwrap(), "item");
        assert_eq!(
            validate_identifier("user_shelf_v2", "type name").unwrap(),
            "user_shelf_v2"
        );
        for bad in [
            "",
            "item; DROP",
            "has space",
            "has-dash",
            "schema:types",
            "a.b",
        ] {
            assert!(
                validate_identifier(bad, "type name").is_err(),
                "`{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn mvcc_conflicts_are_detected_from_the_server_message() {
        for msg in [
            "The record was changed by concurrent transaction",
            "MVCC conflict on update",
            "NeedRetryException: please retry the transaction",
        ] {
            let err = map_rpc_error("commit transaction", "", tonic::Status::aborted(msg));
            assert!(is_retryable_conflict(&err), "`{msg}` must be retryable");
        }
        let err = map_rpc_error("query", "", tonic::Status::internal("syntax error"));
        assert!(!is_retryable_conflict(&err));
        // A plain Rpc error without a message-ish content is not retryable.
        let err = ArcadeDbError::NotFound {
            operation: "x".into(),
            detail: "y".into(),
        };
        assert!(!is_retryable_conflict(&err));
    }

    #[test]
    fn one_row_accessors_decode_or_report_not_found() {
        let row = crate::rec("t", vec![("name", crate::encode::str_v("oak"))]);

        let empty = QueryResult {
            records: vec![],
            execution_time_ms: 0,
        };
        assert!(empty.first::<ItemRow>().unwrap().is_none());
        assert!(matches!(
            empty.one::<ItemRow>(),
            Err(ArcadeDbError::NotFound { .. })
        ));

        let single = QueryResult {
            records: vec![row],
            execution_time_ms: 0,
        };
        let row = single.first::<ItemRow>().unwrap().unwrap();
        assert_eq!(row.name, "oak");
        assert_eq!(single.one::<ItemRow>().unwrap().name, "oak");

        // Multi-column rows are rejected for scalars — map order is not
        // "first column" order.
        let wide = crate::rec(
            "t",
            vec![
                ("a", crate::encode::str_v("1")),
                ("b", crate::encode::str_v("2")),
            ],
        );
        assert!(single_column_value(&wide, "SELECT a, b FROM t").is_err());
        let lone = crate::rec("t", vec![("count", crate::encode::i64_v(7))]);
        let scalar = single_column_value(&lone, "SELECT count(*) AS count FROM t")
            .unwrap()
            .clone();
        assert!(matches!(scalar.kind, Some(grpc_value::Kind::Int64Value(7))));
    }

    #[derive(Clone)]
    struct ItemRow {
        name: String,
    }

    impl<'a> TryFrom<&'a GrpcRecord> for ItemRow {
        type Error = RecordDecodeError;

        fn try_from(rec: &'a GrpcRecord) -> Result<Self, Self::Error> {
            let name = rec
                .properties
                .get("name")
                .ok_or_else(|| RecordDecodeError::missing_field("name"))?;
            let Some(grpc_value::Kind::StringValue(s)) = &name.kind else {
                return Err(RecordDecodeError::conversion("name: expected string"));
            };
            Ok(ItemRow { name: s.clone() })
        }
    }
}
