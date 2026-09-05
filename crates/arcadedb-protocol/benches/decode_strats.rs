//! Zero-copy decode benchmark for the `FromGrpcValue<'de>` trait change.
//!
//! The same wire `GrpcRecord`, decoded two ways plus the wire-copy context:
//!
//! - `owned`    — `String` fields: every string is cloned out of the value
//! - `borrowed` — `&'de str` fields: zero-copy views into the record
//! - `cow`      — `Cow<'de, str>` fields: borrow when possible, own otherwise
//! - `wire_prost_decode` — the prost wire→`GrpcRecord` allocation (the copy
//!   this crate cannot remove without a custom tonic codec), measured so the
//!   per-field copy delta is visible in context
//!
//! Distinct shapes exercise different data: a wide flat row, a list of nested
//! embedded records, and a map column (per-key allocation on the owned path).

// Decode results are only `black_box`-consumed (never field-read), so the
// fixture DTOs' fields are "dead" to the linter.
#![allow(dead_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use prost::Message;
use serde::Deserialize;

use arcadedb_protocol::proto::com::arcadedb::grpc::{
    grpc_value::Kind, GrpcList, GrpcRecord, GrpcValue,
};
use arcadedb_protocol::RecordDecode;

// ---------------------------------------------------------------------------
// Shapes — DTO triples (owned / borrowed / cow) mirroring real consumer rows
// ---------------------------------------------------------------------------

/// Wide flat row: 8 string columns + a float. Field idents differ from the
/// wire keys where the derived DTO would carry `#[serde(rename)]` in
/// production (`updated_at` → `published_at`).
#[derive(Debug, Deserialize, RecordDecode)]
struct WideOwned {
    title: String,
    subtitle: String,
    author: String,
    description: String,
    seo: String,
    #[serde(rename = "updated_at")]
    published_at: String,
    locale: String,
    publisher: String,
    #[serde(default)]
    score: f64,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct WideBorrowed<'a> {
    title: &'a str,
    subtitle: &'a str,
    author: &'a str,
    description: &'a str,
    seo: &'a str,
    #[serde(rename = "updated_at")]
    published_at: &'a str,
    locale: &'a str,
    publisher: &'a str,
    score: f64,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct WideCow<'a> {
    title: Cow<'a, str>,
    subtitle: Cow<'a, str>,
    author: Cow<'a, str>,
    description: Cow<'a, str>,
    seo: Cow<'a, str>,
    #[serde(rename = "updated_at")]
    published_at: Cow<'a, str>,
    locale: Cow<'a, str>,
    publisher: Cow<'a, str>,
    score: f64,
}

