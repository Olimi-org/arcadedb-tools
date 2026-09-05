//! Typed record decoding — the runtime behind [`crate::RecordDecode`].
//!
//! Three pieces:
//!
//! 1. [`FromGrpcValue`] — a conversion trait from a single [`GrpcValue`],
//!    implemented for the primitives, containers (`Option`, `Vec`, maps), and
//!    any struct deriving [`crate::RecordDecode`]. App-specific types plug in
//!    either by implementing this trait or via the derive's
//!    `#[record(with = "...")]` attribute (which references a plain
//!    `fn(&GrpcValue) -> Result<T, RecordDecodeError>`).
//! 2. [`RecordDecodeError`] — per-property decode errors carrying the property
//!    path (`tags[2]`, `speech_prefix.lines[3]`) and the expected vs actual
//!    wire kind.
//! 3. The property-access helpers the derive's generated code calls into
//! (`get`, `get_opt`, `get_or`, `get_with`, `get_with_or`, `get_opt_with`,
//! and the `bytes` family for `Vec<u8>` blobs).

mod types;

use std::collections::HashMap;
use std::fmt;

use crate::proto::com::arcadedb::grpc::{grpc_value::Kind, GrpcMap, GrpcRecord, GrpcValue};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// A decode failure for one property. Carries the property path (dotted for
/// nested records, `list[3]` for elements) so errors read like
/// `property `speech_prefix.plain_text`: expected string, got embedded`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordDecodeError {
    /// The property is absent and the field is neither `Option` nor defaulted.
    MissingField {
        /// Dotted/braced path of the missing property.
        path: String,
    },
    /// The value kind didn't match the target type (or the payload couldn't
    /// convert, e.g. an out-of-range integer).
    TypeMismatch {
        /// Property path; empty when raised deep inside a list/embed.
        path: String,
        /// Human-readable target type, e.g. `"i32"`.
        expected: &'static str,
        /// Human-readable wire kind, e.g. `"int64"`.
        actual: &'static str,
    },
    /// Anything else — unknown enum variant, custom `with`/trait conversions,
    /// invalid payload inside an otherwise-matching kind.
    Conversion { message: String },
}

impl RecordDecodeError {
    /// Missing-property error for a `&'static str` property name (codegen).
    pub fn missing_field(field: &str) -> Self {
        Self::MissingField {
            path: field.to_string(),
        }
    }

    /// Kind mismatch without a field context (nested conversions).
    pub fn type_mismatch(expected: &'static str, actual: &'static str) -> Self {
        Self::TypeMismatch {
            path: String::new(),
            expected,
            actual,
        }
    }

    /// Free-form conversion error.
    pub fn conversion(message: impl Into<String>) -> Self {
        Self::Conversion {
            message: message.into(),
        }
    }

    /// Unknown enum variant string.
    pub fn unknown_variant(variant: &str) -> Self {
        Self::conversion(format!("unknown variant `{variant}`"))
    }

    /// Prepend a property path segment to this error (used by the generated
    /// code and the `get_*` helpers to thread field context upward).
    pub fn with_field(self, field: &str) -> Self {
        match self {
            RecordDecodeError::MissingField { mut path } => {
                if path.is_empty() {
                    path = field.to_string();
                } else {
                    path = format!("{field}.{path}");
                }
                Self::MissingField { path }
            }
            RecordDecodeError::TypeMismatch {
                mut path,
                expected,
                actual,
            } => {
                if path.is_empty() {
                    path = field.to_string();
                } else {
                    path = format!("{field}.{path}");
                }
                Self::TypeMismatch {
                    path,
                    expected,
                    actual,
                }
            }
            RecordDecodeError::Conversion { message } => Self::Conversion {
                message: format!("field `{field}`: {message}"),
            },
        }
    }
}

