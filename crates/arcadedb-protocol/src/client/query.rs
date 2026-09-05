//! Reads — plain and bound queries, rid/key lookups, one-row conveniences,
//! and the returning-write helpers.

use super::*;

impl ArcadeDbClient {
    /// Execute a SQL query and return the result records with server-side timing.
    pub async fn query(&self, database: &str, query: &str) -> Result<QueryResult> {
        let req = tonic::Request::new(ExecuteQueryRequest {
            database: database.into(),
            query: query.into(),
            credentials: Some(self.auth.creds.clone()),
            language: "sql".into(),
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_query(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("query", query, e))?
            .into_inner();

        let mut records = Vec::with_capacity(resp.results.iter().map(|qr| qr.records.len()).sum());
        records.extend(resp.results.into_iter().flat_map(|qr| qr.records));

        tracing::info!(
            count = records.len(),
            time_ms = resp.execution_time_ms,
            query = %query,
            "query results"
        );

        Ok(QueryResult {
            records,
            execution_time_ms: resp.execution_time_ms,
        })
    }

    /// Execute a SQL query with bound parameters (the `:name` placeholders in
    /// the query are filled from `parameters`).
    ///
    /// Prefer this over [`query`](Self::query) whenever the query references
    /// caller-supplied values — bound params avoid SQL-injection concerns and
    /// let ArcadeDB cache the parsed query plan. The vector-search operators
    /// (`vector.sparseNeighbors`, `vector.fuse`, `SEARCH_INDEX`) in particular
    /// expect array-valued params for the query token/weight vectors.
    ///
    /// Build parameter values with [`Params`](crate::Params) (e.g. the
    /// [`params!`](crate::params!) macro), or assemble list/map values
    /// directly from the proto types re-exported at the crate root.
    pub async fn query_with_values(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<QueryResult> {
        let parameters = parameters.into();
        let req = tonic::Request::new(ExecuteQueryRequest {
            database: database.into(),
            query: query.into(),
            parameters: parameters.0,
            credentials: Some(self.auth.creds.clone()),
            language: "sql".into(),
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_query(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("query", query, e))?
            .into_inner();

        let mut records = Vec::with_capacity(resp.results.iter().map(|qr| qr.records.len()).sum());
        records.extend(resp.results.into_iter().flat_map(|qr| qr.records));

        tracing::info!(
            count = records.len(),
            time_ms = resp.execution_time_ms,
            query = %query,
            "query results"
        );

        Ok(QueryResult {
            records,
            execution_time_ms: resp.execution_time_ms,
        })
    }

    /// Fetch one record by rid — the O(1) path: direct bucket/position
    /// dereference, no SQL parsing, no index traversal. The fastest read
    /// ArcadeDB offers; prefer it whenever the caller already holds the
    /// record's [`Link`](crate::Link) (a prior query's `@rid`, an edge's `@in`/`@out` —
    /// all record intrinsics).
    ///
    /// Durability note: rids are stable for a database's *lifetime* but may
    /// shift across rebuilds/backup-restores — treat them as within-lifetime
    /// handles, never as persisted identifiers. Domain keys (e.g. a UNIQUE
    /// index column) remain the durable identity.
    pub async fn lookup_by_rid(
        &self,
        database: &str,
        rid: &crate::Link,
    ) -> Result<Option<GrpcRecord>> {
        let req = LookupByRidRequest {
            database: database.into(),
            credentials: Some(self.auth.creds.clone()),
            rid: rid.to_rid_string(),
            transaction: None,
        };
        let resp = self
            .data
            .clone()
            .lookup_by_rid(self.add_auth_headers(tonic::Request::new(req)))
            .await
            .map_err(|e| map_rpc_error("lookup by rid", &rid.to_string(), e))?
            .into_inner();
        Ok(resp.found.then_some(resp.record.unwrap_or_default()))
    }

    /// [`lookup_by_rid`](Self::lookup_by_rid) decoded into an owned DTO
    /// (`T: TryFrom<&GrpcRecord>`, i.e. an owned `RecordDecode` struct).
    pub async fn lookup<T>(&self, database: &str, rid: &crate::Link) -> Result<Option<T>>
    where
        T: for<'a> ::core::convert::TryFrom<
            &'a GrpcRecord,
            Error = crate::record::RecordDecodeError,
        >,
    {
        match self.lookup_by_rid(database, rid).await? {
            Some(rec) => {
                let decoded = T::try_from(&rec).map_err(|e| ArcadeDbError::Decode {
                    what: format!("lookup record {rid}"),
                    source: e,
                })?;
                Ok(Some(decoded))
            }
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // One-row conveniences + returning-write helpers. Each collapses a
    // repeated call-site template.
    // ------------------------------------------------------------------

    /// Typed fetch of at most one row: `Ok(None)` on an empty result, a
    /// decode error only when a row exists but does not match `T`.
    ///
    /// **Pair point-lookups with `LIMIT 1`** — the call decodes the first
    /// row but the RPC still materializes the whole result set.
    pub async fn fetch_optional<T>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        let res = self.query_with_values(database, query, parameters).await?;
        match res.records.first() {
            None => Ok(None),
            Some(rec) => T::try_from(rec)
                .map(Some)
                .map_err(|e| ArcadeDbError::Decode {
                    what: format!("fetch row of `{query}`"),
                    source: e,
                }),
        }
    }

    /// [`fetch_optional`](Self::fetch_optional) with a one-row contract: an
    /// empty result is [`ArcadeDbError::NotFound`], not `Ok(None)`.
    ///
    /// Same wire-cost note: keep the query itself to one row (`LIMIT 1`)
    /// so the RPC materializes only what is decoded.
    pub async fn fetch_one<T>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        self.fetch_optional(database, query, parameters)
            .await?
            .ok_or_else(|| ArcadeDbError::NotFound {
                operation: "fetch_one".into(),
                detail: query.into(),
            })
    }

    /// Decode the single column of a single-row query (`SELECT count(*)`,
    /// `SELECT max(x)`, …). `Ok(None)` when the query returned no row.
    ///
    /// The column is identified by position: the row must carry exactly one
    /// property (a defensive guard — the proto row is a map, so "first
    /// column" is only well-defined for one-column rows), decoded via
    /// [`FromGrpcValue`](crate::record::FromGrpcValue). Aggregate queries
    /// return a single row anyway (`SELECT count(*)`); anything wide
    /// belongs in [`fetch_optional`](Self::fetch_optional).
    pub async fn query_scalar<T>(
        &self,
        database: &str,
        query: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'de> crate::record::FromGrpcValue<'de>,
    {
        let res = self.query_with_values(database, query, parameters).await?;
        match res.records.into_iter().next() {
            None => Ok(None),
            Some(rec) => {
                let value = single_column_value(&rec, query)?;
                T::from_grpc_value(value)
                    .map(Some)
                    .map_err(|e| ArcadeDbError::Decode {
                        what: format!("query scalar of `{query}`"),
                        source: e,
                    })
            }
        }
    }

    /// Run a write command that returns the stored row and decode it. The
    /// command must be a form whose response carries records:
    ///
    /// - `INSERT INTO t SET …` — returns the stored record as-is (the RPC
    ///   runs with `return_rows = true`; no `RETURN` clause exists for
    ///   INSERT — trailing `… RETURN AFTER` is a syntax error).
    /// - `UPDATE t SET … RETURN AFTER WHERE …` — `RETURN BEFORE|AFTER` sits
    ///   **between the SET/UPSERT part and WHERE**, not at statement end.
    ///   The `UPSERT` variant additionally requires the WHERE column to
    ///   carry an index.
    ///
    /// `Ok(None)` when the statement matched nothing (`RETURN AFTER` on a
    /// zero-row UPDATE); use [`create_and_return`](Self::create_and_return)
    /// or [`upsert_returning`](Self::upsert_returning) for the strict
    /// one-row contract.
    ///
    /// Replaces the execute→extract→decode template at every returning-write
    /// call site.
    pub async fn write_returning_optional<T>(
        &self,
        database: &str,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        let resp = self
            .execute_with_values(database, command, parameters)
            .await?;
        match resp.records.first() {
            None => Ok(None),
            Some(rec) => T::try_from(rec)
                .map(Some)
                .map_err(|e| ArcadeDbError::Decode {
                    what: format!("decode returning row of `{command}`"),
                    source: e,
                }),
        }
    }

    /// [`write_returning_optional`](Self::write_returning_optional) with a
    /// one-row contract (empty result → [`ArcadeDbError::NotFound`]).
    pub async fn upsert_returning<T>(
        &self,
        database: &str,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        self.write_returning_optional(database, command, parameters)
            .await?
            .ok_or_else(|| ArcadeDbError::NotFound {
                operation: "upsert_returning".into(),
                detail: command.into(),
            })
    }

    /// Typed create that returns the stored record (schema defaults
    /// included) instead of only the rid — a plain `INSERT INTO … SET …`
    /// whose response row is decoded (INSERT returns the stored record via
    /// `return_rows`; see [`write_returning_optional`](Self::write_returning_optional)
    /// for the grammar notes). Retires the
    /// create-then-`lookup_by_rid` read-back pair.
    pub async fn create_and_return<T>(
        &self,
        database: &str,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<T>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        self.write_returning_optional(database, command, parameters)
            .await?
            .ok_or_else(|| ArcadeDbError::NotFound {
                operation: "create_and_return".into(),
                detail: command.into(),
            })
    }

    /// Typed lookup by a (single or composite) key on `type_name`: builds
    /// `SELECT FROM {type} WHERE k1 = :k0 AND k2 = :k1 LIMIT 1` with bound
    /// parameters — no identity concept imported, just the Java API's
    /// `lookupByKey` semantics over plain equality.
    ///
    /// Property/type names are validated identifiers (ASCII alphanumerics
    /// and `_`); values are bound, never interpolated.
    pub async fn lookup_by_key<T>(
        &self,
        database: &str,
        type_name: &str,
        key: &[(&str, crate::GrpcValue)],
    ) -> Result<Option<T>>
    where
        T: for<'r> TryFrom<&'r GrpcRecord, Error = RecordDecodeError>,
    {
        if key.is_empty() {
            return Err(ArcadeDbError::InvalidInput {
                message: "lookup_by_key: key must name at least one property".into(),
            });
        }
        let t = validate_identifier(type_name, "type name")?;
        let mut sql = format!("SELECT FROM {t}");
        let mut params = std::collections::HashMap::new();
        for (i, (column, value)) in key.iter().enumerate() {
            let column = validate_identifier(column, "property name")?;
            sql.push_str(if i == 0 { " WHERE " } else { " AND " });
            sql.push_str(&format!("{column} = :key{i}"));
            params.insert(format!("key{i}"), value.clone());
        }
        sql.push_str(" LIMIT 1");
        self.fetch_optional(database, &sql, params).await
    }

    /// Count all records of a type (polymorphic — ArcadeDB `SELECT FROM
    /// type` includes subtypes), the Java API's `countType`. One
    /// [`query_scalar`](Self::query_scalar) roundtrip.
    pub async fn count_type(&self, database: &str, type_name: &str) -> Result<i64> {
        let t = validate_identifier(type_name, "type name")?;
        self.query_scalar::<i64>(
            database,
            &format!("SELECT count(*) AS count FROM {t}"),
            std::collections::HashMap::new(),
        )
        .await?
        .ok_or_else(|| ArcadeDbError::NotFound {
            operation: "count_type".into(),
            detail: format!("no count row returned for `{t}`"),
        })
    }
}

/// A one-column row's value, for [`ArcadeDbClient::query_scalar`] — the
/// proto row is a map, so "the scalar" is only well-defined for exactly one
/// property.
pub(crate) fn single_column_value<'r>(rec: &'r GrpcRecord, query: &str) -> Result<&'r GrpcValue> {
    if rec.properties.len() != 1 {
        return Err(ArcadeDbError::InvalidInput {
            message: format!(
                "query_scalar: `{query}` returned a {}-column row; exactly one column is required",
                rec.properties.len()
            ),
        });
    }
    Ok(rec.properties.values().next().expect("len checked == 1"))
}
