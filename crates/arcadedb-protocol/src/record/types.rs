//! The dedicated per-type [`FromGrpcValue`] implementations ("type handlers"):
//! primitives, temporal types, and the container shapes. Kept separate from
//! `super` so the trait/error/codegen surface stays reviewable.
//!
//! Kind-acceptance follows the wire values ArcadeDB actually sends, with two
//! deliberate behaviors: integer narrowing is range-checked (no silent `as`
//! truncation) and floats accept integer kinds (ArcadeDB returns ints for
//! float-ish columns depending on the operator — e.g. `$score` in hybrid
//! search).
//!
//! The `'de` lifetime threads the wire borrow: `&'de str`, `Cow<'de, str>` and
//! `&'de [u8]` targets take views into the decoded [`GrpcValue`]'s own
//! allocations instead of cloning them; owned targets (`String`) clone once.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use prost_types::Timestamp;
use serde_json::Value as JsonValue;

use crate::decode::grpc_value_to_json;
use crate::proto::com::arcadedb::grpc::{grpc_value::Kind, GrpcRecord, GrpcValue};

use super::{kind_name, FromGrpcValue, Link, RecordDecodeError};

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

impl<'de> FromGrpcValue<'de> for String {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::StringValue(s)) => Ok(s.clone()),
            // A record LINK (`#bucket:pos`) is a string on the wire; keep it
            // string-accessible for projections that select linked RIDs.
            Some(Kind::LinkValue(l)) => Ok(l.rid.clone()),
            // Timestamps decode to the ISO-8601/RFC-3339 form (the legacy
            // deserializer's string path did the same).
            Some(Kind::TimestampValue(t)) => Ok(timestamp_to_string(t)),
            // Non-trivial decimals surface as the literal `<unscaled>e-<scale>`
            // decimal string; scale-0 byte-free decimals are integers instead
            // (the int impls below accept them).
            Some(Kind::DecimalValue(d)) if d.scale != 0 || !d.unscaled_bytes.is_empty() => {
                Ok(format!("{}e-{}", d.unscaled, d.scale))
            }
            _ => Err(RecordDecodeError::type_mismatch(
                "string",
                kind_name(&v.kind),
            )),
        }
    }
}

/// Zero-copy view: borrows the wire `String` directly — no allocation.
/// Timestamps and decimals cannot be borrowed (their string form is
/// synthesized), so a `&'de str` target rejects those kinds.
impl<'de> FromGrpcValue<'de> for &'de str {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::StringValue(s)) => Ok(s.as_str()),
            Some(Kind::LinkValue(l)) => Ok(l.rid.as_str()),
            _ => Err(RecordDecodeError::type_mismatch(
                "string",
                kind_name(&v.kind),
            )),
        }
    }
}

/// Borrowed when the wire kind is directly a string; owned when the string
/// form must be synthesized (timestamp, decimal).
impl<'de> FromGrpcValue<'de> for Cow<'de, str> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::StringValue(s)) => Ok(Cow::Borrowed(s.as_str())),
            Some(Kind::LinkValue(l)) => Ok(Cow::Borrowed(l.rid.as_str())),
            Some(Kind::TimestampValue(t)) => Ok(Cow::Owned(timestamp_to_string(t))),
            Some(Kind::DecimalValue(d)) if d.scale != 0 || !d.unscaled_bytes.is_empty() => {
                Ok(Cow::Owned(format!("{}e-{}", d.unscaled, d.scale)))
            }
            _ => Err(RecordDecodeError::type_mismatch(
                "string",
                kind_name(&v.kind),
            )),
        }
    }
}

