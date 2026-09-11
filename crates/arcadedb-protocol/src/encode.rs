//! Ergonomic constructors for `GrpcRecord` / `GrpcValue` — the encoder side of
//! the [`crate::decode`] bridge. Lets fixture/test code build rows as plain Rust
//! values and bulk-insert them via gRPC, instead of hand-writing SQL or protobuf
//! boilerplate.

#[cfg(feature = "decimal")]
use crate::proto::com::arcadedb::grpc::GrpcDecimal;
use crate::proto::com::arcadedb::grpc::{
    grpc_value::Kind, GrpcEmbedded, GrpcLink, GrpcList, GrpcMap, GrpcRecord, GrpcValue,
};

/// Shorthand for an empty `logical_type` (the common case).
fn gv(kind: Kind) -> GrpcValue {
    GrpcValue {
        kind: Some(kind),
        logical_type: String::new(),
    }
}

pub fn str_v(s: impl Into<String>) -> GrpcValue {
    gv(Kind::StringValue(s.into()))
}
pub fn i64_v(n: i64) -> GrpcValue {
    gv(Kind::Int64Value(n))
}
pub fn i32_v(n: i32) -> GrpcValue {
    gv(Kind::Int32Value(n))
}
pub fn f32_v(n: f32) -> GrpcValue {
    gv(Kind::FloatValue(n))
}
pub fn bool_v(b: bool) -> GrpcValue {
    gv(Kind::BoolValue(b))
}
pub fn f64_v(d: f64) -> GrpcValue {
    gv(Kind::DoubleValue(d))
}
/// A byte blob (the generated proto spells `bytes` as `Vec<u8>`).
pub fn bytes_v(b: impl Into<Vec<u8>>) -> GrpcValue {
    gv(Kind::BytesValue(b.into()))
}
/// An embedded record value.
pub fn embedded_v(type_name: &str, fields: Vec<(&str, GrpcValue)>) -> GrpcValue {
    gv(Kind::EmbeddedValue(GrpcEmbedded {
        r#type: type_name.into(),
        fields: fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }))
}

/// A link to a record by RID (`#bucket:pos`). Used for LINK fields whose
/// target RID was resolved after the target was inserted.
pub fn link_v(rid: impl Into<String>) -> GrpcValue {
    gv(Kind::LinkValue(GrpcLink {
        rid: rid.into(),
        r#type: String::new(),
    }))
}

/// Build a record of `type_name` from `(field, value)` pairs — values go
/// through [`IntoGrpcValue`], same coverage as `params!` (no constructor
/// ceremony at call sites; already-converted `GrpcValue`s pass through).
/// `rid` is left empty (the server assigns it on insert).
pub fn rec<T: IntoGrpcValue>(type_name: &str, props: Vec<(&str, T)>) -> GrpcRecord {
    GrpcRecord {
        rid: String::new(),
        r#type: type_name.to_string(),
        properties: props
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into_value()))
            .collect(),
    }
}

/// A list value from any iterator of already-built values — for
/// heterogeneous/raw list assembly. Typed slices go through
/// [`IntoGrpcValue`] on `Vec<T>` (an always-empty list is
/// `Vec::<GrpcValue>::new().into_value()` via the pass-through impl).
pub fn list_v(values: impl IntoIterator<Item = GrpcValue>) -> GrpcValue {
    gv(Kind::ListValue(GrpcList {
        values: values.into_iter().collect(),
    }))
}

/// A map value from `(key, value)` pairs.
pub fn map_v(entries: impl IntoIterator<Item = (String, GrpcValue)>) -> GrpcValue {
    gv(Kind::MapValue(GrpcMap {
        entries: entries.into_iter().collect(),
    }))
}

/// A timestamp value (`DATETIME` columns). The wire kind is a UTC timestamp;
/// this is also the shape `DATE` columns arrive in (midnight-UTC).
pub fn timestamp_v(t: chrono::DateTime<chrono::Utc>) -> GrpcValue {
    use chrono::Timelike;
    gv(Kind::TimestampValue(prost_types::Timestamp {
        seconds: t.timestamp(),
        nanos: t.nanosecond() as i32,
    }))
}

/// A date value (`DATE` columns) — sent as midnight-UTC on the wire,
/// matching how the server returns them.
pub fn date_v(d: chrono::NaiveDate) -> GrpcValue {
    timestamp_v(d.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc())
}

