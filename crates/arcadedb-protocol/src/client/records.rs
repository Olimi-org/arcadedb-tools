//! Record writes — single-record CRUD by rid plus the bulk insert/upsert
//! RPCs for many-row loads.

use super::*;

#[cfg(feature = "rayon")]
use super::results::par_min_records;

/// DTO → wire-record conversion for the bulk-upsert path: each row is an
/// independent encode (string clones dominate), so under the `rayon` feature
/// large batches convert on the global pool — threshold-gated so small
/// upserts keep the sequential path.
///
/// `Send` is required unconditionally for signature simplicity: DTOs are
/// plain data in practice; a genuinely `!Send` DTO can still call
/// `to_grpc_record` + [`ArcadeDbClient::bulk_upsert`] directly.
fn records_from_dtos<T: crate::record::ToGrpcRecord + Send>(
    rows: Vec<T>,
    target_class: &str,
) -> Vec<GrpcRecord> {
    #[cfg(feature = "rayon")]
    if rows.len() >= par_min_records() {
        use rayon::prelude::*;
        return rows
            .into_par_iter()
            .map(|row| row.to_grpc_record(target_class))
            .collect();
    }
    rows.into_iter()
        .map(|row| row.to_grpc_record(target_class))
        .collect()
}

impl ArcadeDbClient {
    /// Create a record of `class_name` from `(property, value)` pairs — the
    /// typed single-record counterpart of INSERT-INTO SQL. Returns the
    /// server-assigned rid.
    pub async fn create_record(
        &self,
        database: &str,
        class_name: &str,
        props: Vec<(&str, crate::GrpcValue)>,
    ) -> Result<CreateRecordResponse> {
        let req = CreateRecordRequest {
            database: database.into(),
            credentials: Some(self.auth.creds.clone()),
            r#type: class_name.into(),
            record: Some(crate::rec(class_name, props)),
            transaction: None,
        };
        Ok(self
            .data
            .clone()
            .create_record(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("create record", class_name, e))?
            .into_inner())
    }

    /// Partially update a record by rid: only the given properties change,
    /// everything else is left untouched (`UpdateRecord`'s `partial` payload).
    /// The typed alternative to escaped-literal SET SQL; pair with
    /// [`changed_param_map`](crate::record::changed_param_map) for diff-based
    /// writes from DTOs.
    ///
    /// Returns whether a record was actually updated (unknown rid → `false`).
    pub async fn update_record_partial(
        &self,
        database: &str,
        rid: &crate::Link,
        props: impl Into<crate::encode::Params>,
    ) -> Result<bool> {
        let req = UpdateRecordRequest {
            database: database.into(),
            credentials: Some(self.auth.creds.clone()),
            rid: rid.to_rid_string(),
            payload: Some(Payload::Partial(PropertiesUpdate {
                properties: props.into().0,
            })),
            transaction: None,
        };
        let resp: UpdateRecordResponse = self
            .data
            .clone()
            .update_record(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("update record", &rid.to_string(), e))?
            .into_inner();
        if !resp.success {
            // UpdateRecordResponse carries no server message — the rid is the
            // only context available.
            return Err(ArcadeDbError::CommandFailed {
                operation: "update record".into(),
                message: rid.to_string(),
                affected: None,
            });
        }
        Ok(resp.updated)
    }

    /// Delete a record by rid. Returns whether a record was actually removed
    /// (already-absent rid → `false`).
    pub async fn delete_record(&self, database: &str, rid: &crate::Link) -> Result<bool> {
        let req = DeleteRecordRequest {
            database: database.into(),
            rid: rid.to_rid_string(),
            credentials: Some(self.auth.creds.clone()),
            transaction: None,
        };
        let resp = self
            .data
            .clone()
            .delete_record(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("delete record", &rid.to_string(), e))?
            .into_inner();
        if !resp.success {
            return Err(ArcadeDbError::CommandFailed {
                operation: format!("delete record {rid}"),
                message: resp.message,
                affected: None,
            });
        }
        Ok(resp.deleted)
    }