impl<'de> FromGrpcValue<'de> for char {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::StringValue(s)) if s.chars().count() == 1 => {
                Ok(s.chars().next().expect("chars().count() == 1"))
            }
            _ => Err(RecordDecodeError::type_mismatch(
                "single-char string",
                kind_name(&v.kind),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// The graph-native rid: typed `LinkValue` (LINK columns, `expand()`), or a
/// rid-shaped string (`SELECT @out` projections). Anything else — including
/// non-rid strings — is an error, never a garbage id.
impl<'de> FromGrpcValue<'de> for Link {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::LinkValue(l)) => Link::parse(&l.rid),
            Some(Kind::StringValue(s)) => Link::parse(s),
            _ => Err(RecordDecodeError::type_mismatch("link", kind_name(&v.kind))),
        }
    }
}

// ---------------------------------------------------------------------------
// Bools
// ---------------------------------------------------------------------------

impl<'de> FromGrpcValue<'de> for bool {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::BoolValue(b)) => Ok(*b),
            _ => Err(RecordDecodeError::type_mismatch("bool", kind_name(&v.kind))),
        }
    }
}

// ---------------------------------------------------------------------------
// Integers
// ---------------------------------------------------------------------------

macro_rules! impl_int {
    ($t:ty, $name:literal, $conv:path) => {
        impl<'de> FromGrpcValue<'de> for $t {
            fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
                match &v.kind {
                    Some(Kind::Int32Value(n)) => i64::from(*n).try_into().ok(),
                    Some(Kind::Int64Value(n)) => $conv(*n).ok(),
                    // A scale-0, byte-free decimal is a plain 64-bit integer
                    // on the wire (matches the legacy `any` fallback).
                    Some(Kind::DecimalValue(d)) if d.scale == 0 && d.unscaled_bytes.is_empty() => {
                        $conv(d.unscaled).ok()
                    }
                    _ => None,
                }
                .ok_or_else(|| RecordDecodeError::type_mismatch($name, kind_name(&v.kind)))
            }
        }
    };
}

impl_int!(i8, "i8", i8::try_from);
impl_int!(i16, "i16", i16::try_from);
impl_int!(i32, "i32", i32::try_from);
impl_int!(i64, "i64", i64::try_from);
impl_int!(u8, "u8", u8::try_from);
impl_int!(u16, "u16", u16::try_from);
impl_int!(u32, "u32", u32::try_from);
impl_int!(u64, "u64", u64::try_from);

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

impl<'de> FromGrpcValue<'de> for f32 {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::FloatValue(f)) => Ok(*f),
            Some(Kind::DoubleValue(d)) => Ok(*d as f32),
            // Integers decode to floats like serde's own impls (ArcadeDB
            // returns int-typed values for float-ish columns depending on the
            // operator — e.g. `$score` in hybrid search).
            Some(Kind::Int32Value(n)) => Ok(*n as f32),
            Some(Kind::Int64Value(n)) => Ok(*n as f32),
            _ => Err(RecordDecodeError::type_mismatch("f32", kind_name(&v.kind))),
        }
    }
}

impl<'de> FromGrpcValue<'de> for f64 {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::FloatValue(f)) => Ok(f64::from(*f)),
            Some(Kind::DoubleValue(d)) => Ok(*d),
            // Same integer tolerance as `f32`.
            Some(Kind::Int32Value(n)) => Ok(f64::from(*n)),
            Some(Kind::Int64Value(n)) => Ok(*n as f64),
            _ => Err(RecordDecodeError::type_mismatch("f64", kind_name(&v.kind))),
        }
    }
}

/// `timestamp` kind → RFC-3339 string. Mirrors the legacy deserializer,
/// including its fallback epoch string for out-of-range timestamps.
pub(crate) fn timestamp_to_string(t: &Timestamp) -> String {
    DateTime::from_timestamp(t.seconds, t.nanos as u32)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into())
}

// ---------------------------------------------------------------------------
// Temporal
// ---------------------------------------------------------------------------

