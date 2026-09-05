use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::args::Args;

// ---------------------------------------------------------------------------
// DB connect / provision
// ---------------------------------------------------------------------------

pub(super) async fn connect(args: &Args) -> Result<ArcadeDbClient> {
    let addr = std::env::var("ARCADEDB_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());
    let user = std::env::var("ARCADEDB_USER").unwrap_or_else(|_| "root".into());
    let pass = std::env::var("ARCADEDB_PASS").unwrap_or_else(|_| "password".into());
    let client = ArcadeDbClient::connect(&addr, &user, &pass, &args.db)
        .await
        .map_err(|e| MigrateError::Server {
            context: format!("connecting to ArcadeDB at {addr}"),
            source: e,
        })?;
    Ok(client)
}

/// Create the DB if absent. Tolerant of "already exists" — the client
/// normalizes that rejection to [`ArcadeDbError::AlreadyExists`] (the engine
/// has no structured error code for it yet, so detection is message-based at
/// the client boundary).
pub(super) async fn ensure_database(client: &ArcadeDbClient, db: &str) -> Result<()> {
    match client.admin().create_database(db, "graph").await {
        Ok(()) => Ok(()),
        Err(arcadedb_protocol::ArcadeDbError::AlreadyExists { .. }) => Ok(()),
        Err(e) => Err(MigrateError::Server {
            context: "create_database".into(),
            source: e,
        }),
    }
}
