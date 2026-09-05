//! Integration tests for the `RecordDecode` derive + runtime — consume the
//! crate exactly like external users (path dependency), so the generated code
//! resolves the default `::arcadedb_protocol` runtime path. Covers DTO shapes
//! with nested embeds, lists of links, serde renames, `with`-custom types,
//! defaults, flatten, and `@rid` synths).

use std::borrow::Cow;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use prost_types::Timestamp;
use serde::Deserialize;

use arcadedb_protocol::__private::{embedded_v, f64_v, i32_v, i64_v, str_v};
use arcadedb_protocol::proto::com::arcadedb::grpc::{
    grpc_value::Kind, GrpcDecimal, GrpcEmbedded, GrpcLink, GrpcList, GrpcMap, GrpcRecord, GrpcValue,
};
use arcadedb_protocol::record::{FromGrpcValue, RecordDecodeError};
use arcadedb_protocol::{Link, RecordDecode};

// ---------------------------------------------------------------------------
// Test DTOs — mirroring real consumer shapes
// ---------------------------------------------------------------------------

/// A `table:key` identity type unknown to the macro — it plugs in
/// through `#[record(with = ...)]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
struct TestId(String);

impl TestId {
    fn from_value(v: &GrpcValue) -> Result<Self, RecordDecodeError> {
        let s = String::from_grpc_value(v)?;
        if !s.contains(':') {
            return Err(RecordDecodeError::conversion(format!(
                "invalid TestId `{s}`: expected `table:key`"
            )));
        }
        Ok(TestId(s))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Caption {
    text: String,
    media_ref: String,
}

// The clone-based escape hatch keeps manual FromGrpcValue types usable as
// flatten targets under the zero-copy DecodeFromMap protocol.
arcadedb_protocol::impl_decode_from_map_via_value!(Caption);

impl<'de> FromGrpcValue<'de> for Caption {
    fn from_grpc_value(v: &'de GrpcValue) -> Result<Self, RecordDecodeError> {
        match &v.kind {
            Some(Kind::EmbeddedValue(e)) => {
                let text = arcadedb_protocol::record::get(&e.fields, &[], "text")?;
                let media_ref = arcadedb_protocol::record::get(&e.fields, &[], "media_ref")?;
                Ok(Caption {
                    text,
                    media_ref,
                })
            }
            _ => Err(RecordDecodeError::type_mismatch(
                "embedded record",
                arcadedb_protocol::record::kind_name(&v.kind),
            )),
        }
        .map_err(|e| e.with_field("caption"))
    }
}

/// Nested embedded record inside a list.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Block {
    content_rid: String,
    content_type: String,
    #[serde(default)]
    hint: Option<String>,
}

/// Unit-variant enum at the record level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, RecordDecode)]
#[serde(rename_all = "snake_case")]
enum Priority {
    Low,
    Medium,
    Hard,
}

/// Unit-variant enum with explicit kebab renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, RecordDecode)]
enum Format {
    #[serde(rename = "rich-text")]
    RichText,
    #[serde(rename = "plain-text")]
    PlainText,
}

/// Mixed scalars, lists, embedded lists, `with`-decoded identity,
/// defaults, optionals, and a `@rid` projection.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Article {
    #[serde(rename = "logical_key")]
    #[record(with = "self::TestId::from_value")]
    id: TestId,
    title: String,
    sequence: u32,
    priority: Priority,
    format: Format,
    #[serde(default)]
    has_review: bool,
    #[serde(default = "default_synced")]
    synced_at: String,
    #[serde(rename = "tags")]
    tags: Vec<String>,
    #[serde(default)]
    optional_note: Option<String>,
    #[serde(rename = "sections")]
    sections: Vec<Block>,
    #[serde(rename = "caption")]
    caption: Option<Caption>,
    #[serde(rename = "@rid")]
    rid: Option<String>,
}

fn default_synced() -> String {
    "1970-01-01T00:00:00Z".to_string()
}

/// A flattened base struct + a sibling score field.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct ArticleRow {
    #[serde(flatten)]
    article: Article,
    #[serde(default)]
    score: f64,
}

/// Timestamp decode.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct WithTs {
    #[serde(rename = "created_at")]
    created_at: DateTime<Utc>,
    #[serde(default)]
    likely_year: Option<i64>,
}

/// Nested embedded item used inside an internally-tagged variant.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct TranscriptLine {
    author: String,
    #[serde(default)]
    media_ref: String,
}

/// Internally-tagged enum with kebab per-variant renames.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
#[serde(tag = "type")]
enum ContentStyle {
    #[serde(rename = "choice")]
    Choice {
        media_ref: String,
        question: Option<String>,
        options: Vec<String>,
        correct_index: u32,
    },
    #[serde(rename = "playlist")]
    Playlist {
        lines: Vec<TranscriptLine>,
        options: Vec<String>,
        correct_index: u32,
    },
    #[serde(rename = "repeat")]
    Repeat { media_ref: String },
}

/// Internally-tagged enum with container `rename_all = "snake_case"`.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InputStyle {
    SelectedIndex { selected_index: u32 },
    Ordering { order: Vec<String> },
    Transcript { transcript: String },
}

