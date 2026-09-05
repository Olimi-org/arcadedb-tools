//! Integration tests for the `RecordEncode` derive + runtime — consume the
//! crate exactly like external users (path dependency), so the generated code
//! resolves the default `::arcadedb_protocol` runtime path. Covers the shapes
//! the consuming projects need for their bulk writers: typed-or-`&str` fields,
//! map-typed metadata, `Option` omission, skip attrs, renames, nested structs,
//! and the `with` escape hatch. The `RecordDecode` round trips pin the encode
//! mapping to the decode side's expectations.

use std::collections::HashMap;

use serde::Deserialize;

use arcadedb_protocol::__private::{
    date_v, f64_v, i32_v, i64_v, list_v, map_v, str_v, timestamp_v,
};
use arcadedb_protocol::proto::com::arcadedb::grpc::{grpc_value::Kind, GrpcValue};
use arcadedb_protocol::record::ToGrpcRecord;
use arcadedb_protocol::{params, Params, RecordDecode, RecordEncode};

// ---------------------------------------------------------------------------
// Test DTOs
// ---------------------------------------------------------------------------

/// A round-trip DTO with mixed int kinds, a `Cow`/`String`-ish field,
/// and a defaulted option.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
#[serde(rename_all = "camelCase")]
struct MetricRow {
    sensor_id: i64,
    sample_day: i32,
    temperature: i32,
    #[serde(default)]
    humidity: Option<i32>,
}

/// A DTO with a map-valued metadata column, borrowing `&str` views
/// (lifetime-generic struct).
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct CandidateRow<'a> {
    collection_slug: &'a str,
    item_id: i64,
    status: &'a str,
    metadata: HashMap<String, String>,
}

/// Skip/omit attrs: `skip` (both namespaces), `skip_serializing`, and
/// `skip_serializing_if`.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct OmitRow {
    visible: i32,
    #[serde(skip)]
    gone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    maybe: Option<i32>,
    #[serde(rename = "logical_key")]
    logical_key: i64,
}

/// Nested `RecordEncode` struct → map value.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct Inner {
    a: i32,
    b: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct Outer {
    name: String,
    inner: Inner,
    #[serde(rename = "tags")]
    label_ids: Vec<i32>,
}

/// Borrowed-view params DTO: `&[T]` slices encode as list values, `&str`
/// as strings — zero copies, zero clones. Write-only (params aren't
/// decoded back), hence `RecordEncode` only.
#[derive(RecordEncode)]
struct SearchParams<'a> {
    #[serde(rename = "qTokens")]
    tokens: &'a [i32],
    #[serde(rename = "qWeights")]
    weights: &'a [f64],
    keywords: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Wire-kind mapping
// ---------------------------------------------------------------------------

