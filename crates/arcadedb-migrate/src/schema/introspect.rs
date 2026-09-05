//! Read the database's actual schema into the [`Schema`] model, via the
//! `schema:types` virtual query target.
//!
//! `schema:types` returns one row per type with nested `properties[]` and
//! `indexes[]`, and it surfaces everything the structured model needs:
//! `parentTypes`, per-property `mandatory`/`notNull`/`readOnly`/`external`
//! (each key present **only when true**), `min`/`max`/`regexp` (strings),
//! `ofType` for container types, index `unique`. The result normalizes into
//! the same [`Schema`] shape the parser produces, so
//! [`diff()`](crate::schema::diff::diff) works symmetrically on desired-vs-actual.

use serde_json::Value;

use arcadedb_protocol::ArcadeDbClient;

use crate::error::{MigrateError, Result};

use super::ddl::Tail;
use super::model::{
    Constraints, DefaultExpr, Index, IndexKind, Property, Schema, TimeseriesColumn, TimeseriesSpec,
    TsRole, Type, TypeKind,
};
use super::revisions::{LOCK_TYPE, OVERRIDES_TYPE, PROGRESS_TYPE, REVISIONS_TYPE, SNAPSHOT_TYPE};

/// ArcadeDB's system root types (`V` for the vertex hierarchy, `E` for the
/// edge one). `schema:types` always returns them; they're not user schema and
/// must never be dropped (under `DropStrategy::Explicit` the diff would want
/// to). Filtered out of introspection along with the migrator's own
/// bookkeeping types (below).
const SYSTEM_TYPE_ROOTS: [&str; 2] = ["V", "E"];

/// The migrator's own bookkeeping tables (`schema_revisions`,
/// `schema_overrides_applied`, `schema_snapshot`, `schema_override_progress`,
/// `schema_migrate_lock`), created by [`super::revisions::ensure_tables`].
/// They are not user schema — they must never be dropped by a sync, and under
/// `DropStrategy::Explicit` the diff would otherwise treat them as leftovers
/// and emit `DROP TYPE`. Filtered out of introspection for the same reason as
/// the `V`/`E` system roots.
const BOOKKEEPING_TYPES: [&str; 5] = [
    REVISIONS_TYPE,
    OVERRIDES_TYPE,
    SNAPSHOT_TYPE,
    PROGRESS_TYPE,
    LOCK_TYPE,
];

/// Query the database's current schema.
///
/// Uses `SELECT FROM schema:types`, which returns one row per type with nested
/// `properties[]` and `indexes[]`. Decoded via the JSON bridge (the row shape
/// is dynamic).
///
/// Rows with an UNKNOWN type kind (e.g. TimeSeries types surface as `"t"`)
/// are **skipped with a warning**, not errors: the declarative
/// model cannot reconcile what it cannot express, and failing here would break
/// every sync against any database containing such a type. They are invisible
/// to the diff — never dropped, never reconciled — and must be managed via
/// override migrations (see docs/migration-model.md).
pub async fn fetch_actual(client: &ArcadeDbClient, db: &str) -> Result<Schema> {
    let res = client
        .query(db, "SELECT FROM schema:types")
        .await
        .map_err(|e| MigrateError::Server {
            context: "querying schema:types".into(),
            source: e,
        })?;

    let mut schema = Schema::new();
    for rec in &res.records {
        let row = arcadedb_protocol::grpc_record_to_json(rec);
        let ty = match parse_type_row(&row).map_err(|e| MigrateError::Introspect {
            message: format!("parsing a schema:types row: {e}"),
        })? {
            Some(ty) => ty,
            // Unknown kind — outside the declarative model.
            None => {
                let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
                let kind = row.get("type").and_then(Value::as_str).unwrap_or("?");
                tracing::warn!(
                    "skipping `{name}` (engine kind {kind:?}): not representable in the \
                     declarative schema model — manage it via override migrations"
                );
                continue;
            }
        };
        // System roots (V/E) and migrator bookkeeping tables are not user
        // schema — never surface them.
        if SYSTEM_TYPE_ROOTS.contains(&ty.name.as_str())
            || BOOKKEEPING_TYPES.contains(&ty.name.as_str())
        {
            continue;
        }
        schema.types.insert(ty.name.clone(), ty);
    }
    Ok(schema)
}