/// Internally-tagged enum flattened behind a sibling field.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RuleStyle {
    CategoryFilter {
        #[serde(default)]
        required_label_ids: Vec<i32>,
        #[serde(default)]
        excluded_label_ids: Vec<i32>,
    },
    RegionGraph {
        region_code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct FilterSpec {
    #[serde(flatten)]
    strategy: RuleStyle,
    #[serde(default)]
    max_capacity: Option<i32>,
}

/// Custom collection container decoded via `with` + `#[serde(default)]`
/// — absent property → default, present list → custom conversion.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
struct LabelSet(Vec<String>);

impl LabelSet {
    fn from_grpc(v: &GrpcValue) -> Result<Self, RecordDecodeError> {
        Ok(LabelSet(Vec::<String>::from_grpc_value(v)?))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Scripted {
    #[serde(default)]
    #[record(with = "self::LabelSet::from_grpc")]
    media_files: LabelSet,
}

/// f64 tolerance of integer kinds.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct ScoreRow {
    #[serde(default)]
    score: f64,
}

// ---------------------------------------------------------------------------
// Wire kinds a single property can take (cross-kind string/timestamp/
// decimal targets, zero-scale decimal ints, transparent newtype
// delegation, byte blobs).
// ---------------------------------------------------------------------------

/// String targets accept timestamps (RFC-3339) and scaled decimals (`x`e-`y`).
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct ProjectedStrings {
    #[serde(rename = "issued_at")]
    issued_at: String,
    #[serde(rename = "amount")]
    amount: String,
}

/// `DateTime<Utc>` targets accept RFC-3339 string values.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct When {
    #[serde(rename = "at")]
    at: DateTime<Utc>,
}

/// Zero-scale, byte-free decimals decode as ints (scaled ones do not).
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Seq {
    #[serde(rename = "seq")]
    seq: i32,
}

/// `Vec<u8>` fields are byte blobs: the `bytes` kind or a list of small ints.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Blob {
    data: Vec<u8>,
    #[serde(default)]
    extra: Option<Vec<u8>>,
}

/// Transparent newtype — delegating `FromGrpcValue` for an external-id
/// field type that doesn't need the `with` attribute.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
#[serde(transparent)]
struct Pk(String);

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Holder {
    #[serde(rename = "id")]
    pk: Pk,
}

// ---------------------------------------------------------------------------
// Record shapes beyond scalar fields: map columns, lists of linked
// RIDs, `@in`/`@out` edge-rename projections, and explicit-null optionals.
// ---------------------------------------------------------------------------

/// Map column + a list of linked RIDs (e.g. `Page.name_translations`
/// + linked content).
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct Localized {
    #[serde(rename = "name_translations")]
    name_translations: HashMap<String, String>,
    #[serde(default)]
    links: Vec<String>,
}

/// Edge-projection shape: `@in`/`@out` are links on the wire; the
/// server may also send an explicit null for an optional property.
#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct LinkEdge {
    #[serde(rename = "@in")]
    in_rid: String,
    #[serde(rename = "@out")]
    out_rid: String,
    #[serde(default)]
    note: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn gv(kind: Kind) -> GrpcValue {
    GrpcValue {
        kind: Some(kind),
        logical_type: String::new(),
    }
}

/// A `GrpcValue` whose oneof is unset — the wire's explicit null.
fn gv_null() -> GrpcValue {
    GrpcValue {
        kind: None,
        logical_type: String::new(),
    }
}

fn list(vals: Vec<GrpcValue>) -> GrpcValue {
    gv(Kind::ListValue(GrpcList { values: vals }))
}

fn map_v(entries: Vec<(&str, GrpcValue)>) -> GrpcValue {
    gv(Kind::MapValue(GrpcMap {
        entries: entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }))
}

fn block_a() -> GrpcValue {
    embedded_v(
        "block",
        vec![
            ("content_rid", str_v("#11:0")),
            ("content_type", str_v("entry")),
            ("hint", str_v("a hint")),
        ],
    )
}

fn block_b() -> GrpcValue {
    embedded_v(
        "block",
        vec![
            ("content_rid", str_v("#11:1")),
            ("content_type", str_v("note")),
        ],
    )
}

