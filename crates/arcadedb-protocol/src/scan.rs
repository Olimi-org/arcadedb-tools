//! Full-type/bucket iteration — the [`ArcadeDbClient::scan`] facade.
//!
//! A scan is `SELECT FROM {type}` consumed batch-by-batch. Type scans are
//! polymorphic by default (subtypes included).
//!
//! Type/bucket names are validated identifiers; row values can be bound
//! parameters.

use std::ops::ControlFlow;

use crate::client::{validate_identifier, ArcadeDbClient, StreamSummary};
use crate::error::Result;

/// Full-type/bucket iteration. Obtain via [`ArcadeDbClient::scan`].
#[derive(Clone)]
pub struct Scan {
    client: ArcadeDbClient,
}

impl Scan {
    pub(crate) fn new(client: ArcadeDbClient) -> Self {
        Self { client }
    }

    /// Stream every record of `type_name` (polymorphic) through `on_batch`.
    /// `ControlFlow::Break` cancels early.
    pub async fn type_<F>(
        &self,
        database: &str,
        type_name: &str,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&crate::QueryResultBatch) -> ControlFlow<()>,
    {
        let t = validate_identifier(type_name, "type name")?;
        self.client
            .stream_query_impl(
                database,
                &format!("SELECT FROM {t}"),
                Default::default(),
                crate::stream::StreamOptions::default(),
                on_batch,
            )
            .await
    }

    /// [`type_`](Self::type_) with bound `:name` parameters.
    pub async fn type_with_values<F>(
        &self,
        database: &str,
        type_name: &str,
        parameters: impl Into<crate::encode::Params>,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&crate::QueryResultBatch) -> ControlFlow<()>,
    {
        self.type_with_options(
            database,
            type_name,
            parameters,
            crate::StreamOptions::default(),
            on_batch,
        )
        .await
    }

    /// [`type_`](Self::type_) with bound `:name` parameters and explicit
    /// [`StreamOptions`](crate::StreamOptions).
    pub async fn type_with_options<F>(
        &self,
        database: &str,
        type_name: &str,
        parameters: impl Into<crate::encode::Params>,
        options: crate::StreamOptions,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&crate::QueryResultBatch) -> ControlFlow<()>,
    {
        let t = validate_identifier(type_name, "type name")?;
        self.client
            .stream_query_impl(
                database,
                &format!("SELECT FROM {t}"),
                parameters.into(),
                options,
                on_batch,
            )
            .await
    }

    /// Stream every record in one bucket (no subtype fan-out).
    pub async fn bucket<F>(
        &self,
        database: &str,
        bucket_name: &str,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&crate::QueryResultBatch) -> ControlFlow<()>,
    {
        let b = validate_identifier(bucket_name, "bucket name")?;
        self.client
            .stream_query_impl(
                database,
                &format!("SELECT FROM bucket:{b}"),
                Default::default(),
                crate::stream::StreamOptions::default(),
                on_batch,
            )
            .await
    }
}