#[test]
fn metric_row_maps_fields_to_wire_kinds() {
    let row = MetricRow {
        sensor_id: 1001,
        sample_day: 25_000,
        temperature: 25,
        humidity: Some(40),
    };
    let rec = row.to_grpc_record("sensor_readings");

    assert_eq!(rec.r#type, "sensor_readings");
    let props: HashMap<&str, &GrpcValue> = rec
        .properties
        .iter()
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    assert_eq!(props["sensorId"].kind, Some(Kind::Int64Value(1001)));
    assert_eq!(props["sampleDay"].kind, Some(Kind::Int32Value(25_000)));
    assert_eq!(props["temperature"].kind, Some(Kind::Int32Value(25)));
    assert_eq!(props["humidity"].kind, Some(Kind::Int32Value(40)));
}

#[test]
fn candidate_row_encodes_metadata_as_map() {
    let row = CandidateRow {
        collection_slug: "staff-picks",
        item_id: 1002,
        status: "approved",
        metadata: HashMap::from([("title".to_string(), "Red Widget".to_string())]),
    };
    let rec = row.to_grpc_record("listing");

    assert_eq!(rec.r#type, "listing");
    let Some(Kind::MapValue(map)) = &rec.properties["metadata"].kind else {
        panic!("metadata must be a map value");
    };
    let Some(Kind::StringValue(title)) = &map.entries["title"].kind else {
        panic!("metadata.title must be a string");
    };
    assert_eq!(title, "Red Widget");
}

#[test]
fn option_none_omits_the_property() {
    let row = MetricRow {
        sensor_id: 1,
        sample_day: 1,
        temperature: 0,
        humidity: None,
    };
    let rec = row.to_grpc_record("sensor_readings");
    assert!(
        !rec.properties.contains_key("humidity"),
        "None Option must be omitted, got {:?}",
        rec.properties.keys().collect::<Vec<_>>()
    );
    assert!(rec.properties.contains_key("sensorId"));
}

#[test]
fn skip_attrs_omit_properties() {
    let row = OmitRow {
        visible: 7,
        gone: "shhh".to_string(),
        maybe: None,
        logical_key: 99,
    };
    let rec = row.to_grpc_record("omit");
    let mut keys: Vec<&str> = rec.properties.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["logical_key", "visible"]);
    // The present `maybe` is serialized under `skip_serializing_if` too.
    let row = OmitRow {
        visible: 8,
        gone: String::new(),
        maybe: Some(5),
        logical_key: 100,
    };
    let rec = row.to_grpc_record("omit");
    assert!(rec.properties.contains_key("maybe"));
}

#[test]
fn nested_struct_encodes_as_map() {
    let row = Outer {
        name: "outer".to_string(),
        inner: Inner {
            a: 1,
            b: "bee".to_string(),
        },
        label_ids: vec![100, 200],
    };
    let rec = row.to_grpc_record("outer");

    // Renames apply on the encode side too.
    assert!(rec.properties.contains_key("tags"));
    let Some(Kind::ListValue(tags)) = &rec.properties["tags"].kind else {
        panic!("tags must be a list");
    };
    assert_eq!(tags.values[0].kind, Some(Kind::Int32Value(100)));

    let Some(Kind::MapValue(inner)) = &rec.properties["inner"].kind else {
        panic!("nested struct must be a map value");
    };
    assert_eq!(inner.entries["a"].kind, Some(Kind::Int32Value(1)));
    assert_eq!(
        inner.entries["b"].kind,
        Some(Kind::StringValue("bee".to_string()))
    );
}

#[test]
fn borrowed_slices_encode_as_lists_and_params() {
    let params = SearchParams {
        tokens: &[10, 20, 30],
        weights: &[0.5, 1.0],
        keywords: Some("spire"),
    };
    let map = params.to_param_map();

    assert!(map.contains_key("qTokens"));
    assert!(map.contains_key("qWeights"));
    assert!(map.contains_key("keywords"));

    let Some(Kind::ListValue(qt)) = &map["qTokens"].kind else {
        panic!("qTokens must be a list");
    };
    assert_eq!(
        qt.values,
        vec![
            GrpcValue {
                kind: Some(Kind::Int32Value(10)),
                logical_type: String::new()
            },
            GrpcValue {
                kind: Some(Kind::Int32Value(20)),
                logical_type: String::new()
            },
            GrpcValue {
                kind: Some(Kind::Int32Value(30)),
                logical_type: String::new()
            },
        ]
    );
    let Some(Kind::ListValue(qw)) = &map["qWeights"].kind else {
        panic!("qWeights must be a list");
    };
    assert_eq!(qw.values.len(), 2);
    assert_eq!(qw.values[0].kind, Some(Kind::DoubleValue(0.5)));

    // `Option::None` params stay absent — omitted, not null.
    let empty = SearchParams {
        tokens: &[],
        weights: &[],
        keywords: None,
    };
    let map = empty.to_param_map();
    assert!(!map.contains_key("keywords"));
    assert!(
        map.contains_key("qTokens"),
        "empty list params are still bound (absent is null)"
    );
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn round_trip_metric_row() {
    let row = MetricRow {
        sensor_id: 42,
        sample_day: 25_000,
        temperature: 7,
        humidity: Some(3),
    };
    let rec = row.to_grpc_record("sensor_readings");
    let decoded = MetricRow::try_from(&rec).expect("round-trip decode failed");
    assert_eq!(decoded, row);

    // And the None variant round-trips (optionally-defaulted field).
    let row = MetricRow {
        sensor_id: 42,
        sample_day: 25_000,
        temperature: 7,
        humidity: None,
    };
    let rec = row.to_grpc_record("sensor_readings");
    let decoded = MetricRow::try_from(&rec).expect("round-trip decode failed");
    assert_eq!(decoded, row);
}

#[test]
fn round_trip_candidate_row() {
    let row = CandidateRow {
        collection_slug: "staff-picks",
        item_id: 1001,
        status: "approved",
        metadata: HashMap::from([
            ("title".to_string(), "Red Widget".to_string()),
            ("extra".to_string(), "x".to_string()),
        ]),
    };
    let rec = row.to_grpc_record("listing");
    let decoded = CandidateRow::try_from(&rec).expect("round-trip decode failed");
    assert_eq!(decoded, row);
}

#[test]
fn round_trip_outer_with_nested() {
    let row = Outer {
        name: "n".to_string(),
        inner: Inner {
            a: -3,
            b: "b".to_string(),
        },
        label_ids: vec![1, 2, 3],
    };
    let rec = row.to_grpc_record("outer");
    let decoded = Outer::try_from(&rec).expect("round-trip decode failed");
    assert_eq!(decoded, row);
}

// ---------------------------------------------------------------------------
// Custom types via `with`
// ---------------------------------------------------------------------------

/// A `table:key`-style wrapper the macro must not know about; the encode side
/// plugs it in through a `fn(&T) -> GrpcValue`.
#[derive(Debug, Clone, PartialEq)]
struct Shareable(u64);

fn encode_shareable(value: &Shareable) -> GrpcValue {
    // Shareable links are written as i64 (bounded, like item_ids).
    arcadedb_protocol::__private::i64_v(value.0 as i64)
}

#[derive(Debug, Clone, PartialEq, RecordEncode)]
struct WithRow {
    #[record(with = "encode_shareable")]
    shareable: Shareable,
    normal: i32,
}

#[test]
fn with_hatch_shapes_custom_values() {
    let row = WithRow {
        shareable: Shareable(7),
        normal: 1,
    };
    let rec = row.to_grpc_record("with");
    assert_eq!(rec.properties["shareable"].kind, Some(Kind::Int64Value(7)));
    assert_eq!(rec.properties["normal"].kind, Some(Kind::Int32Value(1)));
}

// ---------------------------------------------------------------------------
// Directional skip spellings — serde parity
// ---------------------------------------------------------------------------

/// serde's skip spellings are directional: `skip_serializing` skips WRITES
/// only (the field still decodes from the wire), `skip_deserializing` skips
/// READS only (the field still encodes). Bare `#[serde(skip)]` skips both.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct DirectionalSkipRow {
    always: String,
    #[serde(skip_serializing)]
    write_skipped: String,
    #[serde(skip_deserializing, default)]
    read_skipped: String,
}

#[test]
fn skip_serializing_still_decodes_and_skip_deserializing_skips_decode() {
    let rec = arcadedb_protocol::GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([
            (
                "always".to_string(),
                arcadedb_protocol::__private::str_v("a"),
            ),
            (
                "write_skipped".to_string(),
                arcadedb_protocol::__private::str_v("w"),
            ),
            (
                "read_skipped".to_string(),
                arcadedb_protocol::__private::str_v("r"),
            ),
        ]),
    };
    let row = DirectionalSkipRow::try_from(&rec).unwrap();
    assert_eq!(row.always, "a");
    assert_eq!(
        row.write_skipped, "w",
        "`skip_serializing` is write-only — the field must still decode"
    );
    assert_eq!(
        row.read_skipped, "",
        "`skip_deserializing` is read-only — the field must decode to its default"
    );
}