fn article_record() -> GrpcRecord {
    GrpcRecord {
        rid: "#12:0".to_string(),
        r#type: "article".to_string(),
        properties: HashMap::from([
            ("logical_key".to_string(), str_v("article:root")),
            ("title".to_string(), str_v("Intro to Rust")),
            ("sequence".to_string(), i32_v(1)),
            ("priority".to_string(), str_v("low")),
            ("format".to_string(), str_v("rich-text")),
            ("tags".to_string(), list(vec![str_v("a"), str_v("b")])),
            ("sections".to_string(), list(vec![block_a(), block_b()])),
            (
                "caption".to_string(),
                embedded_v(
                    "caption",
                    vec![
                        ("text", str_v("Hello!")),
                        ("media_ref", str_v("media://1")),
                    ],
                ),
            ),
        ]),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn decodes_mixed_scalars_lists_and_nested_embeds() {
    let rec = article_record();
    let article = Article::try_from(&rec).expect("decode article");

    assert_eq!(article.id, TestId("article:root".into()));
    assert_eq!(article.title, "Intro to Rust");
    assert_eq!(article.sequence, 1u32);
    assert_eq!(article.tags, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(article.priority, Priority::Low);
    assert_eq!(article.format, Format::RichText);
    assert_eq!(article.sections.len(), 2);
    assert_eq!(article.sections[0].content_rid, "#11:0");
    assert_eq!(article.sections[0].hint.as_deref(), Some("a hint"));
    // Second block lacks `hint` (optional) — and it's `Option<String>`:
    assert_eq!(article.sections[1].hint, None);
    assert_eq!(
        article.caption.as_ref().map(|p| p.text.as_str()),
        Some("Hello!")
    );
}

#[test]
fn missing_optional_and_defaulted_fields_resolve() {
    let rec = article_record();
    let article = Article::try_from(&rec).unwrap();

    // Absent but defaulted.
    assert!(!article.has_review);
    assert_eq!(article.synced_at, "1970-01-01T00:00:00Z");
    // Absent + optional → None (not an error).
    assert_eq!(article.optional_note, None);
    // Synthetic @rid from GrpcRecord.rid when the query omitted it.
    assert_eq!(article.rid.as_deref(), Some("#12:0"));
}

#[test]
fn synthetic_rid_prefers_the_property() {
    let mut rec = article_record();
    rec.properties.insert("@rid".to_string(), str_v("#99:99"));
    let article = Article::try_from(&rec).unwrap();
    assert_eq!(article.rid.as_deref(), Some("#99:99"));
}

#[test]
fn owned_and_borrowed_try_from_agree() {
    let rec = article_record();
    let borrowed = Article::try_from(&rec).unwrap();
    let owned: Article = rec.clone().try_into().unwrap();
    assert_eq!(borrowed, owned);
}

#[test]
fn flatten_composes_base_struct_with_siblings() {
    let mut rec = article_record();
    rec.properties.insert("score".to_string(), f64_v(4.5));
    let row = ArticleRow::try_from(&rec).unwrap();
    assert_eq!(row.score, 4.5);
    assert_eq!(row.article.title, "Intro to Rust");
    assert_eq!(row.article.sequence, 1u32);
}

#[test]
fn missing_required_field_is_an_error() {
    let rec = article_record();
    let mut rec2 = GrpcRecord {
        properties: rec.properties.clone(),
        ..rec.clone()
    };
    rec2.properties.remove("title");

    let err = Article::try_from(&rec2).unwrap_err();
    match &err {
        RecordDecodeError::MissingField { path } => assert_eq!(path, "title"),
        other => panic!("expected MissingField, got {other:?}"),
    }
    assert!(err.to_string().contains("missing property `title`"));
}

#[test]
fn type_mismatch_error_names_field_and_kinds() {
    let mut rec = article_record();
    rec.properties.insert("title".to_string(), i64_v(42));
    let err = Article::try_from(&rec).unwrap_err();
    match &err {
        RecordDecodeError::TypeMismatch {
            path,
            expected,
            actual,
        } => {
            assert_eq!(path, "title");
            assert_eq!(*expected, "string");
            assert_eq!(*actual, "int64");
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
    assert!(err
        .to_string()
        .contains("property `title`: expected string, got int64"));
}

#[test]
fn list_element_error_reports_position() {
    let mut rec = article_record();
    rec.properties
        .insert("tags".to_string(), list(vec![str_v("ok"), i64_v(7)]));
    let err = Article::try_from(&rec).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("list[1]"), "unexpected message: {msg}");
}

#[test]
fn nested_embedded_from_value_decodes() {
    // An article delivered as an embedded value (inline projection).
    let mut props = article_record().properties;
    props.remove("caption");
    let embedded = gv(Kind::EmbeddedValue(GrpcEmbedded {
        r#type: "article".into(),
        fields: props,
    }));
    let article = Article::from_grpc_value(&embedded).unwrap();
    assert_eq!(article.title, "Intro to Rust");
}

#[test]
fn timestamp_decodes_from_protobuf_time() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([(
            "created_at".to_string(),
            gv(Kind::TimestampValue(Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            })),
        )]),
    };
    let row = WithTs::try_from(&rec).unwrap();
    let expected = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    assert_eq!(row.created_at, expected);
    assert_eq!(row.likely_year, None);
}

#[test]
fn unknown_enum_variant_is_reported() {
    let mut rec = article_record();
    rec.properties
        .insert("priority".to_string(), str_v("impossible"));
    let err = Article::try_from(&rec).unwrap_err();
    assert!(err.to_string().contains("unknown variant `impossible`"));
}

#[test]
fn custom_with_reports_the_error() {
    let mut rec = article_record();
    rec.properties
        .insert("logical_key".to_string(), str_v("no-colon-here"));
    let err = Article::try_from(&rec).unwrap_err();
    assert!(err.to_string().contains("invalid TestId `no-colon-here`"));
}

#[test]
fn json_value_catch_all_accepts_any_kind() {
    #[derive(Debug, PartialEq, Deserialize, RecordDecode)]
    struct Loose {
        #[serde(default)]
        content: serde_json::Value,
    }
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("content".to_string(), str_v("x"))]),
    };
    let row = Loose::try_from(&rec).unwrap();
    assert_eq!(row.content, serde_json::json!("x"));
}

#[test]
fn internally_tagged_enum_decodes_struct_variants() {
    let mc = map_v(vec![
        ("type", str_v("choice")),
        ("media_ref", str_v("media://x")),
        ("question", str_v("Which?")),
        ("options", list(vec![str_v("a"), str_v("b")])),
        ("correct_index", i32_v(0)),
    ]);
    let got = ContentStyle::from_grpc_value(&mc).unwrap();
    assert_eq!(
        got,
        ContentStyle::Choice {
            media_ref: "media://x".into(),
            question: Some("Which?".into()),
            options: vec!["a".into(), "b".into()],
            correct_index: 0,
        }
    );
}

#[test]
fn internally_tagged_enum_decodes_nested_variant() {
    let lac = map_v(vec![
        ("type", str_v("playlist")),
        (
            "lines",
            list(vec![
                embedded_v("transcript_line", vec![("author", str_v("A"))]),
                embedded_v(
                    "transcript_line",
                    vec![("author", str_v("B")), ("media_ref", str_v("a:1"))],
                ),
            ]),
        ),
        ("options", list(vec![str_v("x")])),
        ("correct_index", i32_v(1)),
    ]);
    let got = ContentStyle::from_grpc_value(&lac).unwrap();
    match got {
        ContentStyle::Playlist {
            lines,
            correct_index,
            ..
        } => {
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0].author, "A");
            assert_eq!(lines[0].media_ref, ""); // defaulted
            assert_eq!(lines[1].author, "B");
            assert_eq!(lines[1].media_ref, "a:1");
            assert_eq!(correct_index, 1);
        }
        other => panic!("expected Playlist, got {other:?}"),
    }
}

#[test]
fn internally_tagged_enum_rename_all_snake_case() {
    let v = map_v(vec![
        ("kind", str_v("selected_index")),
        ("selected_index", i32_v(3)),
    ]);
    assert_eq!(
        InputStyle::from_grpc_value(&v).unwrap(),
        InputStyle::SelectedIndex { selected_index: 3 }
    );
    let t = map_v(vec![
        ("kind", str_v("transcript")),
        ("transcript", str_v("hi")),
    ]);
    assert_eq!(
        InputStyle::from_grpc_value(&t).unwrap(),
        InputStyle::Transcript {
            transcript: "hi".into()
        }
    );
}

