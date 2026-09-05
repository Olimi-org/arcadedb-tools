//! JSON bridge — `GrpcRecord` → `serde_json::Value`.
//!
//! The typed decode path lives in [`crate::record`]: `#[derive(RecordDecode)]`
//! turns a model struct into `TryFrom<&GrpcRecord>` (+ `FromGrpcValue`), with
//! per-field type coercion over the protobuf tree — no intermediate JSON.
//!
//! This module is kept for callers that need **dynamic** JSON access:
//! `serde_json::Value` fields, migrator tooling, and test helpers.
//! `grpc_record_to_json` injects the `@rid` and `@type` record-intrinsic
//! keys when the record carries them (edge endpoint intrinsics `@in`/`@out`
//! arrive as literal property keys on the wire).

use chrono::DateTime;
use serde_json::{Map, Value};

use crate::proto::com::arcadedb::grpc::{
    grpc_value::Kind, GrpcDecimal, GrpcEmbedded, GrpcLink, GrpcList, GrpcMap, GrpcRecord, GrpcValue,
};

/// Convert a record's properties into a JSON object.
pub fn grpc_record_to_json(r: &GrpcRecord) -> Value {
    let mut map = Map::new();
    for (k, v) in &r.properties {
        map.insert(k.clone(), grpc_value_to_json(v));
    }
    if !r.rid.is_empty() {
        map.insert("@rid".into(), Value::String(r.rid.clone()));
    }
    if !r.r#type.is_empty() {
        map.insert("@type".into(), Value::String(r.r#type.clone()));
    }
    Value::Object(map)
}

/// Convert a single typed value. `None` kind -> null.
pub fn grpc_value_to_json(v: &GrpcValue) -> Value {
    match &v.kind {
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::Int32Value(n)) => Value::from(*n),
        Some(Kind::Int64Value(n)) => Value::from(*n),
        Some(Kind::FloatValue(f)) => Value::from(f64::from(*f)),
        Some(Kind::DoubleValue(d)) => Value::from(*d),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::BytesValue(b)) => Value::Array(b.iter().map(|&x| Value::from(x)).collect()),
        Some(Kind::TimestampValue(t)) => {
            let iso = DateTime::from_timestamp(t.seconds, t.nanos as u32)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into());
            Value::String(iso)
        }
        Some(Kind::ListValue(l)) => list_to_json(l),
        Some(Kind::MapValue(m)) => map_to_json(m),
        Some(Kind::EmbeddedValue(e)) => embedded_to_json(e),
        Some(Kind::LinkValue(l)) => link_to_json(l),
        Some(Kind::DecimalValue(d)) => decimal_to_json(d),
        None => Value::Null,
    }
}

/// Convert a `serde_json` value onto the matching wire kind — the inverse of
/// [`grpc_value_to_json`], enabling dynamic (schemaless) ENCODE paths:
/// `serde_json::Value` DTO fields, API-request payloads bound as `:param`s.
///
/// Mapping: `null` → the unset kind (the wire's null); numbers encode as
/// `int32` when they fit, else `int64`, else `double` (a `u64` beyond
/// `i64::MAX` has no integer kind — it widens); objects → maps, arrays →
/// lists, recursively. Round-trips losslessly with [`grpc_value_to_json`]
/// for every JSON value representable on the wire.
pub fn json_to_grpc_value(v: &Value) -> GrpcValue {
    let kind = match v {
        Value::Null => None,
        Value::Bool(b) => Some(Kind::BoolValue(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(
                    i32::try_from(i)
                        .map(Kind::Int32Value)
                        .unwrap_or(Kind::Int64Value(i)),
                )
            } else if let Some(u) = n.as_u64() {
                // Beyond i64 range — the closest wire kind is a double.
                Some(Kind::DoubleValue(u as f64))
            } else {
                Some(Kind::DoubleValue(n.as_f64().unwrap_or_default()))
            }
        }
        Value::String(s) => Some(Kind::StringValue(s.clone())),
        Value::Array(items) => Some(Kind::ListValue(GrpcList {
            values: items.iter().map(json_to_grpc_value).collect(),
        })),
        Value::Object(map) => Some(Kind::MapValue(GrpcMap {
            entries: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_grpc_value(v)))
                .collect(),
        })),
    };
    GrpcValue {
        kind,
        logical_type: String::new(),
    }
}

fn list_to_json(l: &GrpcList) -> Value {
    Value::Array(l.values.iter().map(grpc_value_to_json).collect())
}

fn map_to_json(m: &GrpcMap) -> Value {
    let mut map = Map::new();
    for (k, v) in &m.entries {
        map.insert(k.clone(), grpc_value_to_json(v));
    }
    Value::Object(map)
}

fn embedded_to_json(e: &GrpcEmbedded) -> Value {
    let mut map = Map::new();
    for (k, v) in &e.fields {
        map.insert(k.clone(), grpc_value_to_json(v));
    }
    Value::Object(map)
}

fn link_to_json(l: &GrpcLink) -> Value {
    Value::String(l.rid.clone())
}

fn decimal_to_json(d: &GrpcDecimal) -> Value {
    if d.scale == 0 && d.unscaled_bytes.is_empty() {
        Value::from(d.unscaled)
    } else {
        Value::String(format!("{}e-{}", d.unscaled, d.scale))
    }
}