// ---------------------------------------------------------------------------
// Borrowed blob fields — `Cow<[u8]>` must encode as bytes, not stringify
// ---------------------------------------------------------------------------

#[derive(RecordEncode)]
struct BorrowedBlobRow<'a> {
    data: std::borrow::Cow<'a, [u8]>,
}

#[test]
fn cow_bytes_encode_as_bytes_kind() {
    let row = BorrowedBlobRow {
        data: std::borrow::Cow::Borrowed(&[1u8, 2, 3][..]),
    };
    let props = row.props();
    assert_eq!(props.len(), 1);
    assert_eq!(
        props[0].1.kind,
        Some(Kind::BytesValue(vec![1, 2, 3])),
        "`Cow<[u8]>` is a byte blob — it must never reach the string arm"
    );
}

// ---------------------------------------------------------------------------
// Temporal fields — chrono `DateTime` maps onto the wire timestamp kind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
struct TimestampedRow {
    created_at: chrono::DateTime<chrono::Utc>,
}

#[test]
fn datetime_round_trips_through_the_timestamp_kind() {
    let row = TimestampedRow {
        created_at: chrono::DateTime::from_timestamp(1_755_000_000, 123_456_789).unwrap(),
    };
    let rec = row.to_grpc_record("timestamped");
    assert_eq!(
        rec.properties["created_at"].kind,
        Some(Kind::TimestampValue(arcadedb_protocol::Timestamp {
            seconds: 1_755_000_000,
            nanos: 123_456_789,
        })),
        "chrono DateTime must encode as the protobuf Timestamp kind"
    );
    let back = TimestampedRow::try_from(&rec).unwrap();
    assert_eq!(back, row);
}