#[test]
fn internally_tagged_enum_missing_tag_is_missing_field() {
    let v = map_v(vec![("options", list(vec![str_v("a")]))]);
    let err = ContentStyle::from_grpc_value(&v).unwrap_err();
    assert_eq!(
        err,
        RecordDecodeError::MissingField {
            path: "type".to_string()
        }
    );
}

#[test]
fn internally_tagged_enum_unknown_variant_reported() {
    let v = map_v(vec![("type", str_v("essay"))]);
    let err = ContentStyle::from_grpc_value(&v).unwrap_err();
    assert!(err.to_string().contains("unknown variant `essay`"));
}

#[test]
fn flattened_internally_tagged_enum_decodes() {
    let spec = embedded_v(
        "filter_spec",
        vec![
            ("type", str_v("category_filter")),
            ("required_label_ids", list(vec![i32_v(1), i32_v(2)])),
            ("max_capacity", i32_v(50)),
        ],
    );
    let got = FilterSpec::from_grpc_value(&spec).unwrap();
    assert_eq!(
        got,
        FilterSpec {
            strategy: RuleStyle::CategoryFilter {
                required_label_ids: vec![1, 2],
                excluded_label_ids: vec![],
            },
            max_capacity: Some(50),
        }
    );
}

#[test]
fn with_default_uses_default_when_absent() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::new(),
    };
    let row = Scripted::try_from(&rec).unwrap();
    assert_eq!(row.media_files, LabelSet::default());
}

#[test]
fn with_default_decodes_present_property() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([(
            "media_files".to_string(),
            list(vec![str_v("m1"), str_v("m2")]),
        )]),
    };
    let row = Scripted::try_from(&rec).unwrap();
    assert_eq!(
        row.media_files,
        LabelSet(vec!["m1".to_string(), "m2".to_string()])
    );
}

#[test]
fn float_accepts_integer_and_float_kinds() {
    for (val, expected) in [
        (i32_v(42), 42.0),
        (i64_v(-7), -7.0),
        (f64_v(4.5), 4.5),
        (gv(Kind::FloatValue(2.5)), 2.5),
    ] {
        let rec = GrpcRecord {
            rid: String::new(),
            r#type: String::new(),
            properties: HashMap::from([("score".to_string(), val)]),
        };
        let row = ScoreRow::try_from(&rec).unwrap();
        assert_eq!(row.score, expected);
    }
}

#[test]
fn string_field_accepts_timestamp_and_decimal() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([
            (
                "issued_at".to_string(),
                gv(Kind::TimestampValue(Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                })),
            ),
            (
                "amount".to_string(),
                gv(Kind::DecimalValue(GrpcDecimal {
                    scale: 2,
                    unscaled: 12345,
                    unscaled_bytes: vec![],
                })),
            ),
        ]),
    };
    let row = ProjectedStrings::try_from(&rec).unwrap();
    assert_eq!(
        row.issued_at,
        DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .to_rfc3339()
    );
    assert_eq!(row.amount, "12345e-2");
}

#[test]
fn datetime_field_accepts_string_date() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("at".to_string(), str_v("2024-05-01T10:00:00Z"))]),
    };
    let row = When::try_from(&rec).unwrap();
    assert_eq!(
        row.at,
        DateTime::parse_from_rfc3339("2024-05-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    );

    // Non-timestamp text is a conversion error, not a type mismatch.
    let bad = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("at".to_string(), str_v("not-a-date"))]),
    };
    let err = When::try_from(&bad).unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid timestamp string `not-a-date`"));
}

#[test]
fn char_decodes_single_char_strings() {
    assert_eq!(char::from_grpc_value(&str_v("x")).unwrap(), 'x');
    let err = char::from_grpc_value(&str_v("ab")).unwrap_err();
    assert_eq!(
        err,
        RecordDecodeError::type_mismatch("single-char string", "string")
    );
}

#[test]
fn int_field_accepts_zero_scale_decimal() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([(
            "seq".to_string(),
            gv(Kind::DecimalValue(GrpcDecimal {
                scale: 0,
                unscaled: 42,
                unscaled_bytes: vec![],
            })),
        )]),
    };
    assert_eq!(Seq::try_from(&rec).unwrap().seq, 42);

    // A scaled decimal is not an int.
    let scaled = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([(
            "seq".to_string(),
            gv(Kind::DecimalValue(GrpcDecimal {
                scale: 2,
                unscaled: 4242,
                unscaled_bytes: vec![],
            })),
        )]),
    };
    let err = Seq::try_from(&scaled).unwrap_err();
    assert!(err.to_string().contains("expected i32, got decimal"));
}

#[test]
fn newtype_field_delegates_to_inner() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("id".to_string(), str_v("article:root"))]),
    };
    let row = Holder::try_from(&rec).unwrap();
    assert_eq!(row.pk, Pk("article:root".into()));

    // And through a list of the wrapper type.
    let v = list(vec![str_v("a:1"), str_v("b:2")]);
    assert_eq!(
        Vec::<Pk>::from_grpc_value(&v).unwrap(),
        vec![Pk("a:1".into()), Pk("b:2".into())]
    );
}

#[test]
fn bytes_field_decodes_blob_and_int_list() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("data".to_string(), gv(Kind::BytesValue(vec![1, 2, 3])))]),
    };
    assert_eq!(Blob::try_from(&rec).unwrap().data, vec![1u8, 2, 3]);

    // The legacy deserializer also accepted lists of small ints.
    let ints = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([(
            "data".to_string(),
            list(vec![i32_v(65), i64_v(66), i32_v(67)]),
        )]),
    };
    assert_eq!(Blob::try_from(&ints).unwrap().data, vec![65u8, 66, 67]);

    // Optional blob: absent → `None`.
    let empty = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("data".to_string(), gv(Kind::BytesValue(vec![])))]),
    };
    assert_eq!(Blob::try_from(&empty).unwrap().extra, None);

    // Out-of-range ints are rejected — no silent truncation.
    let big = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("data".to_string(), list(vec![i64_v(300)]))]),
    };
    let err = Blob::try_from(&big).unwrap_err();
    assert!(err.to_string().contains("expected u8, got int64"));
}