/// Nested shape: a list of embedded records (`Vec<Step>`), 3 strings each;
/// the wire key `content_type` is renamed to `kind` (as the consumer models
/// do with `#[serde(rename)]` on enum-ish string columns).
#[derive(Debug, Deserialize, RecordDecode)]
struct BlockOwned {
    content_rid: String,
    #[serde(rename = "content_type")]
    kind: String,
    #[serde(default)]
    hint: String,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct NestedOwned {
    title: String,
    sections: Vec<BlockOwned>,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct BlockBorrowed<'a> {
    content_rid: &'a str,
    #[serde(rename = "content_type")]
    kind: &'a str,
    hint: &'a str,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct NestedBorrowed<'a> {
    title: &'a str,
    sections: Vec<BlockBorrowed<'a>>,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct BlockCow<'a> {
    content_rid: Cow<'a, str>,
    #[serde(rename = "content_type")]
    kind: Cow<'a, str>,
    hint: Cow<'a, str>,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct NestedCow<'a> {
    title: Cow<'a, str>,
    sections: Vec<BlockCow<'a>>,
}

/// Map column (`HashMap<String, String>`), renamed ident →
/// `name_translations` exactly like the crate's `Localized` test shape.
/// Per-key + per-value allocation on the owned path.
#[derive(Debug, Deserialize, RecordDecode)]
struct MapOwned {
    #[serde(rename = "name_translations")]
    name_translations: HashMap<String, String>,
}

#[derive(Debug, Deserialize, RecordDecode)]
struct MapCow<'a> {
    #[serde(rename = "name_translations")]
    name_translations: HashMap<String, Cow<'a, str>>,
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn gv(kind: Kind) -> GrpcValue {
    GrpcValue {
        kind: Some(kind),
        logical_type: String::new(),
    }
}

fn v_str(s: &str) -> GrpcValue {
    gv(Kind::StringValue(s.to_string()))
}

fn v_list(vals: Vec<GrpcValue>) -> GrpcValue {
    gv(Kind::ListValue(GrpcList { values: vals }))
}

fn v_f64(n: f64) -> GrpcValue {
    gv(Kind::DoubleValue(n))
}

fn wide_record() -> GrpcRecord {
    GrpcRecord {
        rid: "#42:0".to_string(),
        r#type: "article".to_string(),
        properties: HashMap::from([
            (
                "title".to_string(),
                v_str("Zero-copy record decoding in Rust"),
            ),
            (
                "subtitle".to_string(),
                v_str("A downside of halfway-typed bridges"),
            ),
            ("author".to_string(), v_str("Ada Lovelace")),
            (
                "description".to_string(),
                v_str("How a lifetime-parameterized FromGrpcValue changes the allocation profile."),
            ),
            (
                "seo".to_string(),
                v_str("rust;arcadedb;grpc;zero-copy;decoding"),
            ),
            ("updated_at".to_string(), v_str("2026-08-19T09:41:00Z")),
            ("locale".to_string(), v_str("en-US")),
            ("publisher".to_string(), v_str("Arcade Labs")),
            ("score".to_string(), v_f64(9.5)),
        ]),
    }
}

fn nested_record() -> GrpcRecord {
    let steps: Vec<GrpcValue> = (0..4)
        .map(|i| {
            gv(Kind::EmbeddedValue(
                arcadedb_protocol::proto::com::arcadedb::grpc::GrpcEmbedded {
                    r#type: "block".to_string(),
                    fields: HashMap::from([
                        ("content_rid".to_string(), v_str(&format!("#11:{i}"))),
                        ("content_type".to_string(), v_str("entry")),
                        ("hint".to_string(), v_str("a helpful hint")),
                    ]),
                },
            ))
        })
        .collect();
    GrpcRecord {
        rid: "#12:0".to_string(),
        r#type: "article".to_string(),
        properties: HashMap::from([
            ("title".to_string(), v_str("Intro to Rust")),
            ("sections".to_string(), v_list(steps)),
        ]),
    }
}

fn map_record() -> GrpcRecord {
    GrpcRecord {
        rid: "#7:3".to_string(),
        r#type: "page".to_string(),
        properties: HashMap::from([(
            "name_translations".to_string(),
            gv(Kind::MapValue(
                arcadedb_protocol::proto::com::arcadedb::grpc::GrpcMap {
                    entries: [
                        ("en", "Silent Harbor"),
                        ("pt", "Porto Silencioso"),
                        ("es", "Puerto Silencioso"),
                        ("fr", "Havre Silencieux"),
                        ("de", "Stiller Hafen"),
                        ("ja", "\u{65e5}\u{672c}\u{8a9e}"), // Japanese (kept for multibyte coverage)
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v_str(v)))
                    .collect(),
                },
            )),
        )]),
    }
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn wide(c: &mut Criterion) {
    let rec = wide_record();
    let mut g = c.benchmark_group("wide");
    g.sample_size(500);

    let proto = rec.encode_to_vec();
    g.bench_function("wire_prost_decode", |b| {
        b.iter(|| black_box(GrpcRecord::decode(black_box(proto.as_slice())).unwrap()))
    });

    g.bench_function("derive_owned", |b| {
        b.iter(|| black_box(WideOwned::try_from(black_box(&rec)).unwrap()))
    });
    g.bench_function("derive_borrowed", |b| {
        b.iter(|| black_box(WideBorrowed::try_from(black_box(&rec)).unwrap()))
    });
    g.bench_function("derive_cow", |b| {
        b.iter(|| black_box(WideCow::try_from(black_box(&rec)).unwrap()))
    });
}

fn nested(c: &mut Criterion) {
    let rec = nested_record();
    let mut g = c.benchmark_group("nested");
    g.sample_size(500);

    g.bench_function("derive_owned", |b| {
        b.iter(|| black_box(NestedOwned::try_from(black_box(&rec)).unwrap()))
    });
    g.bench_function("derive_borrowed", |b| {
        b.iter(|| black_box(NestedBorrowed::try_from(black_box(&rec)).unwrap()))
    });
    g.bench_function("derive_cow", |b| {
        b.iter(|| black_box(NestedCow::try_from(black_box(&rec)).unwrap()))
    });
}

fn map(c: &mut Criterion) {
    let rec = map_record();
    let mut g = c.benchmark_group("map");
    g.sample_size(500);

    g.bench_function("derive_owned", |b| {
        b.iter(|| black_box(MapOwned::try_from(black_box(&rec)).unwrap()))
    });
    g.bench_function("derive_cow", |b| {
        b.iter(|| black_box(MapCow::try_from(black_box(&rec)).unwrap()))
    });
}

criterion_group!(benches, wide, nested, map);
criterion_main!(benches);