/// Parse one `schema:types` row into a [`Type`]. `Ok(None)` = recognized shape
/// but unknown kind (TimeSeries `"t"`, or anything newer) — skippable.
fn parse_type_row(row: &Value) -> Result<Option<Type>> {
    let name = row
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MigrateError::Introspect {
            message: format!("schema:types row missing 'name': {row}"),
        })?
        .to_string();
    let kind_str = row
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("document");
    let kind = match TypeKind::from_introspect(kind_str) {
        Some(kind) => kind,
        None => return Ok(None),
    };

    // `parentTypes` = the super-type list (mirrors the desired `EXTENDS`).
    let extends = row
        .get("parentTypes")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    // TimeSeries types carry their declaration in dedicated row fields:
    // `timestampColumn`, `tsColumns[]` (name/dataType/role), `shardCount`,
    // `retentionMs`, `compactionBucketIntervalMs`. PRECISION and BLOCK_SIZE
    // are NOT surfaced — create-time-only, never compared.
    let timeseries = if kind == TypeKind::Timeseries {
        Some(parse_timeseries_spec(row)?)
    } else {
        None
    };

    let mut properties = std::collections::BTreeMap::new();
    for p in row
        .get("properties")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        // `name` + `type` together form a property; skip rows missing either.
        if let Some((pname, ptype)) = p
            .get("name")
            .and_then(|v| v.as_str())
            .zip(p.get("type").and_then(|v| v.as_str()))
        {
            let type_name = property_type_name(p);
            properties.insert(
                pname.to_string(),
                Property {
                    name: pname.to_string(),
                    type_name: type_name.unwrap_or_else(|| ptype.to_ascii_uppercase()),
                    constraints: parse_property_constraints(p),
                },
            );
        }
    }

    let mut indexes = Vec::new();
    if let Some(idx) = row.get("indexes").and_then(|v| v.as_array()) {
        for i in idx {
            let iname = i
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let columns = index_columns(i);
            let itype = i
                .get("type")
                .or_else(|| i.get("indexType"))
                .and_then(|v| v.as_str())
                .unwrap_or("LSM_TREE");
            let unique = i.get("unique").and_then(|v| v.as_bool()).unwrap_or(false);
            let kind = IndexKind::from_introspect_with_unique(itype, unique).ok_or_else(|| {
                MigrateError::Introspect {
                    message: format!("schema:types index with unknown kind {itype:?}"),
                }
            })?;
            indexes.push(Index {
                name: iname,
                columns,
                kind,
                unique,
                // metadata is not surfaced in schema:types; re-sync always
                // emits it verbatim from the parsed desired schema.
                tail: Tail::none(),
            });
        }
    }

    Ok(Some(Type {
        name,
        kind,
        extends,
        // Create-time-only clauses (BUCKETS, PAGESIZE, ...) are not surfaced
        // by schema:types, and they're not diffed anyway — they're re-emitted
        // verbatim on CREATE and have no ALTER form.
        clause: Tail::none(),
        properties,
        indexes,
        timeseries,
    }))
}