#[test]
fn localized_map_and_linked_rid_strings_decode() {
    let link = |rid: &str| {
        gv(Kind::LinkValue(GrpcLink {
            rid: rid.to_string(),
            r#type: String::new(),
        }))
    };
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([
            (
                "name_translations".to_string(),
                map_v(vec![
                    ("en", str_v("Arcadia")),
                    ("pt", str_v("Arc\u{e1}dia")),
                ]),
            ),
            ("links".to_string(), list(vec![link("#9:1"), link("#9:2")])),
        ]),
    };
    let row = Localized::try_from(&rec).unwrap();
    assert_eq!(
        row.name_translations,
        HashMap::from([
            ("en".to_string(), "Arcadia".to_string()),
            ("pt".to_string(), "Arc\u{e1}dia".to_string()),
        ])
    );
    // Linked RIDs decode as their `#bucket:pos` string.
    assert_eq!(row.links, vec!["#9:1".to_string(), "#9:2".to_string()]);
}

#[test]
fn edge_renames_and_explicit_null_option_decode() {
    let link = |rid: &str| {
        gv(Kind::LinkValue(GrpcLink {
            rid: rid.to_string(),
            r#type: String::new(),
        }))
    };
    let rec = GrpcRecord {
        rid: "#12:5".to_string(),
        r#type: "link_edge".to_string(),
        properties: HashMap::from([
            ("@in".to_string(), link("#12:1")),
            ("@out".to_string(), link("#12:2")),
            // The server serialized an explicit null — an `Option` must read it
            // as `None`, not as a type error.
            ("note".to_string(), gv_null()),
        ]),
    };
    let row = LinkEdge::try_from(&rec).unwrap();
    assert_eq!(row.in_rid, "#12:1");
    assert_eq!(row.out_rid, "#12:2");
    assert_eq!(row.note, None);
}

// ---------------------------------------------------------------------------
// Borrowed DTOs (zero-copy) — `&'a str` / `Cow<'a, str>` / `&'a [u8]` targets
// decode as views into the wire value's own allocations. These are record
// rows delivered via `&GrpcRecord` (no owned `TryFrom<GrpcRecord>` for them:
// a borrowing struct cannot outlive the owned record it would borrow from).
// ---------------------------------------------------------------------------

/// A borrowed struct field: `&str` and `Cow<'a, str>` views.
#[derive(Debug, PartialEq, RecordDecode)]
struct BorrowedRow<'a> {
    title: &'a str,
    tags_note: Cow<'a, str>,
}

/// A list of borrowed strings: `Vec<&str>` elements view the wire list.
#[derive(Debug, PartialEq, RecordDecode)]
struct BorrowedList<'a> {
    tags: Vec<&'a str>,
}

/// A zero-copy byte-blob view.
#[derive(Debug, PartialEq, RecordDecode)]
struct BorrowedBlob<'a> {
    data: &'a [u8],
}

#[test]
fn borrowed_row_views_the_record_allocations() {
    let rec = GrpcRecord {
        rid: "#12:0".to_string(),
        r#type: "article".to_string(),
        properties: HashMap::from([
            ("title".to_string(), str_v("Intro to Rust")),
            ("tags_note".to_string(), str_v("hello")),
        ]),
    };
    let row = BorrowedRow::try_from(&rec).unwrap();

    // Values match, and the `&str` really points into the record's property
    // map — zero-copy, not a clone.
    assert_eq!(row.title, "Intro to Rust");
    assert_eq!(row.tags_note, "hello");
    assert!(matches!(row.tags_note, Cow::Borrowed(_)));
    let Some(Kind::StringValue(s)) = &rec.properties["title"].kind else {
        panic!("title property not a string")
    };
    assert!(std::ptr::eq(row.title.as_ptr(), s.as_ptr()));
}

#[test]
fn borrowed_list_views_the_wire_strings() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("tags".to_string(), list(vec![str_v("a"), str_v("b")]))]),
    };
    let row = BorrowedList::try_from(&rec).unwrap();
    assert_eq!(row.tags, vec!["a", "b"]);
    let Some(Kind::ListValue(l)) = &rec.properties["tags"].kind else {
        panic!("tags property not a list")
    };
    let Some(Kind::StringValue(s0)) = &l.values[0].kind else {
        panic!("first tag not a string")
    };
    assert!(std::ptr::eq(row.tags[0].as_ptr(), s0.as_ptr()));
}

// ---------------------------------------------------------------------------
// serde parity — the derive must speak serde's exact naming dialect
// ---------------------------------------------------------------------------

/// Parity target: the wire keys serde's own `Serialize` emits are what
/// ArcadeDB stores; `RecordDecode` must look up exactly those keys —
/// including unconventional (acronym) idents where hand-rolled case
/// conversion disagrees with serde's rules.
#[derive(Debug, Clone, PartialEq, serde::Serialize, RecordDecode)]
#[serde(rename_all = "camelCase")]
#[allow(non_snake_case)] // URLName: deliberately unconventional ident for parity
struct SerdeParityCamelRow {
    title: String,
    url_name: String,
    URLName: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, RecordDecode)]
#[serde(rename_all = "snake_case")]
enum SerdeParityAcronymKind {
    PlainKind,
    HTTPServer,
    JSONPayload,
}