/// An exact-decimal value (`DECIMAL` columns; `decimal` feature). The
/// unscaled mantissa rides `unscaled` when it fits a signed 64-bit integer,
/// otherwise big-endian two's-complement in `unscaled_bytes`.
#[cfg(feature = "decimal")]
pub fn decimal_v(d: rust_decimal::Decimal) -> GrpcValue {
    let mantissa = d.mantissa();
    let (unscaled, unscaled_bytes) = match i64::try_from(mantissa) {
        Ok(n) => (n, Vec::new()),
        Err(_) => (0, mantissa.to_be_bytes().to_vec()),
    };
    gv(Kind::DecimalValue(GrpcDecimal {
        unscaled,
        scale: d.scale() as i32,
        unscaled_bytes,
    }))
}

/// Bound-parameter map for the client's `*_with_values` / `execute_with_values`
/// methods. Construct it from an array literal (the common case) or wrap an
/// existing map / a RecordEncode DTO's
/// [`ToGrpcRecord::to_param_map`](crate::record::ToGrpcRecord) output.
///
/// ```ignore
    /// client.query_with_values(db, &sql, [("item_id", i64_v(id))])?;
/// client.query_with_values(db, &sql, SearchParams { .. }.to_param_map())?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct Params(pub std::collections::HashMap<String, GrpcValue>);

impl<T: IntoGrpcValue, const N: usize> From<[(&str, T); N]> for Params {
    fn from(pairs: [(&str, T); N]) -> Self {
        Params(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.into_value()))
                .collect(),
        )
    }
}

impl From<std::collections::HashMap<String, GrpcValue>> for Params {
    fn from(map: std::collections::HashMap<String, GrpcValue>) -> Self {
        Params(map)
    }
}

/// The no-parameters form — `query_scalar::<i64>(sql, ())` reads naturally
/// next to param'd calls without reaching for an empty map.
impl From<()> for Params {
    fn from((): ()) -> Self {
        Params(std::collections::HashMap::new())
    }
}