#[test]
fn skip_deserializing_still_encodes_and_skip_serializing_skips_encode() {
    let row = DirectionalSkipRow {
        always: "a".into(),
        write_skipped: "w".into(),
        read_skipped: "r".into(),
    };
    let keys: Vec<&str> = row.props().into_iter().map(|(k, _)| k).collect();
    assert!(keys.contains(&"always"));
    assert!(
        keys.contains(&"read_skipped"),
        "`skip_deserializing` is read-only — the field must still encode"
    );
    assert!(
        !keys.contains(&"write_skipped"),
        "`skip_serializing` is write-only — the field must be omitted at encode"
    );
}
// ---------------------------------------------------------------------------
// KEY_COLUMNS — the DTO declares its bulk-conflict identity
// ---------------------------------------------------------------------------

#[derive(RecordEncode)]
struct KeyedRow {
    #[record(key)]
    item_id: String,
    title: String,
}

#[derive(RecordEncode)]
struct UnkeyedRow {
    title: String,
}

#[test]
fn key_columns_come_from_the_record_key_attribute() {
    use arcadedb_protocol::record::ToGrpcRecord;
    assert_eq!(KeyedRow::KEY_COLUMNS, &["item_id"]);
    assert_eq!(UnkeyedRow::KEY_COLUMNS, <&[&str]>::default());
}

// ---------------------------------------------------------------------------
// Enum encoding — the write mirror of RecordDecode's enum support
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
#[serde(rename_all = "snake_case")]
enum Size {
    Small,
    Large,
}

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode, RecordEncode)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Shape {
    Circle { radius: f64 },
    Rect { w: f64, h: f64 },
}

#[derive(RecordEncode)]
struct SizeCarrier {
    size: Size,
    shape: Option<Shape>,
}

#[test]
fn unit_enum_encodes_as_its_wire_name() {
    let v = Size::Large.to_grpc_value();
    assert_eq!(v.kind, Some(Kind::StringValue("large".into())));
    assert_eq!(
        Size::Small.to_grpc_value().kind,
        Some(Kind::StringValue("small".into()))
    );
}

#[test]
fn tagged_enum_encodes_discriminator_and_fields() {
    let v = Shape::Rect { w: 2.0, h: 3.0 }.to_grpc_value();
    let Some(Kind::MapValue(m)) = &v.kind else {
        panic!("tagged enum must encode as a map");
    };
    let entries: std::collections::HashMap<&str, &Kind> = m
        .entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.kind.as_ref().expect("some kind")))
        .collect();
    assert_eq!(entries["kind"], &Kind::StringValue("rect".into()));
    assert_eq!(entries["w"], &Kind::DoubleValue(2.0));
    assert_eq!(entries["h"], &Kind::DoubleValue(3.0));
}