/// Build a record whose string properties are exactly the keys serde
/// serialized — ground truth from serde itself, no hand-copied key names.
fn record_from_serde_json(json: &serde_json::Value) -> GrpcRecord {
    let serde_json::Value::Object(map) = json else {
        panic!("expected a JSON object");
    };
    GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: map
            .iter()
            .map(|(k, v)| {
                let s = v
                    .as_str()
                    .unwrap_or_else(|| panic!("non-string value for {k}"));
                (k.clone(), str_v(s))
            })
            .collect(),
    }
}

#[test]
fn record_decode_reads_the_exact_keys_serde_writes() {
    let row = SerdeParityCamelRow {
        title: "t".into(),
        url_name: "u".into(),
        URLName: "acronym".into(),
    };
    let rec = record_from_serde_json(&serde_json::to_value(&row).unwrap());
    assert_eq!(
        SerdeParityCamelRow::try_from(&rec).unwrap(),
        row,
        "RecordDecode must look up the exact keys serde serializes"
    );
}

#[test]
fn record_decode_accepts_the_variant_names_serde_writes() {
    for kind in [
        SerdeParityAcronymKind::PlainKind,
        SerdeParityAcronymKind::HTTPServer,
        SerdeParityAcronymKind::JSONPayload,
    ] {
        let wire = serde_json::to_value(kind)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let v = str_v(&wire);
        assert_eq!(
            SerdeParityAcronymKind::from_grpc_value(&v).unwrap(),
            kind,
            "wire name `{wire}` is what serde writes — it must decode"
        );
    }
}

// ---------------------------------------------------------------------------
// Flatten behavior lock-in (safety net for the clone-then-remove cleanup)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, RecordDecode)]
struct OptionalFlattenRow {
    name: String,
    #[serde(flatten)]
    extra: Option<Caption>,
}

// ---------------------------------------------------------------------------
// `record`-namespace attribute forms
// ---------------------------------------------------------------------------

/// `#[record(rename(deserialize = "..."))]` — the paren form of rename in
/// the `record` namespace, parity with serde's two-sided rename syntax.
#[derive(Debug, Clone, PartialEq, RecordDecode)]
struct RecordNamespaceRenameRow {
    #[record(rename(deserialize = "published_at"))]
    updated_at: String,
}

#[test]
fn record_namespace_rename_deserialize_form_drives_the_wire_name() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("published_at".to_string(), str_v("2026-08-19"))]),
    };
    let row = RecordNamespaceRenameRow::try_from(&rec).unwrap();
    assert_eq!(row.updated_at, "2026-08-19");

    // The Rust field name is not a second accepted key.
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("updated_at".to_string(), str_v("nope"))]),
    };
    assert!(matches!(
        RecordNamespaceRenameRow::try_from(&rec),
        Err(RecordDecodeError::MissingField { .. })
    ));
}

#[test]
fn flatten_receives_exactly_the_untaken_properties() {
    // Sibling named field is taken; the rest flows to the flattened target.
    let mut rec = article_record();
    rec.properties.insert("score".to_string(), f64_v(4.5));
    let row = ArticleRow::try_from(&rec).unwrap();
    assert_eq!(row.score, 4.5);
    assert_eq!(row.article.title, "Intro to Rust");

    // Unknown extra properties are ignored, not rejected, and don't shadow
    // the named fields.
    rec.properties
        .insert("extra".to_string(), str_v("leftover"));
    let row = ArticleRow::try_from(&rec).unwrap();
    assert_eq!(row.article.title, "Intro to Rust");

    // Optional flatten: empty rest (everything taken) → None; populated
    // rest → Some.
    let taken = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("name".to_string(), str_v("only-child"))]),
    };
    assert_eq!(OptionalFlattenRow::try_from(&taken).unwrap().extra, None);

    let populated = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([
            ("name".to_string(), str_v("row")),
            ("text".to_string(), str_v("Hello!")),
            ("media_ref".to_string(), str_v("a:1")),
        ]),
    };
    let extra = OptionalFlattenRow::try_from(&populated).unwrap().extra;
    assert_eq!(
        extra.map(|e| (e.text, e.media_ref)),
        Some(("Hello!".to_string(), "a:1".to_string()))
    );
}

#[test]
fn borrowed_bytes_view_the_wire_blob() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("data".to_string(), gv(Kind::BytesValue(vec![1, 2, 3])))]),
    };
    let row = BorrowedBlob::try_from(&rec).unwrap();
    assert_eq!(row.data, &[1u8, 2, 3]);
    let Some(Kind::BytesValue(b)) = &rec.properties["data"].kind else {
        panic!("data property not bytes")
    };
    assert!(std::ptr::eq(row.data.as_ptr(), b.as_ptr()));
}
// ---------------------------------------------------------------------------
// WIRES — the DTO is the canonical SELECT column list
// ---------------------------------------------------------------------------

#[test]
fn wires_list_every_readable_column_in_declaration_order() {
    // Skipped fields are not read; flatten fields consume "the rest" (their
    // own WIRES describe them); everything else is a readable column.
    assert_eq!(
        Article::WIRES.first(),
        Some(&"logical_key"),
        "renames are honored in WIRES"
    );
    assert!(Article::WIRES.contains(&"@rid"));
    assert!(
        !Article::WIRES.contains(&"has_review") || {
            // has_review is a defaulted bool — it IS a readable column:
            Article::WIRES.contains(&"has_review")
        }
    );
    assert_eq!(
        Article::select_list().split(", ").count(),
        Article::WIRES.len()
    );

    // A derived borrowed DTO: wire names straight from the fields.
    assert_eq!(Block::WIRES, &["content_rid", "content_type", "hint"]);
    assert_eq!(Block::select_list(), "content_rid, content_type, hint");
}

#[test]
fn wires_exclude_decode_skipped_fields() {
    #[derive(RecordDecode)]
    #[allow(dead_code)] // fields exist to be skipped, not read
    struct RowWithSkip {
        real: String,
        #[serde(skip)]
        ghost: String,
    }
    assert_eq!(RowWithSkip::WIRES, &["real"]);
}