impl core::ops::Deref for Params {
    type Target = std::collections::HashMap<String, GrpcValue>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Type-directed value conversion (`params!`'s discovery mechanism)
// ---------------------------------------------------------------------------

/// The encode-side mirror of [`crate::record::FromGrpcValue`]: Rust values
/// convert to their natural wire kind, so [`params!`](crate::params!) can
/// accept computed expressions and let trait resolution pick the constructor.
///
/// Canonical widths only — bare integer literals bind as `Int64`, float
/// literals as `Double`. Narrower kinds are opt-in through the explicit
/// constructors (`i32_v(..)` etc.), mirroring how `RecordEncode` never
/// silently changes widths.
pub trait IntoGrpcValue {
    fn into_value(self) -> GrpcValue;
}

// Width resolution for integers: `i32` → wire Int32, `i64` → wire Int64 —
// the TYPE disambiguates. Bare integer literals fall back to Rust's
// default (`i32` → Int32): suffix them (`1001i64`) or bind a typed
// value when the column's wire kind matters.

impl IntoGrpcValue for GrpcValue {
    fn into_value(self) -> GrpcValue {
        self
    }
}

impl IntoGrpcValue for bool {
    fn into_value(self) -> GrpcValue {
        bool_v(self)
    }
}

impl IntoGrpcValue for i64 {
    fn into_value(self) -> GrpcValue {
        i64_v(self)
    }
}

impl IntoGrpcValue for f64 {
    fn into_value(self) -> GrpcValue {
        f64_v(self)
    }
}

impl IntoGrpcValue for &str {
    fn into_value(self) -> GrpcValue {
        str_v(self)
    }
}

impl IntoGrpcValue for &String {
    fn into_value(self) -> GrpcValue {
        str_v(self.as_str())
    }
}

impl IntoGrpcValue for String {
    fn into_value(self) -> GrpcValue {
        str_v(self)
    }
}

impl IntoGrpcValue for std::borrow::Cow<'_, str> {
    fn into_value(self) -> GrpcValue {
        str_v(self.into_owned())
    }
}

impl IntoGrpcValue for chrono::DateTime<chrono::Utc> {
    fn into_value(self) -> GrpcValue {
        timestamp_v(self)
    }
}

impl IntoGrpcValue for chrono::NaiveDate {
    fn into_value(self) -> GrpcValue {
        date_v(self)
    }
}

impl<T: IntoGrpcValue> IntoGrpcValue for Vec<T> {
    fn into_value(self) -> GrpcValue {
        list_v(self.into_iter().map(IntoGrpcValue::into_value))
    }
}

/// `i32` binds as the wire `Int32` — the type itself is the disambiguator,
/// matching [`i64`](impl-IntoGrpcValue-for-i64) → `Int64`. No widening, no
/// runtime decision: callers hand over the width they mean.
impl IntoGrpcValue for i32 {
    fn into_value(self) -> GrpcValue {
        i32_v(self)
    }
}

/// String-keyed maps bind as the wire `Map` — the whole map value goes
/// through one `set`/`params!` entry instead of the caller rebuilding it
/// with [`map_v`] item by item.
impl<V: IntoGrpcValue> IntoGrpcValue for std::collections::HashMap<String, V> {
    fn into_value(self) -> GrpcValue {
        map_v(self.into_iter().map(|(k, v)| (k, v.into_value())))
    }
}

impl<V: IntoGrpcValue> IntoGrpcValue for std::collections::BTreeMap<String, V> {
    fn into_value(self) -> GrpcValue {
        map_v(self.into_iter().map(|(k, v)| (k, v.into_value())))
    }
}

/// `char` binds as a single-character string — the symmetric encode of the
/// decode-side `char` impl.
impl IntoGrpcValue for char {
    fn into_value(self) -> GrpcValue {
        str_v(self.to_string())
    }
}

/// Small signed widths bind as the wire `Int32` (lossless: i8/i16 fit).
/// `u8` is deliberately absent — it would collide with `Vec<u8>` = bytes
/// under the blanket `Vec<T>` impl; byte-ish u8s belong in `Vec<u8>`.
impl IntoGrpcValue for i8 {
    fn into_value(self) -> GrpcValue {
        i32_v(i32::from(self))
    }
}

impl IntoGrpcValue for i16 {
    fn into_value(self) -> GrpcValue {
        i32_v(i32::from(self))
    }
}

impl IntoGrpcValue for u16 {
    fn into_value(self) -> GrpcValue {
        i32_v(i32::from(self))
    }
}

/// `u32` binds as the wire `Int64` (u32 does not fit i32).
impl IntoGrpcValue for u32 {
    fn into_value(self) -> GrpcValue {
        i64_v(i64::from(self))
    }
}

/// `u64` binds as `Int64` by cast — values above `i64::MAX` wrap; ArcadeDB
/// has no u64 wire kind, so this mirrors the derive's widening.
impl IntoGrpcValue for u64 {
    fn into_value(self) -> GrpcValue {
        i64_v(self as i64)
    }
}

/// `f32` binds as the wire `Float`.
impl IntoGrpcValue for f32 {
    fn into_value(self) -> GrpcValue {
        f32_v(self)
    }
}

/// Owned bytes bind as the wire `Bytes` — the symmetric encode of the
/// decode-side `&[u8]` view. (Specialized: `u8` has no scalar impl, so this
/// does not overlap the blanket `Vec<T>`.)
impl IntoGrpcValue for Vec<u8> {
    fn into_value(self) -> GrpcValue {
        bytes_v(self)
    }
}

/// JSON binds through the bridge — the symmetric encode of the decode-side
/// `serde_json::Value` impl.
impl IntoGrpcValue for serde_json::Value {
    fn into_value(self) -> GrpcValue {
        crate::decode::json_to_grpc_value(&self)
    }
}

// `Option<T>` is deliberately NOT implemented: the wire `oneof kind` HAS
// NO NULL VARIANT (see the proto — 13 shapes, null is not one of them), so
// there is no null payload an Option could send; an unset kind binds
// nothing and the server skips such params entirely. An Option impl would
// therefore silently drop writes. Absent-or-set semantics live in
// `SetClause::set_opt` (omit) / `set_or_null` (SQL literal) where the
// choice is explicit.

#[cfg(feature = "decimal")]
impl IntoGrpcValue for rust_decimal::Decimal {
    fn into_value(self) -> GrpcValue {
        decimal_v(self)
    }
}

impl IntoGrpcValue for crate::Link {
    fn into_value(self) -> GrpcValue {
        link_v(self.to_rid_string())
    }
}

/// Assemble bound parameters with map semantics — the literal form for the
/// client's `*_with_values` methods.
///
/// Keys are identifiers (they become the SQL `:name`s); values go through
/// [`IntoGrpcValue`] so computed expressions resolve to their natural wire
/// kind at compile time:
///
/// ```ignore
/// use arcadedb_protocol::{params, date_v};
///
/// let min = 0.8f64;
/// client.query_with_values(db, &sql, params! {
///     item_id: 1001i64,                        // literal: suffix picks the width
///     title: "Red Widget",                     // &str → String
///     filter: {                                // nested map → MapValue
///         "genre": "utility",
///         "min_score": min,                    // typed f64 → Double
///     },
///     since: date_v(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap()),
/// })
/// ```
///
/// Already-converted [`GrpcValue`]s pass through untouched, so explicit
/// constructors compose (`code: i32_v(n)`); `Vec<T>` binds as a list.
/// Field punning works too — `item_id,` ≡ `item_id: item_id`.
#[macro_export]
macro_rules! params {
    ($($tokens:tt)+) => {
        $crate::__params_acc!([] $($tokens)+)
    };
}

/// Accumulator backend for [`params!]` — do not use directly.
#[doc(hidden)]
#[macro_export]
macro_rules! __params_acc {
    // Done: build the map from the accumulated `(String, GrpcValue)` pairs.
    ([$($acc:tt)*]) => {
        $crate::Params(::core::iter::FromIterator::from_iter([$($acc)*]))
    };
    // Nested map value, more entries follow.
    ([$($acc:tt)*] $key:ident : { $($mk:tt : $mv:expr),+ $(,)? } , $($rest:tt)+) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::__private::map_v([
                $(($mk.to_string(), $crate::IntoGrpcValue::into_value($mv))),+
            ])),
        ] $($rest)+)
    };
    // Nested map value, last entry.
    ([$($acc:tt)*] $key:ident : { $($mk:tt : $mv:expr),+ $(,)? } $(,)?) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::__private::map_v([
                $(($mk.to_string(), $crate::IntoGrpcValue::into_value($mv))),+
            ])),
        ])
    };
    // Field punning (`key,` ≡ `key: key`), more entries follow.
    ([$($acc:tt)*] $key:ident , $($rest:tt)+) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::IntoGrpcValue::into_value($key)),
        ] $($rest)+)
    };
    // Field punning, last entry.
    ([$($acc:tt)*] $key:ident $(,)?) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::IntoGrpcValue::into_value($key)),
        ])
    };
    // Plain value, more entries follow. The expr fragment parses past commas
    // inside calls/braces; a failed braced-map parse falls through to this arm.
    ([$($acc:tt)*] $key:ident : $value:expr , $($rest:tt)+) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::IntoGrpcValue::into_value($value)),
        ] $($rest)+)
    };
    // Plain value, last entry.
    ([$($acc:tt)*] $key:ident : $value:expr $(,)?) => {
        $crate::__params_acc!([
            $($acc)*
            (::std::stringify!($key).to_string(), $crate::IntoGrpcValue::into_value($value)),
        ])
    };
}

