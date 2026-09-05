//! Streaming query options — the
//! [`ArcadeDbClient::stream_query_with_options`](crate::ArcadeDbClient::stream_query_with_options)
//! builder.
//!
//! The wire's `StreamQueryRequest` carries execution knobs the pre-0.3 API
//! left at defaults: server `batch_size`, the
//! [`RetrievalMode`] (CURSOR / MATERIALIZE_ALL / PAGED), and projection
//! settings. [`StreamOptions`] surfaces them; the existing
//! `stream_query`/`stream_query_with_values` keep their signatures and run
//! with server defaults.

use crate::proto::com::arcadedb::grpc::ProjectionSettings;

// Canonical import path for the wire enum:
// `arcadedb_protocol::stream::RetrievalMode`.
pub use crate::proto::com::arcadedb::grpc::stream_query_request::RetrievalMode;

/// Execution options for the server-streaming query RPC. Default is
/// server-side defaults (batch sizing, CURSOR retrieval, full records).
///
/// ```no_run
/// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
/// use arcadedb_protocol::{Params, stream::{RetrievalMode, StreamOptions}};
/// use std::ops::ControlFlow;
///
/// let summary = client
///     .stream_query_with_options(
///         "mydb",
///         "SELECT FROM item",
///         Params::default(),
///         StreamOptions::new()
///             .batch_size(50_000)
///             .retrieval_mode(RetrievalMode::Paged),
///         |_batch| ControlFlow::Continue(()),
///     )
///     .await?;
/// println!("{} rows in {} batches", summary.rows, summary.batches);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    /// Server-side batch size; `0` = server default.
    pub(crate) batch_size: i32,
    pub(crate) retrieval_mode: Option<RetrievalMode>,
    pub(crate) projection: Option<ProjectionSettings>,
}

impl StreamOptions {
    /// Server defaults (equivalent to the plain `stream_query` methods).
    pub fn new() -> Self {
        Self::default()
    }

    /// Records per streamed batch: larger batches amortize per-batch
    /// overhead, smaller batches keep memory flat.
    pub fn batch_size(mut self, size: u32) -> Self {
        self.batch_size = size as i32;
        self
    }

    /// Wire retrieval mode:
    /// - `Cursor` (server default) — run once, stream while iterating.
    /// - `MaterializeAll` — server loads the full result, then emits batches.
    /// - `Paged` — server re-issues `LIMIT/SKIP` per batch (consistent
    ///   snapshots of moving data, at the cost of re-execution).
    pub fn retrieval_mode(mut self, mode: RetrievalMode) -> Self {
        self.retrieval_mode = Some(mode);
        self
    }

    /// Server-side projection settings (field selection — the proto's
    /// `ProjectionSettings`), when the query's shape should be narrowed
    /// without rewriting the SQL text.
    pub fn projection(mut self, settings: ProjectionSettings) -> Self {
        self.projection = Some(settings);
        self
    }
}