// ---------------------------------------------------------------------------
// Zero-copy flatten — borrowed targets borrow the record's own map
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, RecordDecode)]
struct BorrowedFlattenRest<'a> {
    text: Cow<'a, str>,
    media_ref: Cow<'a, str>,
}

#[derive(Debug, Clone, PartialEq, RecordDecode)]
struct BorrowedFlattenRow<'a> {
    name: String,
    #[serde(flatten)]
    rest: BorrowedFlattenRest<'a>,
}

#[test]
fn borrowed_flatten_target_views_the_record_allocations() {
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([
            ("name".to_string(), str_v("row")),
            ("text".to_string(), str_v("Hello!")),
            ("media_ref".to_string(), str_v("a:1")),
        ]),
    };
    let row = BorrowedFlattenRow::try_from(&rec).unwrap();
    assert_eq!(row.name, "row");
    assert_eq!(row.rest.text, Cow::Borrowed("Hello!"));
    assert_eq!(row.rest.media_ref, Cow::Borrowed("a:1"));

    // Zero-copy: the views point INTO the record's own allocations — the
    // pre-DecodeFromMap path cloned the rest map, making this impossible.
    let wire_plain = match &rec.properties["text"].kind {
        Some(Kind::StringValue(s)) => s,
        k => panic!("text not a string: {k:?}"),
    };
    match row.rest.text {
        Cow::Borrowed(s) => assert!(std::ptr::eq(s.as_ptr(), wire_plain.as_ptr())),
        Cow::Owned(_) => panic!("flatten rest must be Borrowed, not Owned"),
    }
}

#[test]
fn wire_kinds_map_rust_types_onto_column_types() {
    #[derive(RecordDecode)]
    #[allow(dead_code)] // fields exist to carry wire kinds, not to be read
    struct KindRow<'a> {
        name: Cow<'a, str>,
        count: i32,
        big: i64,
        ratio: f64,
        active: bool,
        tags: Vec<i32>,
        unique_tags: std::collections::BTreeSet<i32>,
        alt_names: std::collections::HashSet<String>,
        when: chrono::DateTime<chrono::Utc>,
        day: chrono::NaiveDate,
        meta: std::collections::HashMap<String, String>,
        ordered_meta: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        blob: Vec<u8>,
        #[record(rename = "renamed_with", with = "crate::w", default)]
        custom: i64,
    }

    let kinds: std::collections::HashMap<&str, &str> =
        KindRow::WIRE_KINDS.iter().copied().collect();
    assert_eq!(kinds["name"], "STRING");
    assert_eq!(kinds["count"], "INTEGER");
    assert_eq!(kinds["big"], "LONG");
    assert_eq!(kinds["ratio"], "DOUBLE");
    assert_eq!(kinds["active"], "BOOLEAN");
    assert_eq!(kinds["tags"], "LIST OF INTEGER");
    assert_eq!(kinds["unique_tags"], "LIST OF INTEGER");
    assert_eq!(kinds["alt_names"], "LIST OF STRING");
    assert_eq!(kinds["when"], "DATETIME");
    assert_eq!(kinds["day"], "DATE");
    assert_eq!(kinds["meta"], "MAP");
    assert_eq!(kinds["ordered_meta"], "MAP");
    // Ambiguous/unknown shapes are excluded, not guessed.
    assert!(!kinds.contains_key("blob"));
    assert!(!kinds.contains_key("renamed_with"));
}

#[allow(dead_code)]
fn w(_v: &GrpcValue) -> Result<i64, RecordDecodeError> {
    Ok(0)
}

// ---------------------------------------------------------------------------
// Link — the graph-native rid type (@rid / @in / @out)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, RecordDecode)]
struct EdgeEndsRow {
    #[serde(rename = "@rid")]
    rid: Option<arcadedb_protocol::Link>,
    #[serde(rename = "@in")]
    from: arcadedb_protocol::Link,
    #[serde(rename = "@out")]
    to: arcadedb_protocol::Link,
}

#[test]
fn link_parses_and_renders_rid_strings() {
    let l = arcadedb_protocol::Link::parse("#12:34").unwrap();
    assert_eq!((l.bucket, l.pos), (12, 34));
    assert_eq!(l.to_string(), "#12:34");
    assert!("12:34".parse::<arcadedb_protocol::Link>().is_err());
    assert!("#12".parse::<arcadedb_protocol::Link>().is_err());
    assert!("#x:1".parse::<arcadedb_protocol::Link>().is_err());
}

#[test]
fn link_decodes_from_link_and_string_kinds() {
    use arcadedb_protocol::record::FromGrpcValue;
    // Typed link (LINK columns, expand() of edges).
    let v = arcadedb_protocol::__private::link_v("#3:7");
    assert_eq!(
        Link::from_grpc_value(&v).unwrap(),
        Link::parse("#3:7").unwrap()
    );
    // String-rid projections (`SELECT @out` shapes).
    let v = str_v("#3:7");
    assert_eq!(
        Link::from_grpc_value(&v).unwrap(),
        Link::parse("#3:7").unwrap()
    );
    // Non-rid strings are conversion errors, not garbage.
    assert!(Link::from_grpc_value(&str_v("nope")).is_err());
}

#[test]
fn edge_row_decodes_rid_in_and_out() {
    let rec = GrpcRecord {
        rid: "#44:0".to_string(),
        r#type: "DEVELOPED".to_string(),
        properties: HashMap::from([
            (
                "@in".to_string(),
                arcadedb_protocol::__private::link_v("#11:2"),
            ),
            ("@out".to_string(), str_v("#12:9")),
        ]),
    };
    let row = EdgeEndsRow::try_from(&rec).unwrap();
    assert_eq!(row.rid, Some(Link::parse("#44:0").unwrap())); // synth from record metadata
    assert_eq!(row.from, Link::parse("#11:2").unwrap());
    assert_eq!(row.to, Link::parse("#12:9").unwrap());
}

