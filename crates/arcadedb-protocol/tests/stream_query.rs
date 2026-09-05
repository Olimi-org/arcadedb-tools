//! Streaming query consumption — the contract of [`consume_stream`]: batch
//! accounting, early-exit truncation, mid-stream error propagation, and the
//! callback scope that makes borrowed `RecordDecode` views safe without
//! callers managing record lifetimes.

use std::collections::HashMap;
use std::ops::ControlFlow;

use tokio_stream::iter as stream_iter;
use tonic::Status;

use arcadedb_protocol::__private::str_v;
use arcadedb_protocol::proto::com::arcadedb::grpc::GrpcRecord;
use arcadedb_protocol::{consume_stream, QueryResultBatch, RecordDecode, StreamSummary};

fn batch(n_records: usize, is_last: bool, running_total: i64) -> QueryResultBatch {
    QueryResultBatch {
        records: vec![GrpcRecord::default(); n_records],
        total_records_in_batch: n_records as i32,
        running_total_emitted: running_total,
        is_last_batch: is_last,
    }
}

fn record(title: &str) -> GrpcRecord {
    GrpcRecord {
        rid: String::new(),
        r#type: String::new(),
        properties: HashMap::from([("title".to_string(), str_v(title))]),
    }
}

#[tokio::test]
async fn counts_batches_and_rows_through_completion() {
    let stream = stream_iter(vec![
        Ok(batch(2, false, 2)),
        Ok(batch(3, false, 5)),
        Ok(batch(1, true, 6)),
    ]);
    let mut seen_batches = 0usize;

    let summary = consume_stream(stream, |_batch| {
        seen_batches += 1;
        ControlFlow::Continue(())
    })
    .await
    .unwrap();

    assert_eq!(seen_batches, 3);
    assert_eq!(
        summary,
        StreamSummary {
            batches: 3,
            rows: 6,
            running_total_emitted: 6,
            is_last_batch: true,
            truncated: false,
        }
    );
}

#[tokio::test]
async fn early_exit_marks_truncated_and_stops_consuming() {
    let stream = stream_iter(vec![
        Ok(batch(2, false, 2)),
        Ok(batch(3, true, 5)), // must never reach the callback
        Ok(batch(1, true, 6)),
    ]);
    let mut seen_batches = 0usize;

    let summary = consume_stream(stream, |_batch| {
        seen_batches += 1;
        ControlFlow::Break(())
    })
    .await
    .unwrap();

    assert_eq!(seen_batches, 1, "Break cancels further consumption");
    assert_eq!(summary.batches, 1);
    assert_eq!(summary.rows, 2);
    assert!(summary.truncated);
    assert!(!summary.is_last_batch);
    assert_eq!(summary.running_total_emitted, 2);
}

#[tokio::test]
async fn mid_stream_error_propagates() {
    let stream = stream_iter(vec![
        Ok(batch(1, false, 1)),
        Err(Status::internal("server exploded mid-query")),
    ]);

    let err = consume_stream(stream, |_batch| ControlFlow::Continue(()))
        .await
        .unwrap_err();

    assert!(
        err.message().contains("server exploded"),
        "status message carried through: {}",
        err.message()
    );
}

#[tokio::test]
async fn empty_stream_yields_zero_summary() {
    let summary = consume_stream(stream_iter(Vec::new()), |_b| ControlFlow::Continue(()))
        .await
        .unwrap();

    assert_eq!(
        summary,
        StreamSummary {
            batches: 0,
            rows: 0,
            running_total_emitted: 0,
            is_last_batch: false,
            truncated: false,
        }
    );
}

/// The point of the callback scope: `RecordDecode` views borrow the batch's
/// records, and the API — not a caller comment — guarantees the borrow
/// scope. Rows decoded inside the callback never escape it borrowed.
#[derive(Debug, PartialEq, RecordDecode)]
struct TitleRow {
    title: String,
}

#[tokio::test]
async fn callback_scope_supports_borrowed_decode() {
    let b = QueryResultBatch {
        records: vec![record("t1"), record("t2")],
        total_records_in_batch: 2,
        running_total_emitted: 2,
        is_last_batch: true,
    };
    let mut seen: Vec<String> = Vec::new();

    let summary = consume_stream(stream_iter(vec![Ok(b)]), |batch| {
        for rec in &batch.records {
            let row = TitleRow::try_from(rec).expect("borrowed decode in scope");
            seen.push(row.title);
        }
        ControlFlow::Continue(())
    })
    .await
    .unwrap();

    assert_eq!(seen, ["t1", "t2"]);
    assert_eq!(summary.rows, 2);
}
