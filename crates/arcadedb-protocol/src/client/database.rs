//! Bound handle — [`DatabaseClient`], a client pinned to one database so
//! call sites drop the `database: &str` parameter.

use super::*;

// ---------------------------------------------------------------------------
// Bound handle — a client pinned to one database
// ---------------------------------------------------------------------------

/// An [`ArcadeDbClient`] bound to one database: every delegated method drops
/// the `database: &str` parameter. The connect-time name is the single source
/// of truth, which removes the per-call-site repetition *and* the wrong-db
/// mistake class.
///
/// ```ignore
/// let db = ArcadeDbClient::connect(addr, user, pass, "mydb").await?;
/// let res = db.query("SELECT FROM item LIMIT 10").await?;
/// ```
///
/// Un-delegated methods stay reachable through `Deref<Target =
/// ArcadeDbClient>` (with the explicit parameter).
#[derive(Clone)]
pub struct DatabaseClient {
    client: ArcadeDbClient,
    database: String,
}

impl std::ops::Deref for DatabaseClient {
    type Target = ArcadeDbClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl std::ops::DerefMut for DatabaseClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

impl ArcadeDbClient {
    /// Bind this client to `database`, producing a [`DatabaseClient`] whose
    /// methods omit the database parameter. Cheap: clones the channel handle.
    pub fn to_database(&self, database: impl Into<String>) -> DatabaseClient {
        DatabaseClient {
            client: self.clone(),
            database: database.into(),
        }
    }
}

impl DatabaseClient {
    /// The multi-statement facade, database-bound (see [`crate::batch::Batch`]).
    pub fn batch(&self) -> crate::batch::Batch {
        self.client.batch()
    }

    /// [`ArcadeDbClient::connect`] straight into a bound handle.
    pub async fn connect(
        addr: &str,
        username: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        Ok(ArcadeDbClient::connect(addr, username, password, database)
            .await?
            .to_database(database))
    }

    /// The bound database name.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// SQL read.
    pub async fn query(&self, query: &str) -> Result<QueryResult> {
        self.client.query(&self.database, query).await
    }

    /// Parameterized SQL read (`params! { .. }` or a DTO's `to_param_map()`).
    pub async fn query_with_values(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<QueryResult> {
        self.client
            .query_with_values(&self.database, query, parameters)
            .await
    }

    /// SQL DDL/DML command.
    pub async fn execute(&self, command: &str) -> Result<ExecuteCommandResponse> {
        self.client.execute(&self.database, command).await
    }

    /// Parameterized SQL command.
    pub async fn execute_with_values(
        &self,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_with_values(&self.database, command, parameters)
            .await
    }

    /// Server-streaming SQL read.
    pub async fn stream_query<F>(&self, query: &str, on_batch: F) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        self.client
            .stream_query(&self.database, query, on_batch)
            .await
    }

    /// Parameterized server-streaming SQL read.
    pub async fn stream_query_with_values<F>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        self.client
            .stream_query_with_values(&self.database, query, parameters, on_batch)
            .await
    }

    /// Direct record fetch by rid.
    pub async fn lookup_by_rid(&self, rid: &crate::Link) -> Result<Option<GrpcRecord>> {
        self.client.lookup_by_rid(&self.database, rid).await
    }