/// Assemble an **embedded record value** — the nestable, value-form sibling
/// of [`rec`]: same `key: value` entry grammar as [`params!`](crate::params!),
/// each field dispatching its own [`IntoGrpcValue`], producing the wire's embedded
/// (`Kind::EmbeddedValue`) shape. Values nest and compose: `vec![embedded!(..),
/// ..]` is a list of embedded values, and a built value rides anywhere a
/// value does (`rec` fields, `params!`, `SetClause`) through the
/// `GrpcValue` pass-through.
///
/// ```
/// # use arcadedb_protocol::{embedded, grpc_value_to_json, rec};
/// let line = embedded!("note_line", author: "A", position: 1, archived: true);
/// // Mixed field types in one invocation, no constructors; lists of
/// // embedded values compose through Vec<GrpcValue>:
/// let row = rec("note", vec![
///     ("entries", vec![
///         line,
///         embedded!("note_line", author: "B", position: 2, archived: false),
///     ]),
/// ]);
/// assert_eq!(
///     grpc_value_to_json(&row.properties["entries"]),
///     serde_json::json!([
///         {"author": "A", "position": 1, "archived": true},
///         {"author": "B", "position": 2, "archived": false},
///     ])
/// );
/// ```
#[macro_export]
macro_rules! embedded {
    ($type:expr $(, $($tokens:tt)+)?) => {
        $crate::__embedded_acc!($type, [] $($($tokens)+)?)
    };
}