#[test]
fn enum_fields_encode_inside_structs() {
    let row = SizeCarrier {
        size: Size::Small,
        shape: Some(Shape::Circle { radius: 1.0 }),
    };
    let rec = row.to_grpc_record("carrier");
    assert_eq!(
        rec.properties["size"].kind,
        Some(Kind::StringValue("small".into()))
    );
    let Some(Kind::MapValue(m)) = &rec.properties["shape"].kind else {
        panic!("shape must be a map");
    };
    assert!(m.entries.iter().any(|(k, _)| k == "kind"));

    // None omits the property, as for every Option.
    let row = SizeCarrier {
        size: Size::Large,
        shape: None,
    };
    let rec = row.to_grpc_record("carrier");
    assert!(!rec.properties.contains_key("shape"));
}

#[test]
fn enum_round_trips_with_decode() {
    use arcadedb_protocol::record::FromGrpcValue;
    for size in [Size::Small, Size::Large] {
        let back = Size::from_grpc_value(&size.to_grpc_value()).unwrap();
        assert_eq!(back, size);
    }
    let shape = Shape::Circle { radius: 2.5 };
    let back = Shape::from_grpc_value(&shape.to_grpc_value()).unwrap();
    assert_eq!(back, shape);
}

// ---------------------------------------------------------------------------
// Dynamic values — the json bridge, encode direction
// ---------------------------------------------------------------------------

#[derive(RecordEncode)]
struct MetadataCarrier {
    name: String,
    metadata: HashMap<String, serde_json::Value>,
}

#[test]
fn json_values_encode_onto_wire_kinds() {
    let mut metadata = HashMap::new();
    metadata.insert("count".to_string(), serde_json::json!(3));
    metadata.insert("label".to_string(), serde_json::json!("hi"));
    metadata.insert("flag".to_string(), serde_json::json!(true));
    metadata.insert("nested".to_string(), serde_json::json!({ "a": [1, 2] }));

    let row = MetadataCarrier {
        name: "m".into(),
        metadata,
    };
    let rec = row.to_grpc_record("meta");
    let Some(Kind::MapValue(m)) = &rec.properties["metadata"].kind else {
        panic!("metadata must be a map");
    };
    let entries: HashMap<&str, &Kind> = m
        .entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.kind.as_ref().expect("some kind")))
        .collect();
    assert_eq!(entries["count"], &Kind::Int32Value(3));
    assert_eq!(entries["label"], &Kind::StringValue("hi".into()));
    assert_eq!(entries["flag"], &Kind::BoolValue(true));
    assert!(matches!(entries["nested"], Kind::MapValue(_)));
}

#[test]
fn json_to_grpc_value_inverts_the_bridge() {
    use arcadedb_protocol::{grpc_value_to_json, json_to_grpc_value};
    let cases = [
        serde_json::json!(1),
        serde_json::json!(1.5),
        serde_json::json!("s"),
        serde_json::json!(true),
        serde_json::json!([1, "a"]),
        serde_json::json!({"k": "v"}),
        serde_json::json!(i64::MAX),
    ];
    for case in cases {
        let wire = json_to_grpc_value(&case);
        let back = grpc_value_to_json(&wire);
        assert_eq!(back, case, "round trip must be lossless for {case}");
    }
    // JSON null maps onto the unset kind — the wire's null.
    assert!(json_to_grpc_value(&serde_json::Value::Null).kind.is_none());
}

// ---------------------------------------------------------------------------
// Diff-based partial updates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, RecordEncode)]
struct DiffRow {
    #[record(key)]
    item_id: String,
    title: String,
    score: i32,
    note: Option<String>,
}

#[test]
fn props_diff_reports_only_changed_columns() {
    use arcadedb_protocol::{changed_param_map, props_diff};

    let old = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 5,
        note: Some("x".into()),
    };
    let same = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 5,
        note: Some("x".into()),
    };
    assert!(
        props_diff(&old, &same).is_empty(),
        "identical snapshots → no write"
    );

    let new = DiffRow {
        item_id: "1".into(),
        title: "B".into(),
        score: 5,
        note: None,
    };
    let diff = props_diff(&old, &new);
    // `title` changed; `note` became None (not expressible as a SET — omitted).
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].0, "title");
    assert_eq!(diff[0].1.kind, Some(Kind::StringValue("B".into())));
    assert_eq!(
        changed_param_map(&old, &new).keys().collect::<Vec<_>>(),
        ["title"]
    );

    // An Option going None → Some counts as changed.
    let revived = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 5,
        note: Some("y".into()),
    };
    let diff = props_diff(&old, &revived);
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].0, "note");
}