impl fmt::Display for RecordDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordDecodeError::MissingField { path } => {
                write!(f, "missing property `{path}`")
            }
            RecordDecodeError::TypeMismatch {
                path,
                expected,
                actual,
            } => {
                if path.is_empty() {
                    write!(f, "expected {expected}, got {actual}")
                } else {
                    write!(f, "property `{path}`: expected {expected}, got {actual}")
                }
            }
            RecordDecodeError::Conversion { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for RecordDecodeError {}

/// Human name for a `GrpcValue` wire kind (or `null` when the oneof is unset).
pub fn kind_name(kind: &Option<Kind>) -> &'static str {
    match kind {
        None => "null",
        Some(Kind::BoolValue(_)) => "bool",
        Some(Kind::Int32Value(_)) => "int32",
        Some(Kind::Int64Value(_)) => "int64",
        Some(Kind::FloatValue(_)) => "float",
        Some(Kind::DoubleValue(_)) => "double",
        Some(Kind::StringValue(_)) => "string",
        Some(Kind::BytesValue(_)) => "bytes",
        Some(Kind::TimestampValue(_)) => "timestamp",
        Some(Kind::ListValue(_)) => "list",
        Some(Kind::MapValue(_)) => "map",
        Some(Kind::EmbeddedValue(_)) => "embedded",
        Some(Kind::LinkValue(_)) => "link",
        Some(Kind::DecimalValue(_)) => "decimal",
    }
}

// ---------------------------------------------------------------------------
// Link — the graph-native record id
// ---------------------------------------------------------------------------

/// A record identifier (`#bucket:pos`) — the typed form of an ArcadeDB link,
/// covering a record's own `@rid` and edge endpoints (`@in`/`@out`).
///
/// Decodes from `LinkValue` (LINK columns, `expand()` projections) and from
/// rid-shaped strings (`SELECT @out`); encodes as a typed `LinkValue`. Fields
/// named `@rid` fall back to the record's own metadata via the derive's synth
/// path, exactly like string rid fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Link {
    /// The bucket id (before the colon).
    pub bucket: i32,
    /// The position within the bucket (after the colon).
    pub pos: i64,
}

impl Link {
    pub fn new(bucket: i32, pos: i64) -> Self {
        Self { bucket, pos }
    }

    /// Parse `#bucket:pos`. Both parts are non-negative; the leading `#` is
    /// required.
    pub fn parse(s: &str) -> Result<Self, RecordDecodeError> {
        let invalid = || RecordDecodeError::conversion(format!("invalid rid `{s}`"));
        let (bucket, pos) = s
            .strip_prefix('#')
            .and_then(|body| body.split_once(':'))
            .ok_or_else(invalid)?;
        let bucket = bucket.parse::<i32>().map_err(|_| invalid())?;
        let pos = pos.parse::<i64>().map_err(|_| invalid())?;
        if bucket < 0 || pos < 0 {
            return Err(invalid());
        }
        Ok(Self { bucket, pos })
    }

    /// The canonical rid string, `#bucket:pos`.
    pub fn to_rid_string(&self) -> String {
        format!("#{}:{}", self.bucket, self.pos)
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Write directly — the old `to_rid_string()` here allocated per
        // formatting, and Display rides every rid in bulk paths.
        write!(f, "#{}:{}", self.bucket, self.pos)
    }
}

impl std::str::FromStr for Link {
    type Err = RecordDecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(feature = "serde")]
mod link_serde {
    use super::*;