/// Accumulator backend for [`embedded!`] — do not use directly.
#[doc(hidden)]
#[macro_export]
macro_rules! __embedded_acc {
    // Done: hand the accumulated (key, value) pairs to the internal
    // constructor (embedded_v stays codegen-internal; the macro IS the
    // public surface).
    ($type:expr, [$($acc:tt)*]) => {
        $crate::__private::embedded_v(
            $type,
            ::core::iter::FromIterator::from_iter([$($acc)*]),
        )
    };
    // Field, more entries follow.
    ($type:expr, [$($acc:tt)*] $key:ident : $value:expr , $($rest:tt)+) => {
        $crate::__embedded_acc!(
            $type, [$($acc)* (::std::stringify!($key), $crate::IntoGrpcValue::into_value($value)),]
            $($rest)+
        )
    };
    // Field, last entry.
    ($type:expr, [$($acc:tt)*] $key:ident : $value:expr $(,)?) => {
        $crate::__embedded_acc!(
            $type, [$($acc)* (::std::stringify!($key), $crate::IntoGrpcValue::into_value($value)),]
        )
    };
}

// ---------------------------------------------------------------------------
// Conditional SET-clause assembly
// ---------------------------------------------------------------------------

/// Accumulates `col = :col` SET fragments and their bound values for
/// `INSERT INTO … SET …` / `UPDATE … SET …` statements whose shape is only
/// known at runtime (optional fields, conditional writes).
///
/// The placeholder name IS the column name, so a fragment can never reference
/// a param that wasn't bound — the silently-nulling placeholder/key mismatch
/// failure mode is unrepresentable by construction.
///
/// ```ignore
/// let clause = SetClause::new()
///     .set("completed_at", now)
///     .set_opt("score", score)          // None → column omitted entirely
///     .set_when(rewrite, "audio_model", model);
/// let sql = format!("UPDATE t SET {} WHERE id = :id", clause.sql());
/// client.execute_with_values(&sql, clause.set("id", id).into_params()).await?;
/// ```
///
/// `None` handling is deliberately *omit, don't bind*: on UPDATE paths the
/// stored value is preserved, on INSERT paths the schema default/null applies.
///
/// List writes: [`append`](SetClause::append) is the
/// server-side append (`col = col || :param`). **Positional element updates
/// have no server-native form**: `SET list[i] = :v` is
/// accepted-but-silently-ignored and `PUT list i :v` is a syntax error —
/// read-modify-write (select the list, patch in Rust,
/// [`set`](SetClause::set) the whole list) remains the shape for
/// those.
#[derive(Debug, Clone, Default)]
pub struct SetClause {
    frags: Vec<String>,
    params: std::collections::HashMap<String, GrpcValue>,
}

impl SetClause {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing was pushed (callers may want to skip the write).
    pub fn is_empty(&self) -> bool {
        self.frags.is_empty()
    }

    /// Always include `col = :col` with the bound value. Build each column
    /// once — re-setting emits the fragment twice (last value wins).
    pub fn set(mut self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.frags.push(format!("{col} = :{col}"));
        self.params.insert(col.to_string(), value.into_value());
        self
    }

    /// [`set`](Self::set) only when `value` is `Some`.
    pub fn set_opt<T: IntoGrpcValue>(self, col: &str, value: Option<T>) -> Self {
        match value {
            Some(v) => self.set(col, v),
            None => self,
        }
    }

    /// [`set`](Self::set) only when `cond` holds.
    pub fn set_when(self, cond: bool, col: &str, value: impl IntoGrpcValue) -> Self {
        if cond {
            self.set(col, value)
        } else {
            self
        }
    }

    /// Escape hatch for fragments that are not a plain `col = :col`.
    /// The fragment must still reference `:name`; `name` is registered
    /// so it can never dangle.
    pub fn set_raw(
        mut self,
        fragment: impl Into<String>,
        name: &str,
        value: impl IntoGrpcValue,
    ) -> Self {
        self.frags.push(fragment.into());
        self.params.insert(name.to_string(), value.into_value());
        self
    }

