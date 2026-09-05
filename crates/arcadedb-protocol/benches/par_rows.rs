//! Parallel decode benchmark (`rayon` feature): `rows().collect()` vs
//! `par_rows()` on the same wire batches, across sizes straddling the
//! `DEFAULT_PAR_MIN_RECORDS` threshold (512), plus the threshold-sensitivity
//! of the sequential fallback.
//!
//! Shapes reuse `decode_strats.rs`'s finding that the owned decode is the
//! expensive strategy — parallelizing it is only interesting where it
//! dominates. `WideOwned` (8 string columns) mirrors real wide rows,
//! `NarrowOwned` (2 columns) mirrors slim rows where the
//! threshold should keep us sequential.
//!
//! Run:
//! ```text
//! cargo bench -p arcadedb-protocol --features rayon --bench par_rows
//! ```

// Decode results are only `black_box`-consumed; the fixture DTOs' fields
// are "dead" to the linter.
#![allow(dead_code)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde::Deserialize;

use arcadedb_protocol::proto::com::arcadedb::grpc::{
    grpc_value::Kind, GrpcList, GrpcRecord, GrpcValue,
};
use arcadedb_protocol::{QueryResult, RecordDecode};

/// Wide row — the shape where parallel decode should pay.
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

/// Slim row — the shape the threshold must route sequentially.
#[derive(Debug, Deserialize, RecordDecode)]
struct NarrowOwned {
    item_id: i64,
    name: String,
}

fn str_v(s: &str) -> GrpcValue {
    GrpcValue {
        kind: Some(Kind::StringValue(s.into())),
        ..Default::default()
    }
}

fn wide_record(i: usize) -> GrpcRecord {
    GrpcRecord {
        properties: [
            (
                "title",
                format!("Field notes {i} — a practical handbook"),
            ),
            (
                "subtitle",
                format!("Notes from the field, volume {i}"),
            ),
            ("author", format!("Jane Doe {i}")),
            (
                "description",
                format!("Handbook with {i} chapters and an index"),
            ),
            ("seo", format!("handbook,reference,{i}")),
            ("updated_at", format!("2026-08-{i:02} 10:00:00")),
            ("locale", "en".into()),
            ("publisher", format!("Acme Press {i}")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), str_v(&v)))
        .collect(),
        ..Default::default()
    }
}

fn narrow_record(i: usize) -> GrpcRecord {
    GrpcRecord {
        properties: [
            (
                "item_id".to_string(),
                GrpcValue {
                    kind: Some(Kind::Int64Value(i as i64)),
                    ..Default::default()
                },
            ),
            (
                "name".to_string(),
                str_v(&format!("Item number {i} with a moderately long title")),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

/// A record carrying list columns too — list decode is per-element work.
#[derive(Debug, Deserialize, RecordDecode)]
struct ListOwned {
    item_id: i64,
    #[serde(default)]
    label_ids: Vec<i32>,
    #[serde(default)]
    images: Vec<String>,
}

fn list_record(i: usize) -> GrpcRecord {
    let tags = GrpcList {
        values: (0..20)
            .map(|t| GrpcValue {
                kind: Some(Kind::Int32Value(((i + t) % 400) as i32)),
                ..Default::default()
            })
            .collect(),
    };
    let imgs = GrpcList {
        values: (0..5)
                .map(|s| str_v(&format!("https://cdn.example/img/{i}/{s}.jpg")))
            .collect(),
    };
    GrpcRecord {
        properties: [
            (
                "item_id".to_string(),
                GrpcValue {
                    kind: Some(Kind::Int64Value(i as i64)),
                    ..Default::default()
                },
            ),
            (
                "label_ids".to_string(),
                GrpcValue {
                    kind: Some(Kind::ListValue(tags)),
                    ..Default::default()
                },
            ),
            (
                "images".to_string(),
                GrpcValue {
                    kind: Some(Kind::ListValue(imgs)),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

fn bench_decode<T>(c: &mut Criterion, name: &str, make: fn(usize) -> GrpcRecord, sizes: &[usize])
where
    T: for<'r> TryFrom<&'r GrpcRecord, Error = arcadedb_protocol::RecordDecodeError> + Send,
{
    let mut group = c.benchmark_group(format!("decode/{name}"));
    for &n in sizes {
        let result = QueryResult {
            records: (0..n).map(make).collect(),
            execution_time_ms: 0,
        };
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("seq", n), &result, |b, r| {
            b.iter(|| {
                black_box(
                    r.records
                        .iter()
                        .map(T::try_from)
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                )
            });
        });
        group.bench_with_input(BenchmarkId::new("par", n), &result, |b, r| {
            b.iter(|| black_box(r.par_rows::<T>().unwrap()));
        });
    }
    group.finish();
}

fn bench_par_rows(c: &mut Criterion) {
    // Sizes straddle the default threshold (512) by an order of magnitude
    // on both sides so the crossover is visible.
    let sizes = &[64usize, 512, 2_048, 8_192];
    bench_decode::<WideOwned>(c, "wide_8str", wide_record, sizes);
    bench_decode::<NarrowOwned>(c, "narrow_2col", narrow_record, sizes);
    bench_decode::<ListOwned>(c, "lists_20x5", list_record, sizes);
}

criterion_group!(benches, bench_par_rows);
criterion_main!(benches);