    /// Serializes as the rid string (`"#bucket:pos"`) — the JSON-facing form
    /// consumers already treat as an opaque handle.
    impl serde::Serialize for Link {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&self.to_rid_string())
        }
    }

    impl<'de> serde::Deserialize<'de> for Link {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let s = String::deserialize(deserializer)?;
            Self::parse(&s).map_err(serde::de::Error::custom)
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion trait
// ---------------------------------------------------------------------------

/// Convert a single [`GrpcValue`] into `Self`, borrowing from the value when
/// the target type is a borrowed view (`&'de str`, `Cow<'de, str>`,
/// `&'de [u8]`).
///
/// Implemented by the primitives, the containers (`Option`, `Vec`, maps),
/// `serde_json::Value`, and every struct/enum deriving [`crate::RecordDecode`].
/// External identity types (opaque string ids, …) that shouldn't be
/// coupled to this crate implement this trait once and reuse it everywhere, or
/// use the `#[record(with = "...")]` field attribute per declaration.
pub trait FromGrpcValue<'de>: Sized {
    fn from_grpc_value(value: &'de GrpcValue) -> Result<Self, RecordDecodeError>;
}

// ---------------------------------------------------------------------------
// Encoding trait
// ---------------------------------------------------------------------------

/// Encode a typed DTO into a wire [`GrpcRecord`] for writes (`BulkInsert`,
/// `UPDATE` payloads) — the encode-side mirror of [`FromGrpcValue`].
///
/// Implemented by deriving [`crate::RecordEncode`] (which maps each field's
/// Rust type onto a `GrpcValue` wire kind), and by hand for small value types
/// that need a specific wire shape. The defaulted methods build on
/// [`Self::props`] lest custom impls repeat the record-construction code;
/// override [`Self::to_grpc_value`] when the nested form should not be a map
/// (e.g. a scalar wrapper used as a field value).
pub trait ToGrpcRecord {
    /// The bulk-conflict identity: wire names of the fields marked
    /// `#[record(key)]` (empty when none — value-shaped or append-only
    /// types). Consumed by the typed bulk helpers so call sites cannot
    /// disagree with the DTO about the key.
    const KEY_COLUMNS: &'static [&'static str] = &[];

    /// Field name → wire value pairs: the record's property map. Fields
    /// skipped for encoding (`Option::None`, `skip`/`skip_serializing_if`)
    /// are absent.
    fn props(&self) -> Vec<(&str, GrpcValue)>;

    /// The whole record, tagged with the target class (`type` on the wire).
    fn to_grpc_record(&self, class: &str) -> GrpcRecord {
        crate::rec(class, self.props())
    }

    /// The type as a nested value: a map of its properties (ArcadeDB's
    /// `embedded`/`map` shapes). Nested derived structs fall back to this
    /// automatically.
    fn to_grpc_value(&self) -> GrpcValue {
        GrpcValue {
            kind: Some(Kind::MapValue(GrpcMap {
                entries: self
                    .props()
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            })),
            logical_type: String::new(),
        }
    }

    /// The properties as a `name → value` map for SQL `:param` binding
    /// (`query`/`query_with_values`), where the *record* shape doesn't apply —
    /// `Option::None` fields stay absent, exactly like `props()`.
    fn to_param_map(&self) -> HashMap<String, GrpcValue> {
        self.props()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    /// Wire names of the columns that differ between two snapshots of the
    /// same record (`self` = old, `other` = new). The derive compares Rust
    /// field values directly — no wire encoding, and allocation proportional
    /// to the number of CHANGED columns (identical snapshots allocate
    /// nothing); this default (for hand impls) diffs the encoded `props()`
    /// of both sides.
    ///
    /// Semantics match [`props_diff`]: an `Option` going `Some → None` is not
    /// SET-expressible and is never reported; fields the NEW snapshot omits
    /// (`skip_serializing_if`) are never reported. Derived structs require
    /// `PartialEq` field types — wrap exotic ones in `#[record(with = "...")]`
    /// (compared via their encoded value) if needed.
    fn changed_wires(&self, other: &Self) -> Vec<String> {
        props_diff(self, other)
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    /// The by-value encode path: consume the DTO, MOVING owned payloads
    /// (`String`/`Vec<u8>`/`Vec<String>` fields) into the wire record instead
    /// of cloning them. The derive overrides this with a moving
    /// implementation; this default falls back to the borrowing
    /// [`to_grpc_record`](Self::to_grpc_record) (hand impls clone, as before).
    /// Use it wherever the row is owned and spent — bulk writers in
    /// particular (`bulk_upsert_dtos` already does).
    fn into_grpc_record(self, class: &str) -> GrpcRecord
    where
        Self: Sized,
    {
        self.to_grpc_record(class)
    }
}

// ---------------------------------------------------------------------------
// Diff-based partial updates (built on `props()`)
// ---------------------------------------------------------------------------

/// The columns whose encoded value differs between two snapshots of the same
/// record — old-first: a column present in `new` but absent from `old`
/// (e.g. an `Option` that became `Some`) counts as changed; a column absent
/// from `new` (an `Option` that became `None`) cannot be expressed as a SET
/// and is omitted. Re-scrapes that change nothing produce an empty diff and
/// should skip the write entirely.
pub fn props_diff<T: ToGrpcRecord + ?Sized>(old: &T, new: &T) -> Vec<(String, GrpcValue)> {
    let old_props_vec = old.props();
    let old_props: HashMap<&str, &GrpcValue> = old_props_vec.iter().map(|(k, v)| (*k, v)).collect();
    new.props()
        .into_iter()
        .filter(|(k, v)| old_props.get(*k) != Some(&v))
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// [`props_diff`] as a `:param` map — bind it after `SET`-ing exactly the
/// changed columns (build the SET list from the map's keys).
pub fn changed_param_map<T: ToGrpcRecord + ?Sized>(old: &T, new: &T) -> HashMap<String, GrpcValue> {
    props_diff(old, new).into_iter().collect()
}

/// Decode a struct directly from a property MAP — the zero-copy flatten
/// path. [`FromGrpcValue`] necessarily goes through an owned
/// [`GrpcValue`] wrapper (embedded/map), which forces
/// flattening code to clone the remaining entries; this trait lets derived
/// structs decode straight from the record's own map with the parent's
/// consumed keys hidden via `taken` — no rest-map materialization, and
/// borrowed (`Cow<'de>`) flatten targets become possible.
///
/// Implemented by every `#[derive(RecordDecode)]` struct; manual types used
/// as flatten targets implement it via
/// [`from_map_via_value`] (the legacy clone-based path) — see the
/// `impl_decode_from_map_via_value!` macro.
pub trait DecodeFromMap<'de>: Sized {
    /// Decode from `props`, treating every key in `taken` as absent (the
    /// parent flatten scope consumed it). The synth args feed `@rid`/`@type`
    /// fallbacks exactly like the record-level path.
    fn from_map(
        props: &'de HashMap<String, GrpcValue>,
        taken: &[&str],
        syn_rid: Option<&'de String>,
        syn_type: Option<&'de String>,
    ) -> Result<Self, RecordDecodeError>;
}

/// The clone-based escape hatch for manual [`FromGrpcValue`] types used as
/// flatten targets: materializes the (taken-filtered) rest as an owned
/// embedded value and decodes through `FromGrpcValue` — the pre-zero-copy
/// behavior. Prefer deriving `RecordDecode` for flatten targets. The rest is
/// OWNED here, so the decode lifetime is independent of the record's.
pub fn from_map_via_value<T>(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
) -> Result<T, RecordDecodeError>
where
    T: for<'a> FromGrpcValue<'a>,
{
    let rest: HashMap<String, GrpcValue> = props
        .iter()
        .filter(|(k, _)| !taken.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let gv = crate::GrpcValue {
        kind: Some(Kind::EmbeddedValue(crate::GrpcEmbedded {
            fields: rest,
            r#type: String::new(),
        })),
        logical_type: String::new(),
    };
    T::from_grpc_value(&gv)
}

// ---------------------------------------------------------------------------
// Property-access helpers (used by the derive's generated code)
// ---------------------------------------------------------------------------

/// The value for `key`, unless a parent flatten scope already consumed it
/// (`taken`). All decode getters consult this — it is what makes flatten
/// zero-copy: instead of cloning a filtered rest-map, the parent's taken
/// keys are simply invisible to the flattened child.
#[doc(hidden)]
pub fn visible<'a>(
    props: &'a HashMap<String, GrpcValue>,
    key: &str,
    taken: &[&str],
) -> Option<&'a GrpcValue> {
    if taken.contains(&key) {
        None
    } else {
        props.get(key)
    }
}

/// Decode a required property. Absent property → `MissingField` error.
pub fn get<'de, T: FromGrpcValue<'de>>(
    props: &'de HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
) -> Result<T, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => T::from_grpc_value(v).map_err(|e| e.with_field(key)),
        None => Err(RecordDecodeError::missing_field(key)),
    }
}

