use crate::error::{MigrateError, Result};

use arcadedb_protocol::ArcadeDbClient;

use super::LOCK_TYPE;
use super::OVERRIDES_TYPE;
use super::PROGRESS_TYPE;
use super::REVISIONS_TYPE;
use super::SNAPSHOT_TYPE;

/// Create the bookkeeping types if they don't exist (idempotent).
pub async fn ensure_tables(client: &ArcadeDbClient, db: &str) -> Result<()> {
    // Schema DDL for our own bookkeeping. Plain `IF NOT EXISTS` is fine — these
    // never change shape. The `statements` column on the overrides registry is
    // a lazy migration (added after the table shipped); `IF NOT EXISTS` makes
    // it idempotent on databases that already have it.
    let ddl = format!(
        "CREATE DOCUMENT TYPE {REVISIONS_TYPE} IF NOT EXISTS;
         CREATE PROPERTY {REVISIONS_TYPE}.checksum IF NOT EXISTS STRING;
         CREATE PROPERTY {REVISIONS_TYPE}.applied_at IF NOT EXISTS DATETIME;
         CREATE PROPERTY {REVISIONS_TYPE}.actions IF NOT EXISTS STRING;
         CREATE INDEX idx_{REVISIONS_TYPE}_checksum IF NOT EXISTS ON {REVISIONS_TYPE}(checksum) NOTUNIQUE;

         CREATE DOCUMENT TYPE {OVERRIDES_TYPE} IF NOT EXISTS;
         CREATE PROPERTY {OVERRIDES_TYPE}.filename IF NOT EXISTS STRING;
         CREATE PROPERTY {OVERRIDES_TYPE}.checksum IF NOT EXISTS STRING;
         CREATE PROPERTY {OVERRIDES_TYPE}.applied_at IF NOT EXISTS DATETIME;
         CREATE PROPERTY {OVERRIDES_TYPE}.statements IF NOT EXISTS STRING;
         CREATE INDEX idx_{OVERRIDES_TYPE}_filename IF NOT EXISTS ON {OVERRIDES_TYPE}(filename) UNIQUE;

         CREATE DOCUMENT TYPE {SNAPSHOT_TYPE} IF NOT EXISTS;
         CREATE PROPERTY {SNAPSHOT_TYPE}.checksum IF NOT EXISTS STRING;
         CREATE PROPERTY {SNAPSHOT_TYPE}.state_json IF NOT EXISTS STRING;
         CREATE PROPERTY {SNAPSHOT_TYPE}.default_exprs IF NOT EXISTS STRING;
         CREATE PROPERTY {SNAPSHOT_TYPE}.applied_at IF NOT EXISTS DATETIME;

         CREATE DOCUMENT TYPE {PROGRESS_TYPE} IF NOT EXISTS;
         CREATE PROPERTY {PROGRESS_TYPE}.filename IF NOT EXISTS STRING;
         CREATE PROPERTY {PROGRESS_TYPE}.kind IF NOT EXISTS STRING;
         CREATE PROPERTY {PROGRESS_TYPE}.ordinal IF NOT EXISTS INTEGER;
         CREATE INDEX idx_{PROGRESS_TYPE}_filename IF NOT EXISTS ON {PROGRESS_TYPE}(filename) NOTUNIQUE;

         CREATE DOCUMENT TYPE {LOCK_TYPE} IF NOT EXISTS;
         CREATE PROPERTY {LOCK_TYPE}.key IF NOT EXISTS STRING;
         CREATE PROPERTY {LOCK_TYPE}.holder IF NOT EXISTS STRING;
         CREATE PROPERTY {LOCK_TYPE}.acquired_ms IF NOT EXISTS LONG;
         CREATE PROPERTY {LOCK_TYPE}.expires_ms IF NOT EXISTS LONG;
         CREATE INDEX idx_{LOCK_TYPE}_key IF NOT EXISTS ON {LOCK_TYPE}(key) UNIQUE;"
    );
    client
        .execute_language(db, "sqlscript", &format!("BEGIN;\n{ddl}\nCOMMIT;"))
        .await
        .map_err(|e| MigrateError::Server {
            context: "creating schema bookkeeping tables".into(),
            source: e,
        })?;
    Ok(())
}

/// Record an applied sync (checksum + the DDL actions that ran).
pub async fn record_revision(
    client: &ArcadeDbClient,
    db: &str,
    checksum: &str,
    actions_json: &str,
) -> Result<()> {
    // Single-quote-escape the JSON payload for the SQL string literal.
    let escaped_json = actions_json.replace('\\', "\\\\").replace('\'', "\\'");
    let sql = format!(
        "INSERT INTO {REVISIONS_TYPE} SET \
           checksum = '{checksum}', \
           applied_at = sysdate(), \
           actions = '{escaped_json}'"
    );
    client
        .execute(db, &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "recording schema revision".into(),
            source: e,
        })?;
    Ok(())
}

/// Record an applied override file — its content checksum (immutability) and
/// the statements it ran (override-object ownership derivation).
pub async fn record_override(
    client: &ArcadeDbClient,
    db: &str,
    filename: &str,
    checksum: &str,
    statements: &[String],
) -> Result<()> {
    // Escape the filename for the SQL string literal — filenames are
    // filesystem-sourced and may contain single quotes. Sibling
    // `record_revision`/`write_snapshot` escape the same way.
    let escaped_filename = sql_escape(filename);
    let statements_json = serde_json::to_string(statements).unwrap_or_else(|_| "[]".into());
    let sql = format!(
        "INSERT INTO {OVERRIDES_TYPE} SET \
           filename = '{escaped_filename}', \
           checksum = '{checksum}', \
           applied_at = sysdate(), \
           statements = '{}'",
        sql_escape(&statements_json)
    );
    client
        .execute(db, &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "recording applied override".into(),
            source: e,
        })?;
    Ok(())
}

/// Backslash-style single-quote escape for an ArcadeDB SQL string literal
/// (matches the engine's string parsing — NOT SQL-standard `''` doubling).
pub(crate) fn sql_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