    /// Typed single-record fetch.
    pub async fn lookup<T>(&self, rid: &crate::Link) -> Result<Option<T>>
    where
        T: for<'a> TryFrom<&'a GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client.lookup(&self.database, rid).await
    }

    /// Create a record from `(property, value)` pairs; returns the new rid.
    pub async fn create_record(
        &self,
        class_name: &str,
        props: Vec<(&str, crate::GrpcValue)>,
    ) -> Result<CreateRecordResponse> {
        self.client
            .create_record(&self.database, class_name, props)
            .await
    }

    /// Partially update a record by rid.
    pub async fn update_record_partial(
        &self,
        rid: &crate::Link,
        props: impl Into<crate::encode::Params>,
    ) -> Result<bool> {
        self.client
            .update_record_partial(&self.database, rid, props)
            .await
    }

    /// [`stream_query_with_values`](Self::stream_query_with_values) with
    /// explicit execution knobs (bound database).
    pub async fn stream_query_with_options<F>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        options: crate::stream::StreamOptions,
        on_batch: F,
    ) -> Result<StreamSummary>
    where
        F: FnMut(&QueryResultBatch) -> ControlFlow<()>,
    {
        self.client
            .stream_query_with_options(&self.database, query, parameters, options, on_batch)
            .await
    }

    /// Typed row streaming (bound database) — decoded rows passed to
    /// `on_row` as they arrive, no result-set buffering.
    pub async fn stream_query_rows<T, F>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
        on_row: F,
    ) -> Result<StreamSummary>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
        F: FnMut(T) -> ControlFlow<()>,
    {
        self.client
            .stream_query_rows(&self.database, query, parameters, on_row)
            .await
    }

    /// Delete a record by rid.
    pub async fn delete_record(&self, rid: &crate::Link) -> Result<bool> {
        self.client.delete_record(&self.database, rid).await
    }

    /// Typed fetch of at most one row (bound database).
    pub async fn fetch_optional<T>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client
            .fetch_optional(&self.database, query, parameters)
            .await
    }

    /// Typed one-row fetch (bound database); empty result is
    /// [`ArcadeDbError::NotFound`].
    pub async fn fetch_one<T>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client
            .fetch_one(&self.database, query, parameters)
            .await
    }

    /// Single-column scalar decode (bound database).
    pub async fn query_scalar<T>(
        &self,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'de> crate::record::FromGrpcValue<'de>,
    {
        self.client
            .query_scalar(&self.database, query, parameters)
            .await
    }

    /// Write command returning the stored row, decoded (bound database);
    /// `Ok(None)` when the statement matched nothing.
    pub async fn write_returning_optional<T>(
        &self,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client
            .write_returning_optional(&self.database, command, parameters)
            .await
    }

    /// Write command returning the stored row, decoded (bound database).
    pub async fn upsert_returning<T>(
        &self,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client
            .upsert_returning(&self.database, command, parameters)
            .await
    }

    /// Typed lookup by (composite) key (bound database).
    pub async fn lookup_by_key<T>(
        &self,
        type_name: &str,
        key: &[(&str, crate::GrpcValue)],
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = crate::record::RecordDecodeError>,
    {
        self.client
            .lookup_by_key(&self.database, type_name, key)
            .await
    }

    /// Count all records of a type (bound database).
    pub async fn count_type(&self, type_name: &str) -> Result<i64> {
        self.client.count_type(&self.database, type_name).await
    }

    /// Retrying transaction closure on the bound database.
    pub async fn run_transaction<T, F, Fut>(
        &self,
        isolation: TransactionIsolation,
        retries: u32,
        body: F,
    ) -> Result<T>
    where
        F: Fn(TxCommands) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.client
            .run_transaction(&self.database, isolation, retries, body)
            .await
    }

    /// Idempotent bulk insert (ignore-on-conflict).
    pub async fn bulk_insert(
        &self,
        target_class: &str,
        key_columns: &[&str],
        rows: Vec<GrpcRecord>,
    ) -> Result<InsertSummary> {
        self.client
            .bulk_insert(&self.database, target_class, key_columns, rows)
            .await
    }

    /// Bulk upsert (update-on-conflict).
    pub async fn bulk_upsert(
        &self,
        target_class: &str,
        key_columns: &[&str],
        update_columns_on_conflict: &[&str],
        rows: Vec<GrpcRecord>,
    ) -> Result<InsertSummary> {
        self.client
            .bulk_upsert(
                &self.database,
                target_class,
                key_columns,
                update_columns_on_conflict,
                rows,
            )
            .await
    }

    /// Typed bulk upsert from `ToGrpcRecord` DTOs.
    pub async fn bulk_upsert_dtos<T>(
        &self,
        target_class: &str,
        rows: Vec<T>,
    ) -> Result<InsertSummary>
    where
        T: crate::record::ToGrpcRecord + Send,
    {
        self.client
            .bulk_upsert_dtos(&self.database, target_class, rows)
            .await
    }

    /// Begin a drop-safe transaction on the bound database.
    pub async fn transaction(&self, isolation: TransactionIsolation) -> Result<Transaction<'_>> {
        self.client.transaction(&self.database, isolation).await
    }

    /// Command in an explicit language (`"sql"`, `"cypher"`, …).
    pub async fn execute_language(
        &self,
        language: &str,
        command: &str,
    ) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_language(&self.database, language, command)
            .await
    }

    /// Cypher read query.
    pub async fn cypher_query(&self, query: &str) -> Result<QueryResult> {
        self.client.cypher_query(&self.database, query).await
    }

    /// Cypher write command.
    pub async fn cypher_execute(&self, command: &str) -> Result<ExecuteCommandResponse> {
        self.client.cypher_execute(&self.database, command).await
    }
}