/// Decode an optional property: absent or explicitly null → `None`.
pub fn get_opt<'de, T: FromGrpcValue<'de>>(
    props: &'de HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
) -> Result<Option<T>, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => Option::<T>::from_grpc_value(v).map_err(|e| e.with_field(key)),
        None => Ok(None),
    }
}

/// Decode a property, falling back to a default-producing closure when absent.
pub fn get_or<'de, T: FromGrpcValue<'de>, F>(
    props: &'de HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
    def: F,
) -> Result<T, RecordDecodeError>
where
    F: FnOnce() -> T,
{
    match visible(props, key, taken) {
        Some(v) => T::from_grpc_value(v).map_err(|e| e.with_field(key)),
        None => Ok(def()),
    }
}

/// Decode a required property through a custom conversion function
/// (`#[record(with = "...")]`).
pub fn get_with<T>(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
    f: fn(&GrpcValue) -> Result<T, RecordDecodeError>,
) -> Result<T, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => f(v).map_err(|e| e.with_field(key)),
        None => Err(RecordDecodeError::missing_field(key)),
    }
}

/// Like [`get_with`], but absent property → default-producing closure
/// (the `#[serde(default)]` + `#[record(with = "...")]` combination). The
/// custom fn must still tolerate an explicit null if the server sends one.
pub fn get_with_or<T>(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
    f: fn(&GrpcValue) -> Result<T, RecordDecodeError>,
    def: impl FnOnce() -> T,
) -> Result<T, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => f(v).map_err(|e| e.with_field(key)),
        None => Ok(def()),
    }
}

