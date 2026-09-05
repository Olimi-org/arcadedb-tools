use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::super::model::{Schema, SchemaSnapshot};
use super::tables::sql_escape;
use super::SNAPSHOT_TYPE;

/// Read the applied-state snapshot — the introspected schema state right after
/// our last apply, plus the desired `DEFAULT` texts we applied and the checksum
/// that produced it. Returns `None` when no sync has ever run.
pub async fn read_snapshot(client: &ArcadeDbClient, db: &str) -> Result<Option<SchemaSnapshot>> {
    let res = client
        .query(
            db,
            &format!(
                "SELECT checksum, state_json, default_exprs FROM {SNAPSHOT_TYPE} ORDER BY applied_at DESC LIMIT 1"
            ),
        )
        .await
        .map_err(|e| MigrateError::Server {
            context: "reading schema_snapshot".into(),
            source: e,
        })?;
    let Some(rec) = res.records.into_iter().next() else {
        return Ok(None);
    };
    let row = arcadedb_protocol::grpc_record_to_json(&rec);
    let checksum = row
        .get("checksum")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MigrateError::Revision {
            message: "schema_snapshot.checksum not a string".into(),
        })?
        .to_string();
    let state_json = row
        .get("state_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MigrateError::Revision {
            message: "schema_snapshot.state_json not a string".into(),
        })?;
    let state: Schema = serde_json::from_str(state_json).map_err(|e| MigrateError::Serde {
        context: "deserializing schema_snapshot.state_json".into(),
        source: e,
    })?;
    let default_exprs = row
        .get("default_exprs")
        .and_then(|v| v.as_str())
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| MigrateError::Serde {
            context: "deserializing schema_snapshot.default_exprs".into(),
            source: e,
        })?
        .unwrap_or_default();
    Ok(Some(SchemaSnapshot {
        checksum,
        state,
        default_exprs,
    }))
}

/// Record the applied-state snapshot (single-row: any previous snapshot is
/// removed). `checksum` is the desired-schema checksum that produced the state;
/// `state` is the **introspected** schema *after* the apply (resolved values);
/// `default_exprs` is the `"{type}.{property}" → DEFAULT source text` map drawn
/// from the desired schema.
pub async fn write_snapshot(
    client: &ArcadeDbClient,
    db: &str,
    checksum: &str,
    state: &Schema,
    default_exprs: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let state_json = serde_json::to_string(state).map_err(|e| MigrateError::Serde {
        context: "serializing schema snapshot state".into(),
        source: e,
    })?;
    let exprs_json = serde_json::to_string(default_exprs).map_err(|e| MigrateError::Serde {
        context: "serializing schema snapshot defaults".into(),
        source: e,
    })?;
    let sql = format!(
        "BEGIN;\n\
         DELETE FROM {SNAPSHOT_TYPE};\n\
         INSERT INTO {SNAPSHOT_TYPE} SET \
           checksum = '{checksum}', \
           state_json = '{escaped}', \
           default_exprs = '{exprs}', \
           applied_at = sysdate();\n\
         COMMIT;",
        escaped = sql_escape(&state_json),
        exprs = sql_escape(&exprs_json),
    );
    client
        .execute_language(db, "sqlscript", &sql)
        .await
        .map_err(|e| MigrateError::Server {
            context: "recording schema snapshot".into(),
            source: e,
        })?;
    Ok(())
}
