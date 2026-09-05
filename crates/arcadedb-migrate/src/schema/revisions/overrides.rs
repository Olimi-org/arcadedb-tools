use std::collections::BTreeMap;

use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::OVERRIDES_TYPE;

/// One applied override's registry row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedOverride {
    /// The file-content checksum recorded at apply time (immutability check).
    pub checksum: String,
    /// The statements the override ran (stored at record time so the diff can
    /// derive which schema objects are override-*owned*). Empty for rows
    /// recorded before this column existed.
    pub statements: Vec<String>,
}

/// The applied-override registry: filename → its recorded content + checksum.
/// Callers re-verify the checksum against the current file (an edited applied
/// migration is a hard error) and may scan the stored statements for
/// object-ownership tracking.
pub async fn applied_overrides(
    client: &ArcadeDbClient,
    db: &str,
) -> Result<BTreeMap<String, AppliedOverride>> {
    let res = client
        .query(
            db,
            &format!("SELECT filename, checksum, statements FROM {OVERRIDES_TYPE}"),
        )
        .await
        .map_err(|e| MigrateError::Server {
            context: "reading applied overrides".into(),
            source: e,
        })?;
    let mut out = BTreeMap::new();
    for rec in res.records {
        let row = arcadedb_protocol::grpc_record_to_json(&rec);
        let (Some(name), Some(checksum)) = (
            row.get("filename").and_then(|v| v.as_str()),
            row.get("checksum").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let statements = row
            .get("statements")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default();
        out.insert(
            name.to_string(),
            AppliedOverride {
                checksum: checksum.to_string(),
                statements,
            },
        );
    }
    Ok(out)
}