    /// Set a column to a SERVER-SIDE expression (`count = count + 1`,
    /// `checked_at = sysdate()`) — the increment/anchor forms that have no
    /// client-side value to bind. Both halves are caller-owned SQL: `col`
    /// must be a plain schema identifier (same contract as
    /// [`set`](Self::set), which likewise interpolates it), `expr` is
    /// inserted verbatim — never data.
    pub fn set_expr(mut self, col: &str, expr: &str) -> Self {
        self.frags.push(format!("{col} = {expr}"));
        self
    }

    /// Register a param WITHOUT emitting a fragment — for values referenced
    /// from elsewhere in the statement. Composes with
    /// [`sql`](Self::sql)/[`into_params`](Self::into_params).
    pub fn bind(mut self, name: &str, value: impl IntoGrpcValue) -> Self {
        self.params.insert(name.to_string(), value.into_value());
        self
    }

    /// Apply a multi-call step only when `option` is `Some` — the
    /// conditional-composition form for write shapes that need more than
    /// one builder call (e.g. [`append`](Self::append) + [`bind`](Self::bind)
    /// together). `None` passes the clause through untouched. Read-side
    /// twin: [`FilterClause::apply_if`](crate::FilterClause::apply_if).
    ///
    /// ```
    /// # use arcadedb_protocol::SetClause;
    /// let clause = SetClause::new()
    ///     .apply_if(Some("red"), |c, v| c.append("labels", v))
    ///     .apply_if(None::<String>, |c, v| c.set("note", v));
    /// assert_eq!(clause.sql(), "labels = labels || :labels__append");
    /// ```
    pub fn apply_if<T>(self, option: Option<T>, f: impl FnOnce(Self, T) -> Self) -> Self {
        match option {
            Some(v) => f(self, v),
            None => self,
        }
    }

    /// Server-side list append: emits `col = col || :{col}__append` with the
    /// element bound as a one-element list. One roundtrip, no
    /// read-merge-write window, works on a NULL column
    /// (`null || [x] = [x]`) and preserves element kinds.
    ///
    /// Chained appends on the same column coalesce into ONE fragment + one
    /// merged list param. Appending to a column also plain-`set` in the
    /// same clause is unsupported (build one or the other).
    ///
    /// The idempotent append is one statement — `CONTAINS` accepts the same
    /// single-element list param as a membership test:
    ///
    /// ```
    /// # use arcadedb_protocol::SetClause;
    /// let clause = SetClause::new().append("labels", "red");
    /// assert_eq!(clause.sql(), "labels = labels || :labels__append");
    /// ```
    ///
    /// The update's `affected_records` is the "did it change" bool.
    pub fn append(mut self, col: &str, value: impl IntoGrpcValue) -> Self {
        let param = format!("{col}__append");
        match self.params.get_mut(&param) {
            // Second+ append on the same column: merge into the bound list
            // and keep the single fragment.
            Some(GrpcValue {
                kind: Some(Kind::ListValue(list)),
                ..
            }) => list.values.push(value.into_value()),
            _ => {
                self.frags.push(format!("{col} = {col} || :{param}"));
                self.params.insert(param, list_v([value.into_value()]));
            }
        }
        self
    }

    /// [`set`](Self::set) when `value` is `Some`; **explicitly null the
    /// column** when `None`.
    ///
    /// Unlike [`set_opt`](Self::set_opt) (which omits `None` columns to
    /// preserve stored values), this is the write-what-you-mean form for
    /// refresh paths where an absent source field must CLEAR stale data.
    ///
    /// The `None` arm cannot bind: the wire oneof has no null kind, so an
    /// unset value is simply skipped server-side (no property can be
    /// nulled through a bound parameter) — it falls back to a `col = null`
    /// SQL literal, the only spelling the engine actually honors.
    /// The only interpolated text is the column name (caller-owned schema
    /// identifier, never data).
    pub fn set_or_null<T: IntoGrpcValue>(mut self, col: &str, value: Option<T>) -> Self {
        match value {
            Some(v) => self.set(col, v),
            None => {
                self.frags.push(format!("{col} = null"));
                self
            }
        }
    }

    /// The SET-clause body: `a = :a, b = :b`. Empty string when nothing was
    /// pushed (callers decide whether an empty SET is valid for their shape).
    pub fn sql(&self) -> String {
        self.frags.join(", ")
    }

    /// Consume into `Params` for `query_with_values` / `execute_with_values`.
    pub fn into_params(self) -> Params {
        Params(self.params)
    }
}
