//! Connection configuration: the [`ClientOptions`] builder.
//!
//! Everything [`ArcadeDbClient::connect_with`](crate::ArcadeDbClient::connect_with)
//! needs, spelled explicitly: endpoint, credentials, TLS,
//! connect/retry policy, and the default per-request deadline. Credentials
//! are validated up front (non-ASCII metadata is a typed error, never a
//! panic).

use std::time::Duration;

use tonic::transport::ClientTlsConfig;

use crate::error::{ArcadeDbError, Result};

/// Connection settings for
/// [`ArcadeDbClient::connect_with`](crate::ArcadeDbClient::connect_with).
///
/// Defaults: plaintext transport, 30 connect attempts with a 400ms backoff,
/// no per-request deadline.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub(crate) addr: String,
    pub(crate) user: String,
    pub(crate) pass: String,
    pub(crate) database: String,
    /// `Some` → TLS transport (`https` scheme + this config).
    pub(crate) tls: Option<ClientTlsConfig>,
    /// Per-attempt TCP connect deadline.
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) connect_attempts: u32,
    pub(crate) connect_backoff: Duration,
    /// Applied to every request as the gRPC `grpc-timeout` deadline.
    pub(crate) request_timeout: Option<Duration>,
    /// Create the database on connect when it does not exist (the Java
    /// `DatabaseFactory.exists() → create()/open()` idiom).
    pub(crate) open_or_create: bool,
}

impl ClientOptions {
    /// Settings for `addr` (`host:port`), credentials, and the default
    /// database — plaintext transport, default retry policy.
    pub fn new(
        addr: impl Into<String>,
        user: impl Into<String>,
        pass: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self {
            addr: addr.into(),
            user: user.into(),
            pass: pass.into(),
            database: database.into(),
            tls: None,
            connect_timeout: None,
            connect_attempts: 30,
            connect_backoff: Duration::from_millis(400),
            request_timeout: None,
            open_or_create: false,
        }
    }

    /// After connecting, create `database` if it does not exist (as a
    /// `"graph"` database). Tolerates the `AlreadyExists` race — two
    /// bootstrapping workers pointing at the same fresh database both
    /// succeed.
    pub fn open_or_create(mut self, yes: bool) -> Self {
        self.open_or_create = yes;
        self
    }

    /// Use a TLS transport with this config (e.g.
    /// `ClientTlsConfig::new().ca_certificate(..)`). Implies the `https`
    /// scheme; without it the channel is plaintext.
    pub fn tls(mut self, config: ClientTlsConfig) -> Self {
        self.tls = Some(config);
        self
    }

    /// Per-attempt TCP connect deadline (a cold JVM that accepts nothing
    /// fails the attempt fast instead of hanging).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// How many times to retry the TCP connect before giving up (default 30).
    pub fn connect_attempts(mut self, attempts: u32) -> Self {
        self.connect_attempts = attempts.max(1);
        self
    }

    /// Delay between connect attempts (default 400ms).
    pub fn connect_backoff(mut self, backoff: Duration) -> Self {
        self.connect_backoff = backoff;
        self
    }

    /// Default per-request gRPC deadline, applied to every call that does
    /// not set its own timeout. Prevents a hung server from stalling callers
    /// forever (and the [`Transaction`](crate::Transaction) drop-guard from
    /// silently rolling back a wedged transaction).
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Read settings from the environment with the standard `ARCADEDB_`
    /// prefix: `ARCADEDB_ADDR`, `ARCADEDB_USER`, `ARCADEDB_PASSWORD`,
    /// `ARCADEDB_DATABASE`. Missing/empty variables are a
    /// [`ArcadeDbError::InvalidInput`] naming the first offender.
    pub fn from_env() -> Result<Self> {
        Self::from_env_prefix("ARCADEDB")
    }

    /// [`from_env`](Self::from_env) with a custom prefix (`{PREFIX}_ADDR`, …).
    pub fn from_env_prefix(prefix: &str) -> Result<Self> {
        let get = |suffix: &str| -> Result<String> {
            let name = format!("{prefix}_{suffix}");
            std::env::var(&name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| ArcadeDbError::InvalidInput {
                    message: format!(
                        "ClientOptions::from_env: environment variable `{name}` is not set"
                    ),
                })
        };
        Ok(Self::new(
            get("ADDR")?,
            get("USER")?,
            get("PASSWORD")?,
            get("DATABASE")?,
        ))
    }

    /// Validate before dialing: non-empty fields and ASCII-only metadata
    /// values (anything else is a typed error, never a panic).
    pub(crate) fn validate(&self) -> Result<()> {
        let check = |value: &str, what: &str| -> Result<()> {
            if value.is_empty() {
                return Err(ArcadeDbError::InvalidInput {
                    message: format!("ClientOptions: {what} must not be empty"),
                });
            }
            if value.parse::<tonic::metadata::MetadataValue<_>>().is_err() {
                return Err(ArcadeDbError::InvalidInput {
                    message: format!(
                        "ClientOptions: {what} contains characters invalid in a gRPC metadata value (visible ASCII only)"
                    ),
                });
            }
            Ok(())
        };
        check(&self.addr, "addr")?;
        check(&self.user, "user")?;
        check(&self.pass, "password")?;
        check(&self.database, "database")?;
        Ok(())
    }
}