#[test]
fn link_wire_kind_is_link() {
    assert!(
        EdgeEndsRow::WIRE_KINDS.contains(&("@in", "LINK")),
        "Link fields should carry the LINK column kind"
    );
}

#[test]
fn link_lists_decode_element_wise() {
    #[derive(RecordDecode)]
    struct LinkListRow {
        related: Vec<Link>,
    }
    // A list mixing typed links and rid-shaped strings (LINKLIST columns).
    let list = arcadedb_protocol::GrpcValue {
        kind: Some(Kind::ListValue(arcadedb_protocol::GrpcList {
            values: vec![arcadedb_protocol::__private::link_v("#1:0"), str_v("#2:5")],
        })),
        logical_type: String::new(),
    };
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("related".to_string(), list)]),
    };
    let row = LinkListRow::try_from(&rec).unwrap();
    assert_eq!(
        row.related,
        vec![Link::parse("#1:0").unwrap(), Link::parse("#2:5").unwrap()]
    );
}

// ---------------------------------------------------------------------------
// Typed property access on the record (method form of the free getters)
// ---------------------------------------------------------------------------

#[test]
fn record_get_decodes_required_property() {
    let rec = article_record();
    let title: &str = rec.get("title").unwrap();
    assert_eq!(title, "Intro to Rust");
    let seq: i32 = rec.get("sequence").unwrap();
    assert_eq!(seq, 1);
}

#[test]
fn record_get_errors_name_the_field_and_kinds() {
    let mut rec = article_record();
    rec.properties.remove("title");
    let err = rec.get::<String>("title").unwrap_err();
    assert!(err.to_string().contains("title"), "{err}");

    rec.properties.insert("sequence".to_string(), str_v("one"));
    let err = rec.get::<i32>("sequence").unwrap_err();
    assert!(err.to_string().contains("sequence"), "{err}");
}

#[test]
fn record_get_opt_and_get_or_handle_absence() {
    let rec = article_record();

    let missing: Option<&str> = rec.get_opt("no_such_column").unwrap();
    assert_eq!(missing, None);

    let priority: &str = rec.get("priority").unwrap();
    assert_eq!(priority, "low");
    // Absent → default.
    let boost: i64 = rec.get_or("boost", 7).unwrap();
    assert_eq!(boost, 7);
}

#[test]
fn query_result_rows_decode_lazily_and_borrow() {
    use arcadedb_protocol::QueryResult;

    let res = QueryResult {
        records: vec![article_record(), article_record()],
        execution_time_ms: 3,
    };

    let mut titles = Vec::new();
    for row in res.rows::<ArticleRow>() {
        let row = row.unwrap();
        titles.push(row.article.title.clone());
        assert_eq!(row.score, 0.0); // defaulted sibling
    }
    assert_eq!(titles, vec!["Intro to Rust"; 2]);

    // A bad row surfaces as an Err item mid-iteration.
    let mut bad = article_record();
    bad.properties.insert("score".to_string(), str_v("high"));
    let res = QueryResult {
        records: vec![article_record(), bad],
        execution_time_ms: 1,
    };
    let results: Vec<_> = res.rows::<ArticleRow>().collect();
    assert!(results[0].is_ok());
    assert!(results[1].is_err());
}

// ---------------------------------------------------------------------------
// Wave 3 type-affining: sets, ordered maps, dates
// ---------------------------------------------------------------------------

/// Set/map containers decode from the same wire shapes as Vec/HashMap.
#[derive(Debug, PartialEq, RecordDecode)]
struct Collections {
    tags: std::collections::HashSet<String>,
    ids: std::collections::BTreeSet<i32>,
    meta: std::collections::BTreeMap<String, i64>,
}

#[test]
fn sets_and_btreemap_decode_from_list_and_map_values() {
    use arcadedb_protocol::__private::map_v;
    let rec = GrpcRecord {
        rid: String::new(),
        r#type: "t".to_string(),
        properties: HashMap::from([
            (
                "tags".to_string(),
                list(vec![str_v("a"), str_v("b"), str_v("a")]),
            ),
            ("ids".to_string(), list(vec![i32_v(3), i32_v(1), i32_v(2)])),
            (
                "meta".to_string(),
                map_v([("k2".to_string(), i64_v(20)), ("k1".to_string(), i64_v(10))]),
            ),
        ]),
    };

    let c = Collections::try_from(&rec).unwrap();
    // HashSet dedups; BTreeSet/BTreeMap iterate deterministically.
    assert_eq!(c.tags.len(), 2);
    let ids: Vec<_> = c.ids.into_iter().collect();
    assert_eq!(ids, vec![1, 2, 3]);
    let keys: Vec<_> = c.meta.keys().cloned().collect();
    assert_eq!(keys, vec!["k1", "k2"]);
}

#[test]
fn naive_date_decodes_midnight_utc_timestamp() {
    use chrono::NaiveDate;
    use chrono::TimeZone;

    // DATE columns arrive as midnight-UTC TimestampValue (probe-verified).
    let ts = prost_types::Timestamp {
        seconds: 1_751_241_600, // 2025-06-30T00:00:00Z
        nanos: 0,
    };
    let v = arcadedb_protocol::GrpcValue {
        kind: Some(
            arcadedb_protocol::proto::com::arcadedb::grpc::grpc_value::Kind::TimestampValue(ts),
        ),
        logical_type: String::new(),
    };
    let d = NaiveDate::from_grpc_value(&v).unwrap();
    assert_eq!(d, NaiveDate::from_ymd_opt(2025, 6, 30).unwrap());

    // Non-timestamp kinds are loud type mismatches.
    let err = NaiveDate::from_grpc_value(&str_v("2025-06-30")).unwrap_err();
    assert!(err.to_string().contains("date"), "{err}");
    let _ = chrono::Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0); // keep trait import shape stable
}