impl<'de> FromGrpcValue<'de> for DateTime<Utc> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::TimestampValue(t)) => DateTime::from_timestamp(t.seconds, t.nanos as u32)
                .ok_or_else(|| RecordDecodeError::conversion("timestamp out of range")),
            // A string target is parsed like the legacy `str` path (chrono's
            // visitor) — accepts RFC-3339 text.
            Some(Kind::StringValue(s)) => DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| {
                    RecordDecodeError::conversion(format!("invalid timestamp string `{s}`"))
                }),
            _ => Err(RecordDecodeError::type_mismatch(
                "timestamp",
                kind_name(&v.kind),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

/// Zero-copy view of a `bytes` blob.
impl<'de> FromGrpcValue<'de> for &'de [u8] {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::BytesValue(b)) => Ok(b.as_slice()),
            _ => Err(RecordDecodeError::type_mismatch(
                "bytes",
                kind_name(&v.kind),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

impl<'de, T: FromGrpcValue<'de>> FromGrpcValue<'de> for Option<T> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            // Explicit null from the server → `None`.
            None => Ok(None),
            Some(_) => T::from_grpc_value(v).map(Some),
        }
    }
}

/// List values. Nested derived structs, links, scalars — any `FromGrpcValue`
/// element (including borrowed views like `&'de str`). Byte blobs
/// (`Vec<u8>`) are NOT covered by this blanket impl: the derive emits the
/// `get_bytes`-family helpers, which accept the wire `bytes` kind or a list
/// of small ints; a borrowing slice view is `&'de [u8]` above.
impl<'de, T: FromGrpcValue<'de>> FromGrpcValue<'de> for Vec<T> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        let list = match &v.kind {
            Some(Kind::ListValue(l)) => l,
            _ => return Err(RecordDecodeError::type_mismatch("list", kind_name(&v.kind))),
        };
        list.values
            .iter()
            .enumerate()
            .map(|(i, item)| {
                T::from_grpc_value(item).map_err(|e| {
                    let path = format!("list[{i}]");
                    e.with_field(&path)
                })
            })
            .collect()
    }
}

impl<'de, T: FromGrpcValue<'de>> FromGrpcValue<'de> for HashMap<String, T> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        let map = match &v.kind {
            Some(Kind::MapValue(m)) => m,
            _ => return Err(RecordDecodeError::type_mismatch("map", kind_name(&v.kind))),
        };
        let mut out = HashMap::with_capacity(map.entries.len());
        for (k, entry) in &map.entries {
            let decoded = T::from_grpc_value(entry).map_err(|e| {
                let path = format!("map[{k}]");
                e.with_field(&path)
            })?;
            out.insert(k.clone(), decoded);
        }
        Ok(out)
    }
}

/// Ordered map — same wire shape as [`HashMap`] (`MapValue`, string keys),
/// chosen per-field when deterministic iteration matters.
impl<'de, T: FromGrpcValue<'de>> FromGrpcValue<'de> for BTreeMap<String, T> {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        let map = match &v.kind {
            Some(Kind::MapValue(m)) => m,
            _ => return Err(RecordDecodeError::type_mismatch("map", kind_name(&v.kind))),
        };
        let mut out = BTreeMap::new();
        for (k, entry) in &map.entries {
            let decoded = T::from_grpc_value(entry).map_err(|e| {
                let path = format!("map[{k}]");
                e.with_field(&path)
            })?;
            out.insert(k.clone(), decoded);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Sets
// ---------------------------------------------------------------------------

macro_rules! impl_set {
    ($t:ident, $($bound:tt)*) => {
        impl<'de, T: FromGrpcValue<'de> + $($bound)*> FromGrpcValue<'de>
        for ::std::collections::$t<T>
        {
            fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
                let list = match &v.kind {
                    Some(Kind::ListValue(l)) => l,
                    _ => {
                        return Err(RecordDecodeError::type_mismatch(
                            "list",
                            kind_name(&v.kind),
                        ))
                    }
                };
                // The wire cannot distinguish SET OF from LIST OF (both are
                // `ListValue`); the DTO's container type carries that intent.
                list.values
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        T::from_grpc_value(item).map_err(|e| {
                            let path = format!("set[{i}]");
                            e.with_field(&path)
                        })
                    })
                    .collect()
            }
        }
    };
}

