//! Query result shapes — the owned [`QueryResult`] wrapper, the streaming
//! [`StreamSummary`], and the `StreamQuery` batch consumer.

use super::*;

/// One batch of streamed query results — the raw `QueryResult` proto
/// message, aliased to avoid clashing with the owned [`QueryResult`]
/// wrapper. Carries `records`, `total_records_in_batch`,
/// `running_total_emitted`, and `is_last_batch`.
pub use crate::proto::com::arcadedb::grpc::QueryResult as QueryResultBatch;

/// The result of a SQL query, bundling the returned records with server-side
/// execution timing.
#[derive(Debug)]
pub struct QueryResult {
    /// Record rows returned by the query.
    pub records: Vec<GrpcRecord>,
    /// Server-reported execution time in milliseconds.
    pub execution_time_ms: i64,
}

impl QueryResult {
    /// Number of records in the result set.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the result set is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Iterate over the records.
    pub fn iter(&self) -> Iter<'_, GrpcRecord> {
        self.records.iter()
    }

    /// Lazy, borrowing typed decode: each record → `T` on demand, with no
    /// per-row promotion to owned (string/blob views borrow from this
    /// result — consume rows within its scope, which the iterator's lifetime
    /// enforces). The method form of `records.iter().map(T::try_from)`.
    ///
    /// A decode failure surfaces as an `Err` item mid-iteration (mirroring
    /// the streaming path) rather than failing the whole call up front;
    /// collect into `Result<Vec<_>, _>` for fail-fast when ownership of all
    /// rows is wanted anyway.
    pub fn rows<'r, T>(&'r self) -> impl Iterator<Item = Result<T, RecordDecodeError>> + 'r
    where
        T: TryFrom<&'r GrpcRecord, Error = RecordDecodeError> + 'r,
    {
        self.records.iter().map(T::try_from)
    }

    /// Typed decode of the first row, `None` when the result is empty — the
    /// `fetch_optional` shape for callers that already hold a
    /// [`QueryResult`]. A decode failure surfaces as `Err` (the row existed
    /// but did not match `T`), the empty result as `Ok(None)`.
    pub fn first<T>(&self) -> std::result::Result<Option<T>, RecordDecodeError>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        self.records.first().map(T::try_from).transpose()
    }

    /// Typed decode of the first row, with an empty result set an
    /// [`ArcadeDbError::NotFound`] — the one-row contract without repeating
    /// `records.into_iter().next()` + hand-rolled error at every call site.
    pub fn one<T>(&self) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        self.first()
            .map_err(|e| ArcadeDbError::Decode {
                what: "first row".into(),
                source: e,
            })?
            .ok_or_else(|| ArcadeDbError::NotFound {
                operation: "query".into(),
                detail: "result set was empty".into(),
            })
    }

    /// Eager parallel decode of the whole result set (requires the `rayon`
    /// feature): `rows().collect()` semantics on the global rayon pool,
    /// with one contract difference — this is a collect, not an iterator,
    /// so the first `Err` short-circuits (after in-flight items drain).
    ///
    /// Below `par_min_records()` the sequential path runs instead: task
    /// overhead would eat the gain on point lookups and small candidate
    /// lists, and opting into the feature should never regress them.
    #[cfg(feature = "rayon")]
    pub fn par_rows<T>(&self) -> Result<Vec<T>, RecordDecodeError>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError> + Send,
    {
        use rayon::prelude::*;
        if self.records.len() < par_min_records() {
            return self.records.iter().map(T::try_from).collect();
        }
        self.records
            .par_iter()
            .map(T::try_from)
            .collect::<Result<Vec<_>, _>>()
    }
}

/// Compiled-in default for the parallel-path batch threshold; override at
/// runtime via `ARCADEDB_PAR_MIN_RECORDS` (see [`par_min_records`]) — the
/// right number depends on row width, wide rows pay off earlier.
#[cfg(feature = "rayon")]
pub const DEFAULT_PAR_MIN_RECORDS: usize = 512;

/// Effective parallel-path batch threshold, resolved once. Env override:
/// `ARCADEDB_PAR_MIN_RECORDS=<n>` (invalid/zero values fall back to the
/// compiled default).
#[cfg(feature = "rayon")]
pub fn par_min_records() -> usize {
    static OVERRIDE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("ARCADEDB_PAR_MIN_RECORDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PAR_MIN_RECORDS)
    })
}

impl Deref for QueryResult {
    type Target = Vec<GrpcRecord>;

    fn deref(&self) -> &Self::Target {
        &self.records
    }
}

impl IntoIterator for QueryResult {
    type Item = GrpcRecord;
    type IntoIter = IntoIter<GrpcRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.records.into_iter()
    }
}

/// Accounting for a consumed `StreamQuery` stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamSummary {
    /// Batches consumed before completion or early exit.
    pub batches: u32,
    /// Total records across the consumed batches.
    pub rows: usize,
    /// Server-side running total from the last consumed batch.
    pub running_total_emitted: i64,
    /// Whether the last consumed batch was flagged final by the server.
    pub is_last_batch: bool,
    /// Whether consumption stopped early via `ControlFlow::Break` — the
    /// stream was dropped, cancelling the RPC (h2 cancel), and the server
    /// may have more batches it never sent.
    pub truncated: bool,
}