// ---------------------------------------------------------------------------
// changed_wires — the zero-allocation diff primitive
// ---------------------------------------------------------------------------

#[test]
fn changed_wires_compares_rust_values_directly() {
    use arcadedb_protocol::record::ToGrpcRecord;
    let old = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 5,
        note: Some("x".into()),
    };
    let same = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 5,
        note: Some("x".into()),
    };
    assert!(
        old.changed_wires(&same).is_empty(),
        "identical → no diff, no encoding"
    );

    let new = DiffRow {
        item_id: "1".into(),
        title: "B".into(),
        score: 5,
        note: None,
    };
    // `title` changed; `note` went Some→None (not SET-expressible → omitted,
    // matching `props_diff`).
    assert_eq!(old.changed_wires(&new), vec!["title".to_string()]);

    let revived = DiffRow {
        item_id: "1".into(),
        title: "A".into(),
        score: 6,
        note: Some("y".into()),
    };
    let mut changed = revived.changed_wires(&old);
    changed.sort_unstable();
    assert_eq!(changed, vec!["note".to_string(), "score".to_string()]);
}

// ---------------------------------------------------------------------------
// By-value encode — moved payloads, not cloned
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, RecordEncode)]
struct OwnedRow {
    #[record(key)]
    id: String,
    title: String,
    blob: Vec<u8>,
    flags: Vec<String>,
    note: Option<String>,
    score: i32,
}

#[test]
fn into_grpc_record_moves_payloads_and_matches_the_borrow_path() {
    use arcadedb_protocol::record::ToGrpcRecord;
    let row = OwnedRow {
        id: "k".into(),
        title: "t".into(),
        blob: vec![1, 2, 3],
        flags: vec!["a".into()],
        note: Some("n".into()),
        score: 7,
    };
    let borrowed = row.clone().to_grpc_record("owned");
    let moved = row.into_grpc_record("owned");
    assert_eq!(borrowed.properties.len(), moved.properties.len());
    for (k, v) in &borrowed.properties {
        assert_eq!(
            moved.properties[k].kind, v.kind,
            "column `{k}` must encode identically"
        );
    }
}

// ---------------------------------------------------------------------------
// Link encode — links write back as typed LinkValue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, RecordEncode)]
struct EdgeWriteRow {
    #[serde(rename = "@in")]
    from: arcadedb_protocol::Link,
    #[serde(rename = "@out")]
    to: Option<arcadedb_protocol::Link>,
    related: Vec<arcadedb_protocol::Link>,
}

#[test]
fn link_fields_encode_as_link_kind() {
    let row = EdgeWriteRow {
        from: arcadedb_protocol::Link::parse("#11:2").unwrap(),
        to: Some(arcadedb_protocol::Link::parse("#12:9").unwrap()),
        related: vec![arcadedb_protocol::Link::parse("#3:1").unwrap()],
    };
    let rec = row.clone().into_grpc_record("edge");
    assert_eq!(
        rec.properties["@in"].kind,
        Some(Kind::LinkValue(arcadedb_protocol::GrpcLink {
            rid: "#11:2".into(),
            r#type: String::new(),
        }))
    );
    assert!(matches!(
        rec.properties["@out"].kind,
        Some(Kind::LinkValue(_))
    ));
    let Some(Kind::ListValue(l)) = &rec.properties["related"].kind else {
        panic!("related must be a list");
    };
    assert!(matches!(l.values[0].kind, Some(Kind::LinkValue(_))));

    // The borrow path agrees.
    let borrowed = row.to_grpc_record("edge");
    assert_eq!(borrowed.properties["@in"].kind, rec.properties["@in"].kind);
}

// ---------------------------------------------------------------------------
// Value helpers + Params
// ---------------------------------------------------------------------------