/// Like [`get_with`], but absent or explicitly null → `None`.
pub fn get_opt_with<T>(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
    f: fn(&GrpcValue) -> Result<T, RecordDecodeError>,
) -> Result<Option<T>, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => match &v.kind {
            None => Ok(None),
            Some(_) => f(v).map(Some).map_err(|e| e.with_field(key)),
        },
        None => Ok(None),
    }
}

/// Decode a required `Vec<u8>` blob property.
///
/// The wire `bytes` kind maps directly to the blob; a list of small integers
/// also decodes (both were accepted historically).
/// Emitted by the derive for `Vec<u8>` fields — the generic [`Vec`]
/// impl in `types` only handles list-typed elements.
pub fn get_bytes(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
) -> Result<Vec<u8>, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => decode_bytes(v).map_err(|e| e.with_field(key)),
        None => Err(RecordDecodeError::missing_field(key)),
    }
}

/// Decode an optional byte blob: absent or explicitly null → `None`.
pub fn get_opt_bytes(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
) -> Result<Option<Vec<u8>>, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => match &v.kind {
            None => Ok(None),
            Some(_) => decode_bytes(v).map(Some).map_err(|e| e.with_field(key)),
        },
        None => Ok(None),
    }
}

/// Decode a byte blob, falling back to a default-producing closure when absent.
pub fn get_bytes_or(
    props: &HashMap<String, GrpcValue>,
    taken: &[&str],
    key: &str,
    def: impl FnOnce() -> Vec<u8>,
) -> Result<Vec<u8>, RecordDecodeError> {
    match visible(props, key, taken) {
        Some(v) => decode_bytes(v).map_err(|e| e.with_field(key)),
        None => Ok(def()),
    }
}

// ---------------------------------------------------------------------------
// Typed property access on the record itself
// ---------------------------------------------------------------------------

impl GrpcRecord {
    /// Typed single-property decode — the method form of [`get`], for rows
    /// consumed outside a derived DTO. Borrow-preserving: string/blob views
    /// (`&str`, `Cow`, `&[u8]`) borrow from this record.
    ///
    /// ```ignore
    /// let n: u64 = rec.get("n")?;               // required
    /// let t: Option<&str> = rec.get_opt("title")?; // absent/null → None
    /// let b: bool = rec.get_or("flag", false)?;  // defaulted
    /// ```
    pub fn get<'a, T: FromGrpcValue<'a>>(&'a self, key: &str) -> Result<T, RecordDecodeError> {
        get(&self.properties, &[], key)
    }

    /// Typed optional-property decode (absent or null → `Ok(None)`).
    pub fn get_opt<'a, T: FromGrpcValue<'a>>(
        &'a self,
        key: &str,
    ) -> Result<Option<T>, RecordDecodeError> {
        get_opt(&self.properties, &[], key)
    }

    /// Typed decode with a default for the absent case.
    pub fn get_or<'a, T: FromGrpcValue<'a>>(
        &'a self,
        key: &str,
        default: T,
    ) -> Result<T, RecordDecodeError> {
        get_or(&self.properties, &[], key, || default)
    }
}

/// Convert a `GrpcValue` into raw bytes: `bytes` kind → the blob; `list` of
/// small ints → the concatenated ints (matching the legacy deserializer).
fn decode_bytes(v: &GrpcValue) -> Result<Vec<u8>, RecordDecodeError> {
    match &v.kind {
        Some(Kind::BytesValue(b)) => Ok(b.clone()),
        Some(Kind::ListValue(l)) => {
            l.values
                .iter()
                .map(|item| match &item.kind {
                    Some(Kind::Int32Value(n)) => u8::try_from(*n)
                        .map_err(|_| RecordDecodeError::type_mismatch("u8", "int32")),
                    Some(Kind::Int64Value(n)) => u8::try_from(*n)
                        .map_err(|_| RecordDecodeError::type_mismatch("u8", "int64")),
                    _ => Err(RecordDecodeError::type_mismatch(
                        "u8",
                        kind_name(&item.kind),
                    )),
                })
                .collect()
        }
        _ => Err(RecordDecodeError::type_mismatch(
            "bytes",
            kind_name(&v.kind),
        )),
    }
}
