//! Server-streaming queries — the `StreamQuery` RPC: per-batch consumption,
//! typed row streaming, and the testable [`consume_stream`] core.

use super::*;

/// Consume a `StreamQuery` batch stream through `on_batch`, one batch at a
/// time. This is the testable core of
/// [`ArcadeDbClient::stream_query`](crate::ArcadeDbClient::stream_query):
/// generic over any stream of proto batches, so tests drive it with fakes
/// instead of a live server.
///
/// The callback's borrow of each batch ends when it returns — borrowed
/// `RecordDecode` views must be consumed (or converted to owned) inside the
/// call; the API, not a caller-side comment, enforces the scope. Returning
/// [`ControlFlow::Break`] stops pulling, drops the stream (cancelling the
/// RPC), and marks the returned summary `truncated`.
pub async fn consume_stream<S, F>(
    stream: S,
    mut on_batch: F,
) -> std::result::Result<StreamSummary, tonic::Status>
where
    S: Stream<Item = std::result::Result<QueryResultBatch, tonic::Status>> + Unpin,
    F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
{
    let mut stream = stream;
    let mut summary = StreamSummary::default();
    while let Some(item) = stream.next().await {
        let batch = item?;
        summary.batches += 1;
        summary.rows += batch.records.len();
        summary.running_total_emitted = batch.running_total_emitted;
        summary.is_last_batch = batch.is_last_batch;
        if on_batch(&batch).is_break() {
            summary.truncated = true;
            return Ok(summary);
        }
    }
    Ok(summary)
}

impl ArcadeDbClient {
    /// Execute a SQL query on the server-streaming RPC (`StreamQuery`),
    /// feeding each result batch to `on_batch` as it arrives — ranking,
    /// filtering, and discarding happen per batch while the server produces
    /// the next one, instead of materializing the entire result set
    /// client-side first.
    ///
    /// The callback contract is [`consume_stream`]'s: borrowed views are
    /// consumed inside the call, and `ControlFlow::Break` cancels the stream
    /// early (the summary reports `truncated`). Prefer this over
    /// [`query`](Self::query) for large candidate sets; keep `query` for
    /// small reads.
    pub async fn stream_query<F>(
        &self,
        database: &str,
        query: &str,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        self.stream_query_impl(
            database,
            query,
            crate::encode::Params::default(),
            crate::stream::StreamOptions::default(),
            on_batch,
        )
        .await
    }

    /// [`stream_query`](Self::stream_query) with bound `:name` parameters —
    /// same parameter-safety and plan-caching notes as
    /// [`query_with_values`](Self::query_with_values).
    pub async fn stream_query_with_values<F>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        let parameters = parameters.into();
        self.stream_query_impl(
            database,
            query,
            parameters,
            crate::stream::StreamOptions::default(),
            on_batch,
        )
        .await
    }

    /// [`stream_query_with_values`](Self::stream_query_with_values) with
    /// explicit execution knobs — the
    /// [`StreamOptions`](crate::stream::StreamOptions) knobs
    /// (`batch_size`, `retrieval_mode`, projections) the plain methods
    /// leave at server defaults.
    pub async fn stream_query_with_options<F>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        options: crate::stream::StreamOptions,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        self.stream_query_impl(database, query, parameters.into(), options, on_batch)
            .await
    }

    /// Typed row streaming: each record is decoded into `T` (the
    /// [`RecordDecode`](crate::RecordDecode) bound) and passed to `on_row`
    /// as it arrives — the [`rows`](QueryResult::rows) contract without
    /// buffering the result set.
    ///
    /// `ControlFlow::Break` stops the stream early (summary reports
    /// `truncated`). A row that does not decode fails the whole call with
    /// [`ArcadeDbError::Decode`] — fail-fast, matching the buffered path's
    /// whole-query error semantics.
    pub async fn stream_query_rows<T, F>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        on_row: F,
    ) -> Result<StreamSummary>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
        F: FnMut(T) -> ControlFlow<()>,
    {
        self.stream_query_rows_impl(
            database,
            query,
            parameters,
            crate::stream::StreamOptions::default(),
            on_row,
        )
        .await
    }

    /// [`stream_query_rows`](Self::stream_query_rows) with explicit
    /// [`StreamOptions`](crate::stream::StreamOptions).
    pub async fn stream_query_rows_with_options<T, F>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        options: crate::stream::StreamOptions,
        on_row: F,
    ) -> Result<StreamSummary>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
        F: FnMut(T) -> ControlFlow<()>,
    {
        self.stream_query_rows_impl(database, query, parameters, options, on_row)
            .await
    }

    pub(crate) async fn stream_query_impl<F>(
        &self,
        database: &str,
        query: &str,
        parameters: crate::encode::Params,
        options: crate::stream::StreamOptions,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        let req = tonic::Request::new(StreamQueryRequest {
            database: database.into(),
            query: query.into(),
            parameters: parameters.0,
            credentials: Some(self.auth.creds.clone()),
            language: "sql".into(),
            batch_size: options.batch_size,
            retrieval_mode: options.retrieval_mode.map_or(0, |m| m as i32),
            projection_settings: options.projection,
            ..Default::default()
        });

        let streaming = self
            .data
            .clone()
            .stream_query(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("stream_query", query, e))?
            .into_inner();

        let summary = consume_stream(streaming, on_batch)
            .await
            .map_err(|e| map_rpc_error("stream_query", query, e))?;

        tracing::info!(
            count = summary.rows,
            batches = summary.batches,
            truncated = summary.truncated,
            query = %query,
            "stream query results"
        );

        Ok(summary)
    }

    /// Typed-row streaming core: wraps [`stream_query_impl`](Self::
    /// stream_query_impl)'s batch callback with per-row decode. Decode
    /// failure and callback `Break` both stop the stream; the decode failure
    /// wins as the returned error (fail-fast, never half-reported).
    pub(crate) async fn stream_query_rows_impl<T, F>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        options: crate::stream::StreamOptions,
        mut on_row: F,
    ) -> Result<StreamSummary>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
        F: FnMut(T) -> ControlFlow<()>,
    {
        let mut row_error: Option<ArcadeDbError> = None;
        let mut stopped = false;
        let result = self
            .stream_query_impl(database, query, parameters.into(), options, |batch| {
                if stopped {
                    return ControlFlow::Break(());
                }
                for record in &batch.records {
                    match T::try_from(record) {
                        Ok(row) => {
                            if let ControlFlow::Break(()) = on_row(row) {
                                stopped = true;
                                return ControlFlow::Break(());
                            }
                        }
                        Err(e) => {
                            row_error = Some(ArcadeDbError::Decode {
                                what: format!("stream row of `{query}`"),
                                source: e,
                            });
                            stopped = true;
                            return ControlFlow::Break(());
                        }
                    }
                }
                ControlFlow::Continue(())
            })
            .await;

        if let Some(e) = row_error {
            return Err(e);
        }
        result
    }
}