/// Build the [`TimeseriesSpec`] from a TimeSeries `schema:types` row.
fn parse_timeseries_spec(row: &Value) -> Result<TimeseriesSpec> {
    let timestamp_column = row
        .get("timestampColumn")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut tags = Vec::new();
    let mut fields = Vec::new();
    for col in row
        .get("tsColumns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = col.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let data_type = col
            .get("dataType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let role_str = col.get("role").and_then(Value::as_str).unwrap_or_default();
        match TsRole::from_introspect(role_str) {
            Some(TsRole::Timestamp) => {} // covered by timestampColumn
            Some(TsRole::Tag) => tags.push(TimeseriesColumn {
                name: name.to_string(),
                data_type,
                role: TsRole::Tag,
            }),
            Some(TsRole::Field) => fields.push(TimeseriesColumn {
                name: name.to_string(),
                data_type,
                role: TsRole::Field,
            }),
            None => {
                return Err(MigrateError::Introspect {
                    message: format!("unknown TimeSeries column role {role_str:?}"),
                })
            }
        }
    }

    Ok(TimeseriesSpec {
        timestamp_column,
        // PRECISION is not surfaced by schema:types — never compared.
        precision: None,
        tags,
        fields,
        shards: row
            .get("shardCount")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        // Authored text isn't recoverable; store the engine's ms value as the
        // canonical duration string so desired-vs-actual comparisons compare
        // like-with-like when the authored form normalizes to the same ms.
        retention: row
            .get("retentionMs")
            .and_then(Value::as_i64)
            .map(ms_duration_text),
        compaction_interval: row
            .get("compactionBucketIntervalMs")
            .and_then(Value::as_i64)
            .filter(|ms| *ms > 0)
            .map(ms_duration_text),
        block_size: None,
    })
}

/// Render milliseconds as the canonical `<n> <unit>` duration text (largest
/// exact unit; falls back to raw ms when not whole seconds+).
fn ms_duration_text(ms: i64) -> String {
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;
    const MINUTE: i64 = 60_000;
    const SECOND: i64 = 1_000;
    match (
        ms / DAY,
        ms % DAY,
        ms / HOUR,
        ms % HOUR,
        ms / MINUTE,
        ms % MINUTE,
        ms / SECOND,
        ms % SECOND,
    ) {
        (d, 0, ..) if d > 0 => format!("{d} DAYS"),
        (_, _, h, 0, ..) if h > 0 => format!("{h} HOURS"),
        (_, _, _, _, m, 0, ..) if m > 0 => format!("{m} MINUTES"),
        (_, _, _, _, _, _, s, 0) if s > 0 => format!("{s} SECONDS"),
        _ => format!("{ms} MS"),
    }
}

/// Normalize an introspected property's `type` (+ `ofType`) to the canonical
/// `type_name` form. Container types come back split: `LIST` + `ofType: STRING`
/// → `LIST OF STRING`. Flat types (`STRING`, `ARRAY_OF_INTEGERS`, `MAP`, ...)
/// come back as-is — this returns `None` only when composition is impossible.
fn property_type_name(p: &Value) -> Option<String> {
    let ty = p.get("type")?.as_str()?;
    let canonical = match p.get("ofType").and_then(Value::as_str) {
        Some(of) => format!("{ty} OF {of}"),
        None => ty.to_string(),
    };
    Some(canonical.to_ascii_uppercase())
}

/// Parse a `schema:types` property entry's attribute fields into [`Constraints`].
///
/// Engine semantics: boolean keys (`mandatory`, `notNull`,
/// `readOnly`, `external`) are present **only when true** — absent == false.
/// `min`/`max`/`regexp` are strings. `default` is the resolved value, or the
/// `<DEFAULT_NOT_SET>` sentinel, or absent — all three collapse to
/// `Option`.
fn parse_property_constraints(p: &Value) -> Constraints {
    let bool_attr = |key: &str| p.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    let string_attr = |key: &str| p.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let default = match p.get("default") {
        // The sentinel the engine emits when no default was ever set — same as absent.
        Some(Value::String(s)) if s == "<DEFAULT_NOT_SET>" => None,
        // `default null` removes the key entirely.
        None => None,
        Some(other) => Some(DefaultExpr::new(&json_scalar_string(other))),
    };

    Constraints {
        mandatory: bool_attr("mandatory"),
        not_null: bool_attr("notNull"),
        readonly: bool_attr("readOnly"),
        external: bool_attr("external"),
        min: string_attr("min"),
        max: string_attr("max"),
        regexp: string_attr("regexp"),
        default,
    }
}

/// Render a non-string JSON default value as text (numbers, booleans, ...).
/// Only presence matters downstream — this text is never re-emitted.
fn json_scalar_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Extract the column list from an introspected index's `properties` field.
///
/// `schema:types` represents an index's columns as a list of lists (e.g.
/// `[["tag_tokens", "tag_weights"]]` for a compound index). We take the first
/// inner list — the index-on-multiple-columns case — and flatten it.
fn index_columns(idx: &Value) -> Vec<String> {
    let Some(props) = idx.get("properties").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    // Two shapes observed: [["a","b"]] (list of lists) or ["a","b"] (list of
    // strings). Handle both; entries that are neither are skipped.
    props
        .iter()
        .flat_map(|entry| match entry {
            Value::Array(inner) => inner.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            other => other.as_str().into_iter().collect::<Vec<_>>(),
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parent_types_become_extends() {
        let row = json!({
            "name": "Employee",
            "type": "document",
            "parentTypes": ["Person", "User"],
            "properties": [],
            "indexes": [],
        });
        let ty = parse_type_row(&row)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        assert_eq!(ty.extends, vec!["Person".to_string(), "User".to_string()]);
    }

    #[test]
    fn list_of_string_composed_from_type_and_oftype() {
        let row = json!({
            "name": "T",
            "type": "document",
            "properties": [{ "name": "tags", "type": "LIST", "ofType": "STRING" }],
            "indexes": [],
        });
        let ty = parse_type_row(&row)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        assert_eq!(ty.properties["tags"].type_name, "LIST OF STRING");
    }

    #[test]
    fn flat_types_pass_through_uppercased() {
        let row = json!({
            "name": "T",
            "type": "document",
            "properties": [
                { "name": "ai", "type": "ARRAY_OF_INTEGERS" },
                { "name": "m", "type": "MAP" },
            ],
            "indexes": [],
        });
        let ty = parse_type_row(&row)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        assert_eq!(ty.properties["ai"].type_name, "ARRAY_OF_INTEGERS");
        assert_eq!(ty.properties["m"].type_name, "MAP");
    }

    #[test]
    fn constraints_read_only_when_true() {
        let row = json!({
            "name": "T",
            "type": "document",
            "properties": [{
                "name": "x",
                "type": "STRING",
                "mandatory": true,
                "notNull": true,
                "readOnly": true,
                "external": true,
                "min": "1",
                "max": "200",
                "regexp": "[A-Za-z ]+",
            }],
            "indexes": [],
        });
        let ty = parse_type_row(&row)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        let c = &ty.properties["x"].constraints;
        assert!(c.mandatory && c.not_null && c.readonly && c.external);
        assert_eq!(c.min.as_deref(), Some("1"));
        assert_eq!(c.max.as_deref(), Some("200"));
        assert_eq!(c.regexp.as_deref(), Some("[A-Za-z ]+"));
    }

    #[test]
    fn absent_booleans_are_false_and_min_max_default_none() {
        let row = json!({
            "name": "T",
            "type": "document",
            "properties": [{ "name": "x", "type": "STRING" }],
            "indexes": [],
        });
        let ty = parse_type_row(&row)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        let c = &ty.properties["x"].constraints;
        assert!(!c.mandatory && !c.not_null && !c.readonly && !c.external);
        assert_eq!(c.min, None);
        assert_eq!(c.max, None);
        assert!(c.default.is_none(), "no default key → no default");
    }

    #[test]
    fn default_sentinel_and_missing_both_mean_none() {
        let sentinel = json!({
            "name": "T", "type": "document",
            "properties": [{ "name": "a", "type": "STRING", "default": "<DEFAULT_NOT_SET>" }],
            "indexes": [],
        });
        let ty = parse_type_row(&sentinel)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        assert!(
            ty.properties["a"].constraints.default.is_none(),
            "sentinel must be None"
        );

        let resolved = json!({
            "name": "T", "type": "document",
            "properties": [{ "name": "b", "type": "DATETIME", "default": "2026-08-09T19:34:04.747+00:00" }],
            "indexes": [],
        });
        let ty = parse_type_row(&resolved)
            .unwrap()
            .unwrap_or_else(|| panic!("row should be representable"));
        assert!(
            ty.properties["b"].constraints.default.is_some(),
            "resolved value must be Some"
        );
    }

    #[test]
    fn system_roots_are_v_and_e() {
        // ArcadeDB's two system root types. fetch_actual filters these out of
        // introspection so the diff never proposes DROP TYPE V/E.
        assert_eq!(SYSTEM_TYPE_ROOTS, ["V", "E"]);
    }

    #[test]
    fn timeseries_rows_parse_into_first_class_specs() {
        // TimeSeries row shape: kind "t" + dedicated fields.
        let ts_row = json!({
            "name": "SensorReading",
            "type": "t",
            "timestampColumn": "ts",
            "shardCount": 2,
            "retentionMs": 18_144_000_000i64,
            "compactionBucketIntervalMs": 0i64,
            "properties": [
                { "name": "ts", "type": "LONG" },
                { "name": "sensor_id", "type": "LONG" },
                { "name": "temperature", "type": "INTEGER" },
            ],
            "indexes": [],
            "tsColumns": [
                { "name": "ts", "dataType": "LONG", "role": "TIMESTAMP" },
                { "name": "sensor_id", "dataType": "LONG", "role": "TAG" },
                { "name": "temperature", "dataType": "INTEGER", "role": "FIELD" },
            ],
        });
        let ty = parse_type_row(&ts_row)
            .unwrap()
            .expect("ts type is representable");
        assert_eq!(ty.kind, TypeKind::Timeseries);
        let spec = ty.timeseries.expect("spec present");
        assert_eq!(spec.timestamp_column, "ts");
        assert_eq!(spec.shards, Some(2));
        assert_eq!(spec.retention_ms(), Some(18_144_000_000));
        // Compaction 0ms = engine default → normalized to None-equivalent text
        // is NOT authored; introspection keeps it as "0 MS" only when >0, else
        // leaves None.
        assert_eq!(spec.compaction_interval, None);
        // Columns flattened into ordinary properties too.
        assert!(ty.properties.contains_key("sensor_id"));
    }

    #[test]
    fn unknown_kind_rows_are_skippable_not_fatal() {
        // Genuinely unknown kinds (future engine features) must parse to None
        // (skipped by fetch_actual with a warning), never error — or every
        // sync against such a database would break. Malformed rows still
        // error (missing name is a real problem).
        assert!(
            parse_type_row(&json!({ "name": "x", "type": "weird_kind" }))
                .unwrap()
                .is_none()
        );
        assert!(parse_type_row(&json!({ "type": "document" })).is_err());
    }
}