impl_set!(HashSet, Eq + std::hash::Hash);
impl_set!(BTreeSet, Ord);

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// `DATE` columns arrive as midnight-UTC timestamps.
impl<'de> FromGrpcValue<'de> for chrono::NaiveDate {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::TimestampValue(ts)) => {
                chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
                    .map(|dt| dt.date_naive())
                    .ok_or_else(|| {
                        RecordDecodeError::conversion(format!(
                            "timestamp out of range for date: {}",
                            ts.seconds
                        ))
                    })
            }
            _ => Err(RecordDecodeError::type_mismatch("date", kind_name(&v.kind))),
        }
    }
}

// ---------------------------------------------------------------------------
// Exact decimals (opt-in `decimal` feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "decimal")]
mod decimal {
    use super::*;

    /// The wire decimal → `rust_decimal::Decimal`. Accepts the decimal kind
    /// plus scale-0 integers. Values exceeding the 96-bit mantissa or
    /// scale 28 error instead of losing precision.
    impl<'de> FromGrpcValue<'de> for rust_decimal::Decimal {
        fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
            let d = match &v.kind {
                Some(Kind::DecimalValue(d)) => d,
                // Scale-0 byte-free decimal ≡ a plain 64-bit integer.
                Some(Kind::Int64Value(n)) => {
                    return Ok(rust_decimal::Decimal::from(*n));
                }
                _ => {
                    return Err(RecordDecodeError::type_mismatch(
                        "decimal",
                        kind_name(&v.kind),
                    ))
                }
            };

            let unscaled: i128 = if d.unscaled_bytes.is_empty() {
                i128::from(d.unscaled)
            } else {
                // Big-endian two's complement; sign-extend into 16 bytes.
                if d.unscaled_bytes.len() > 16 {
                    return Err(RecordDecodeError::conversion(format!(
                        "decimal unscaled value too wide for rust_decimal: {} bytes",
                        d.unscaled_bytes.len()
                    )));
                }
                let negative = d.unscaled_bytes.first().is_some_and(|b| *b >= 0x80);
                let mut buf = [if negative { 0xFF } else { 0x00 }; 16];
                buf[16 - d.unscaled_bytes.len()..].copy_from_slice(&d.unscaled_bytes);
                i128::from_be_bytes(buf)
            };

            let scale = u32::try_from(d.scale).map_err(|_| {
                RecordDecodeError::conversion(format!("decimal scale out of range: {}", d.scale))
            })?;
            rust_decimal::Decimal::try_from_i128_with_scale(unscaled, scale).map_err(|e| {
                RecordDecodeError::conversion(format!("decimal value not representable: {e}"))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Dynamic
// ---------------------------------------------------------------------------

/// Catch-all: any kind converts to a JSON value (mirrors
/// [`crate::grpc_value_to_json`]). Used for dynamic/schemaless fields.
impl<'de> FromGrpcValue<'de> for JsonValue {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        Ok(grpc_value_to_json(v))
    }
}

/// Whole-record dynamic decode: `serde_json::Value::try_from(&rec)` yields the
/// record's properties as a JSON object (with `@rid`/`@type` when present).
/// The untyped companion to the derived `TryFrom` impls — infallible by
/// construction, so the error type is carried only to satisfy the bound.
impl<'de> TryFrom<&'de GrpcRecord> for JsonValue {
    type Error = RecordDecodeError;

    fn try_from(rec: &'de GrpcRecord) -> Result<Self, Self::Error> {
        let mut map = serde_json::Map::new();
        map.insert("@rid".to_string(), JsonValue::String(rec.rid.clone()));
        map.insert("@type".to_string(), JsonValue::String(rec.r#type.clone()));
        for (k, v) in &rec.properties {
            map.insert(k.clone(), grpc_value_to_json(v));
        }
        Ok(JsonValue::Object(map))
    }
}