#[test]
fn list_and_map_helpers_shape_the_wire_values() {
    let list = list_v([str_v("a"), str_v("b")]);
    let Some(Kind::ListValue(l)) = &list.kind else {
        panic!("expected list");
    };
    assert_eq!(l.values.len(), 2);

    let map = map_v([("k".to_string(), i64_v(1))]);
    let Some(Kind::MapValue(m)) = &map.kind else {
        panic!("expected map");
    };
    assert_eq!(m.entries["k"].kind, Some(Kind::Int64Value(1)));
}

#[test]
fn temporal_values_encode_as_utc_timestamps() {
    use chrono::TimeZone;

    let dt = chrono::Utc
        .with_ymd_and_hms(2024, 1, 15, 10, 30, 0)
        .unwrap();
    let Some(Kind::TimestampValue(ts)) = timestamp_v(dt).kind else {
        panic!("expected timestamp");
    };
    assert_eq!(ts.seconds, dt.timestamp());

    // DATE columns: midnight-UTC on the wire (probe-verified shape).
    let d = chrono::NaiveDate::from_ymd_opt(2025, 6, 30).unwrap();
    let Some(Kind::TimestampValue(ts)) = date_v(d).kind else {
        panic!("expected timestamp");
    };
    assert_eq!(ts.seconds, 1_751_241_600); // 2025-06-30T00:00:00Z
    assert_eq!(ts.nanos, 0);
}

#[test]
fn params_construct_from_arrays_and_maps() {
    // Array literal — the call-site form.
    let p = Params::from([("item_id", i64_v(7)), ("name", str_v("ada"))]);
    assert_eq!(p["item_id"].kind, Some(Kind::Int64Value(7)));
    assert_eq!(p.len(), 2);

    // Existing maps (e.g. a DTO's to_param_map output) wrap as-is.
    let mut m = HashMap::new();
    m.insert("k".to_string(), f64_v(0.5));
    let p = Params::from(m);
    assert_eq!(p["k"].kind, Some(Kind::DoubleValue(0.5)));

    // Default (no parameters).
    assert_eq!(Params::default().len(), 0);
}

#[test]
fn params_macro_discovers_kinds_via_into_grpc_value() {
    use chrono::TimeZone;
    let since = chrono::Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let title_owned = String::from("Red Widget");
    let min: f64 = 0.8;

    let p = params! {
        item_id: 1001i64,
        // bare literal pins the fallback: unsuffixed ints resolve i32
        bare: 1001,
        title: "Red Widget",
        owned_title: &title_owned,
        min_score: min,
        featured: true,
        since,
    };

    assert_eq!(p["item_id"].kind, Some(Kind::Int64Value(1001)));
    assert_eq!(p["bare"].kind, Some(Kind::Int32Value(1001)));
    assert_eq!(
        p["title"].kind,
        Some(Kind::StringValue("Red Widget".into()))
    );
    assert_eq!(
        p["owned_title"].kind,
        Some(Kind::StringValue("Red Widget".into()))
    );
    assert_eq!(p["min_score"].kind, Some(Kind::DoubleValue(0.8)));
    assert_eq!(p["featured"].kind, Some(Kind::BoolValue(true)));
    assert_eq!(
        p["since"].kind,
        Some(Kind::TimestampValue(prost_types::Timestamp {
            seconds: since.timestamp(),
            nanos: 0
        }))
    );
}

#[test]
fn params_macro_handles_nested_maps_lists_and_passthrough() {
    // Nested map braces + explicit constructor passthrough + Vec binding.
    let labels = vec!["utility".to_string(), "handbook".to_string()];
    let p = params! {
        item_id: i32_v(416),                     // explicit narrow kind passes through
        filter: { "genre": "utility", "min": 0.8 },
        labels,
    };

    assert_eq!(p["item_id"].kind, Some(Kind::Int32Value(416)));

    let Some(Kind::MapValue(m)) = &p["filter"].kind else {
        panic!("filter must be a map");
    };
    assert_eq!(
        m.entries["genre"].kind,
        Some(Kind::StringValue("utility".into()))
    );
    assert_eq!(m.entries["min"].kind, Some(Kind::DoubleValue(0.8)));

    let Some(Kind::ListValue(l)) = &p["labels"].kind else {
        panic!("Vec must bind as a list");
    };
    assert_eq!(l.values.len(), 2);
}

