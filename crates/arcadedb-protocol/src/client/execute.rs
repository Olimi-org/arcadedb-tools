//! Write and query-language execution — the `execute_command` RPC surface:
//! SQL commands, bound parameters, and the Cypher variants.

use super::*;

impl ArcadeDbClient {
    /// Execute a command in the given query language ("sql" or "cypher").
    ///
    /// Returns the server response including affected records and write stats.
    pub async fn execute_language(
        &self,
        database: &str,
        language: &str,
        command: &str,
    ) -> Result<ExecuteCommandResponse> {
        let req = tonic::Request::new(ExecuteCommandRequest {
            database: database.into(),
            command: command.into(),
            credentials: Some(self.auth.creds.clone()),
            language: language.into(),
            return_rows: true,
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_command(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("execute command", command, e))?
            .into_inner();

        if !resp.success {
            return Err(command_failed(
                "execute command",
                resp.message,
                resp.affected_records,
            ));
        }

        Ok(resp)
    }

    /// Execute a SQL command (DDL, INSERT, UPDATE, DELETE).
    ///
    /// Returns the server response including affected records and write stats.
    pub async fn execute(&self, database: &str, command: &str) -> Result<ExecuteCommandResponse> {
        self.execute_with_values(database, command, std::collections::HashMap::new())
            .await
    }

    /// [`execute`](Self::execute) with bound `:name` parameters — the
    /// command-RPC analogue of [`query_with_values`](Self::query_with_values).
    /// Prefer this whenever the command references caller-supplied values
    /// (user names, urls, ids): no literal escaping, no injection surface.
    /// Build parameter values with [`Params`](crate::Params) (e.g. the
    /// [`params!`](crate::params!) macro) or a `RecordEncode` DTO's
    /// [`ToGrpcRecord::to_param_map`](crate::ToGrpcRecord::to_param_map).
    pub async fn execute_with_values(
        &self,
        database: &str,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<ExecuteCommandResponse> {
        let parameters = parameters.into();
        let req = tonic::Request::new(ExecuteCommandRequest {
            database: database.into(),
            command: command.into(),
            parameters: parameters.0,
            credentials: Some(self.auth.creds.clone()),
            language: "sql".into(),
            return_rows: true,
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_command(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("execute command", command, e))?
            .into_inner();

        if !resp.success {
            return Err(command_failed(
                "execute command",
                resp.message,
                resp.affected_records,
            ));
        }

        Ok(resp)
    }

    /// Execute a Cypher read query (MATCH + RETURN, etc.).
    ///
    /// Uses the streaming query RPC with language="cypher".
    pub async fn cypher_query(&self, database: &str, query: &str) -> Result<QueryResult> {
        let req = tonic::Request::new(ExecuteQueryRequest {
            database: database.into(),
            query: query.into(),
            credentials: Some(self.auth.creds.clone()),
            language: "cypher".into(),
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_query(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("cypher query", query, e))?
            .into_inner();

        let mut records = Vec::with_capacity(resp.results.iter().map(|qr| qr.records.len()).sum());
        records.extend(resp.results.into_iter().flat_map(|qr| qr.records));

        Ok(QueryResult {
            records,
            execution_time_ms: resp.execution_time_ms,
        })
    }

    /// Execute a Cypher write command (MATCH + SET, etc.).
    ///
    /// Uses the execute_command RPC with language set to "opencypher"
    /// (the variant that ArcadeDB accepts for write operations).
    pub async fn cypher_execute(
        &self,
        database: &str,
        command: &str,
    ) -> Result<ExecuteCommandResponse> {
        self.execute_language(database, "opencypher", command).await
    }
}