    /// Bulk-insert rows into `target_class` via the gRPC `BulkInsert` RPC — a
    /// single round trip for many records. `key_columns` + ignore-conflict make
    /// it idempotent (safe to re-run on a non-empty table). Used by the test
    /// fixture generators to seed data efficiently.
    ///
    /// Returns the server's [`InsertSummary`] (inserted/updated/ignored counts).
    pub async fn bulk_insert(
        &self,
        database: &str,
        target_class: &str,
        key_columns: &[&str],
        rows: Vec<GrpcRecord>,
    ) -> Result<InsertSummary> {
        let creds = self.auth.creds.clone();
        let req = BulkInsertRequest {
            database: database.into(),
            credentials: Some(creds.clone()),
            options: Some(InsertOptions {
                target_class: target_class.into(),
                key_columns: key_columns.iter().map(|s| (*s).to_string()).collect(),
                // Idempotent re-runs.
                conflict_mode: ConflictMode::ConflictIgnore as i32,
                // The server's bulkInsert handler reads database + credentials
                // from options, not the top-level request fields.
                database: database.into(),
                credentials: Some(creds),
                ..Default::default()
            }),
            rows,
            transaction: None,
        };

        let resp = self
            .data
            .clone()
            .bulk_insert(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("bulk insert", target_class, e))?
            .into_inner();

        tracing::info!(
            class = %target_class,
            inserted = resp.inserted,
            ignored = resp.ignored,
            failed = resp.failed,
            time_ms = resp.execution_time_ms,
            "bulk insert"
        );
        Ok(resp)
    }

    /// Bulk upsert rows into `target_class` via the gRPC `BulkInsert` RPC with
    /// `CONFLICT_UPDATE` (mode 1): key columns are matched against existing
    /// records and the non-key columns are overwritten. One round trip for many
    /// records — the batch analogue of `UPDATE ... UPSERT`, which the SQL path
    /// only accepts when the WHERE columns carry an index.
    ///
    /// `key_columns` must be covered by an index for conflict detection to be
    /// reliable (a UNIQUE index on the match key is the sanctioned shape).
    /// `update_columns_on_conflict` limits which columns are overwritten on a
    /// match; empty = merge all non-key columns present on the incoming record.
    ///
    /// Returns the server's [`InsertSummary`] (inserted/updated/ignored counts).
    pub async fn bulk_upsert(
        &self,
        database: &str,
        target_class: &str,
        key_columns: &[&str],
        update_columns_on_conflict: &[&str],
        rows: Vec<GrpcRecord>,
    ) -> Result<InsertSummary> {
        let creds = self.auth.creds.clone();
        let req = BulkInsertRequest {
            database: database.into(),
            credentials: Some(creds.clone()),
            options: Some(InsertOptions {
                target_class: target_class.into(),
                key_columns: key_columns.iter().map(|s| (*s).to_string()).collect(),
                // CONFLICT_UPDATE = 1 (upsert/update on match).
                conflict_mode: ConflictMode::ConflictUpdate as i32,
                update_columns_on_conflict: update_columns_on_conflict
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                database: database.into(),
                credentials: Some(creds),
                ..Default::default()
            }),
            rows,
            transaction: None,
        };

        let resp = self
            .data
            .clone()
            .bulk_insert(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("bulk upsert", target_class, e))?
            .into_inner();

        tracing::info!(
            class = %target_class,
            inserted = resp.inserted,
            updated = resp.updated,
            ignored = resp.ignored,
            failed = resp.failed,
            time_ms = resp.execution_time_ms,
            "bulk upsert"
        );
        Ok(resp)
    }

    /// The typed [`bulk_upsert`](Self::bulk_upsert): rows are
    /// `RecordEncode` DTOs, the conflict key comes from the DTO's
    /// `#[record(key)]` fields (`ToGrpcRecord::KEY_COLUMNS`) — the call
    /// site cannot disagree with the DTO about the identity — and every
    /// present column is merged on conflict (absent `Option`s keep their
    /// stored values).
    ///
    /// Errors up front when the DTO declares no key columns.
    pub async fn bulk_upsert_dtos<T>(
        &self,
        database: &str,
        target_class: &str,
        rows: Vec<T>,
    ) -> Result<InsertSummary>
    where
        T: crate::record::ToGrpcRecord + Send,
    {
        let keys = T::KEY_COLUMNS;
        if keys.is_empty() {
            return Err(ArcadeDbError::InvalidInput {
                message: format!(
                    "bulk_upsert_dtos: `{}` declares no #[record(key)] columns — \
                     the server cannot detect conflicts without a key",
                    std::any::type_name::<T>(),
                ),
            });
        }
        let records = records_from_dtos(rows, target_class);
        self.bulk_upsert(database, target_class, keys, &[], records)
            .await
    }
}