#[test]
fn params_macro_single_entry_and_trailing_comma() {
    let one = params! { k: "v" };
    assert_eq!(one["k"].kind, Some(Kind::StringValue("v".into())));

    let two = params! { a: 1, b: 2, };
    assert_eq!(two.len(), 2);
}

// ---------------------------------------------------------------------------
// Wave 3 type-affining: set/ordered-map containers
// ---------------------------------------------------------------------------

#[derive(RecordEncode)]
struct TaggedRow {
    tags: std::collections::HashSet<i32>,
    ordered: std::collections::BTreeMap<String, f64>,
}

#[test]
fn sets_encode_as_lists_and_btreemaps_as_maps() {
    let mut tags = std::collections::HashSet::new();
    tags.insert(10);
    tags.insert(20);
    let row = TaggedRow {
        tags,
        ordered: std::collections::BTreeMap::from([("a".to_string(), 0.5), ("b".to_string(), 1.5)]),
    };
    let rec = row.to_grpc_record("t");

    let Some(Kind::ListValue(l)) = &rec.properties["tags"].kind else {
        panic!("set field must encode as a list");
    };
    assert_eq!(l.values.len(), 2);

    let Some(Kind::MapValue(m)) = &rec.properties["ordered"].kind else {
        panic!("BTreeMap field must encode as a map");
    };
    assert_eq!(m.entries["a"].kind, Some(Kind::DoubleValue(0.5)));
}

#[cfg(feature = "decimal")]
mod decimal_feature {
    use arcadedb_protocol::__private::decimal_v;
    use arcadedb_protocol::proto::com::arcadedb::grpc::{grpc_value::Kind, GrpcDecimal, GrpcValue};
    use arcadedb_protocol::RecordDecode;

    #[derive(Debug, PartialEq, RecordDecode)]
    struct PriceRow {
        price: rust_decimal::Decimal,
    }

    fn wire(d: GrpcDecimal) -> arcadedb_protocol::GrpcRecord {
        arcadedb_protocol::rec(
            "t",
            vec![(
                "price",
                GrpcValue {
                    kind: Some(Kind::DecimalValue(d)),
                    logical_type: String::new(),
                },
            )],
        )
    }

    #[test]
    fn decimal_round_trips_through_the_wire_kind() {
        // 12345.6789 — mantissa fits a signed 64-bit integer.
        let d = rust_decimal::Decimal::try_from_i128_with_scale(123_456_789, 4).unwrap();
        let v = decimal_v(d);
        let Some(Kind::DecimalValue(w)) = &v.kind else {
            panic!("expected decimal kind");
        };
        assert_eq!(w.unscaled, 123_456_789);
        assert_eq!(w.scale, 4);
        assert!(w.unscaled_bytes.is_empty());

        let back = PriceRow::try_from(&wire(w.clone())).unwrap();
        assert_eq!(back.price, d);
    }

    #[test]
    fn wide_mantissa_rides_bytes_and_overflows_loudly() {
        // >63-bit (but representable) mantissa must take the bytes path...
        let wide = rust_decimal::Decimal::from(1i128 << 70);
        let Some(Kind::DecimalValue(w)) = &decimal_v(wide).kind else {
            panic!("expected decimal kind");
        };
        assert!(!w.unscaled_bytes.is_empty() || w.unscaled != (1i128 << 70) as i64);
        assert_eq!(
            PriceRow::try_from(&wire(w.clone())).unwrap().price,
            wide,
            "bytes-path mantissa round-trips"
        );

        // ...and >96-bit unscaled wire values decode as loud errors, never
        // silent precision loss.
        let too_wide = wire(GrpcDecimal {
            unscaled: 0,
            scale: 2,
            unscaled_bytes: vec![0xFF; 17],
        });
        let err = PriceRow::try_from(&too_wide).unwrap_err();
        assert!(err.to_string().contains("too wide"), "{err}");
    }
}
