//! Explicit transactions — raw begin/commit/rollback, the [`Transaction`]
//! drop-guard, the [`TxCommands`] attempt handle, and the retrying
//! [`ArcadeDbClient::run_transaction`] runner.

use super::*;

impl ArcadeDbClient {
    /// Begin an explicit server-side transaction.
    ///
    /// Returns a transaction id; pass it to [`execute_in_transaction`](Self::execute_in_transaction)
    /// and [`query_in_transaction`](Self::query_in_transaction), then finish with
    /// [`commit_transaction`](Self::commit_transaction) or [`rollback_transaction`](Self::rollback_transaction).
    ///
    /// NOTE: ArcadeDB keeps schema changes (DDL) outside transaction scope —
    /// they commit immediately in their own sub-transaction. Use the explicit
    /// transaction for *record* operations (the copy phase of an override
    /// rebuild) and verify the copy before committing; do not rely on it to
    /// make batch DDL atomic.
    pub async fn begin_transaction(
        &self,
        database: &str,
        isolation: TransactionIsolation,
    ) -> Result<String> {
        let req = tonic::Request::new(BeginTransactionRequest {
            database: database.into(),
            credentials: Some(self.auth.creds.clone()),
            isolation: isolation as i32,
        });

        let resp = self
            .data
            .clone()
            .begin_transaction(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("begin transaction", "", e))?
            .into_inner();

        tracing::info!(tx = %resp.transaction_id, "transaction begun");
        Ok(resp.transaction_id)
    }

    /// Execute a command inside an explicitly-begun transaction.
    ///
    /// The command runs with the transaction attached; it is only durable when
    /// the transaction is later committed. (Schema statements still commit
    /// immediately — see [`begin_transaction`](Self::begin_transaction).)
    pub async fn execute_in_transaction(
        &self,
        database: &str,
        language: &str,
        command: &str,
        transaction_id: &str,
    ) -> Result<ExecuteCommandResponse> {
        let req = tonic::Request::new(ExecuteCommandRequest {
            database: database.into(),
            command: command.into(),
            credentials: Some(self.auth.creds.clone()),
            language: language.into(),
            return_rows: true,
            transaction: Some(TransactionContext {
                transaction_id: transaction_id.into(),
                ..Default::default()
            }),
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_command(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("execute command (in tx)", command, e))?
            .into_inner();

        if !resp.success {
            return Err(command_failed(
                "execute command (in tx)",
                resp.message,
                resp.affected_records,
            ));
        }

        Ok(resp)
    }

    /// Run a read query *inside* an explicitly-begun transaction, so the
    /// caller can verify what the transaction has written before committing.
    pub async fn query_in_transaction(
        &self,
        database: &str,
        query: &str,
        transaction_id: &str,
    ) -> Result<QueryResult> {
        let req = tonic::Request::new(ExecuteQueryRequest {
            database: database.into(),
            query: query.into(),
            credentials: Some(self.auth.creds.clone()),
            language: "sql".into(),
            transaction: Some(TransactionContext {
                transaction_id: transaction_id.into(),
                ..Default::default()
            }),
            ..Default::default()
        });

        let resp = self
            .data
            .clone()
            .execute_query(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("query (in tx)", query, e))?
            .into_inner();

        let mut records = Vec::with_capacity(resp.results.iter().map(|qr| qr.records.len()).sum());
        records.extend(resp.results.into_iter().flat_map(|qr| qr.records));

        Ok(QueryResult {
            records,
            execution_time_ms: resp.execution_time_ms,
        })
    }

    /// Commit an explicitly-begun transaction, making its record operations durable.
    pub async fn commit_transaction(&self, database: &str, transaction_id: &str) -> Result<()> {
        let req = tonic::Request::new(CommitTransactionRequest {
            transaction: Some(TransactionContext {
                transaction_id: transaction_id.into(),
                database: database.into(),
                ..Default::default()
            }),
            credentials: Some(self.auth.creds.clone()),
        });

        let resp = self
            .data
            .clone()
            .commit_transaction(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("commit transaction", transaction_id, e))?
            .into_inner();

        tracing::info!(tx = %transaction_id, ok = resp.success, committed = resp.committed, "transaction committed");
        if !resp.success {
            return Err(ArcadeDbError::CommandFailed {
                operation: "commit transaction".into(),
                message: resp.message,
                affected: None,
            });
        }
        Ok(())
    }

    /// Roll back an explicitly-begun transaction, discarding its record operations.
    pub async fn rollback_transaction(&self, database: &str, transaction_id: &str) -> Result<()> {
        let req = tonic::Request::new(RollbackTransactionRequest {
            transaction: Some(TransactionContext {
                transaction_id: transaction_id.into(),
                database: database.into(),
                ..Default::default()
            }),
            credentials: Some(self.auth.creds.clone()),
        });

        let resp = self
            .data
            .clone()
            .rollback_transaction(self.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("rollback transaction", transaction_id, e))?
            .into_inner();

        tracing::info!(tx = %transaction_id, ok = resp.success, rolled_back = resp.rolled_back, "transaction rolled back");
        if !resp.success {
            return Err(ArcadeDbError::CommandFailed {
                operation: "rollback transaction".into(),
                message: resp.message,
                affected: None,
            });
        }
        Ok(())
    }

    /// Begin an explicit server-side transaction as a guard ([`Transaction`]).
    ///
    /// The guard owns the transaction lifecycle: run commands with
    /// [`Transaction::execute`] / [`Transaction::query`], then finish with
    /// [`Transaction::commit`] or [`Transaction::rollback`]. Dropping the
    /// guard without finishing fires a best-effort background rollback (the
    /// server's idle-reaper is the backstop), so forgotten transactions
    /// cannot leak.
    pub async fn transaction(
        &self,
        database: &str,
        isolation: TransactionIsolation,
    ) -> Result<Transaction<'_>> {
        let id = self.begin_transaction(database, isolation).await?;
        Ok(Transaction {
            client: self,
            database: database.into(),
            id,
            finished: false,
        })
    }

    /// Run `body` inside an explicit transaction, retrying the whole block
    /// up to `retries` extra times when the server reports an MVCC conflict
    /// (concurrent modification) — the Java API's
    /// `transaction(txBlock, retries)`. A fresh transaction is begun per
    /// attempt; every failed attempt is rolled back before retrying.
    ///
    /// Conflict detection is best-effort message matching (the gRPC surface
    /// exposes no structured conflict code yet): `concurrent`, `mvcc`, or
    /// `needretry` in the server message. Non-conflict errors and exhausted
    /// retries return the last error, transaction rolled back.
    ///
    /// DDL is outside transaction scope (see
    /// [`begin_transaction`](Self::begin_transaction)) — record operations
    /// only. Pairs with the [`Transaction`](crate::Transaction) drop-guard:
    /// the guard makes multi-statement units atomic, this makes them
    /// self-healing under concurrent writers.
    pub async fn run_transaction<T, F, Fut>(
        &self,
        database: &str,
        isolation: TransactionIsolation,
        retries: u32,
        body: F,
    ) -> Result<T>
    where
        F: Fn(TxCommands) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let attempts = retries.saturating_add(1);
        let mut last_err: Option<ArcadeDbError> = None;
        for attempt in 0..attempts {
            let tx_id = self.begin_transaction(database, isolation).await?;
            let commands = TxCommands::new(self, database, &tx_id);
            match body(commands).await {
                Ok(value) => match self.commit_transaction(database, &tx_id).await {
                    Ok(()) => return Ok(value),
                    Err(e) => {
                        let _ = self.rollback_transaction(database, &tx_id).await;
                        if is_retryable_conflict(&e) && attempt + 1 < attempts {
                            tracing::warn!(attempt, "transaction commit conflict — retrying");
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e);
                    }
                },
                Err(e) => {
                    let _ = self.rollback_transaction(database, &tx_id).await;
                    if is_retryable_conflict(&e) && attempt + 1 < attempts {
                        tracing::warn!(attempt, "transaction body conflict — retrying");
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ArcadeDbError::InvalidInput {
            message: "run_transaction: retries exhausted without an error".into(),
        }))
    }
}

/// Commands issued inside [`ArcadeDbClient::run_transaction`]'s current
/// attempt — the [`Transaction`] guard's operations without the lifecycle
/// (begin/commit/rollback are the runner's job).
///
/// Owns its data (client handle is a cheap `Arc` clone, plus two small
/// `String`s) so the [`run_transaction`](ArcadeDbClient::run_transaction)
/// body can be a plain `|tx| async move { … }` closure — a borrowed
/// `TxCommands<'_>` would make the closure's future tie to the argument's
/// lifetime, which stable Rust's closure generics cannot express (no HRTB
/// over futures). Owned state also means every retry attempt gets a fresh
/// set of commands, which is exactly the retry semantics.
pub struct TxCommands {
    client: ArcadeDbClient,
    database: String,
    tx_id: String,
}

impl TxCommands {
    pub(crate) fn new(client: &ArcadeDbClient, database: &str, tx_id: &str) -> Self {
        Self {
            client: client.clone(),
            database: database.to_string(),
            tx_id: tx_id.to_string(),
        }
    }

    /// Run a command in the current transaction attempt.
    pub async fn execute(&self, language: &str, command: &str) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_in_transaction(&self.database, language, command, &self.tx_id)
            .await
    }

    /// Run a parameterized SQL command in the current transaction attempt.
    pub async fn execute_with_values(
        &self,
        command: &str,
        parameters: impl Into<crate::encode::Params>,
    ) -> Result<ExecuteCommandResponse> {
        // `execute_in_transaction` has no parameter path — inline its request.
        let parameters = parameters.into();
        let req = tonic::Request::new(ExecuteCommandRequest {
            database: self.database.clone(),
            command: command.into(),
            parameters: parameters.0,
            credentials: Some(self.client.creds()),
            language: "sql".into(),
            return_rows: true,
            transaction: Some(TransactionContext {
                transaction_id: self.tx_id.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let resp = self
            .client
            .data
            .clone()
            .execute_command(self.client.add_auth_headers(req))
            .await
            .map_err(|e| map_rpc_error("execute command (in tx)", command, e))?
            .into_inner();
        if !resp.success {
            return Err(command_failed(
                "execute command (in tx)",
                resp.message,
                resp.affected_records,
            ));
        }
        Ok(resp)
    }

    /// Run a SQL read in the current transaction attempt.
    pub async fn query(&self, query: &str) -> Result<QueryResult> {
        self.client
            .query_in_transaction(&self.database, query, &self.tx_id)
            .await
    }
}

/// Best-effort MVCC-conflict detection from the server message (no
/// structured gRPC code yet — see
/// [`run_transaction`](ArcadeDbClient::run_transaction)).
pub(crate) fn is_retryable_conflict(err: &ArcadeDbError) -> bool {
    err.server_message()
        .map(|m| {
            let m = m.to_ascii_lowercase();
            m.contains("concurrent") || m.contains("mvcc") || m.contains("needretry")
        })
        .unwrap_or(false)
}

/// A drop-safe explicit transaction: the guard returned by
/// [`ArcadeDbClient::transaction`]. Commands run through the guard; finishing
/// is explicit (`commit`/`rollback` both consume `self`), and a guard dropped
/// unfinished rolls back in a background task — the server's idle-reaper is
/// the last line of defense.
///
/// NOTE (mirrors [`ArcadeDbClient::begin_transaction`]): ArcadeDB keeps DDL
/// outside transaction scope — use the guard for *record* operations only.
pub struct Transaction<'a> {
    client: &'a ArcadeDbClient,
    database: String,
    id: String,
    finished: bool,
}

impl Transaction<'_> {
    /// The server-assigned transaction id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Run a command inside the transaction.
    pub async fn execute(&self, language: &str, command: &str) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_in_transaction(&self.database, language, command, &self.id)
            .await
    }

    /// Run a SQL read inside the transaction.
    pub async fn query(&self, query: &str) -> Result<QueryResult> {
        self.client
            .query_in_transaction(&self.database, query, &self.id)
            .await
    }

    /// Commit, making every record operation durable. Consumes the guard.
    pub async fn commit(mut self) -> Result<()> {
        self.finished = true;
        self.client
            .commit_transaction(&self.database, &self.id)
            .await
    }

    /// Roll back, discarding every record operation. Consumes the guard.
    pub async fn rollback(mut self) -> Result<()> {
        self.finished = true;
        self.client
            .rollback_transaction(&self.database, &self.id)
            .await
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        tracing::warn!(tx = %self.id, db = %self.database, "transaction dropped unfinished — rolling back");
        // Best-effort async rollback: requires a live runtime (the caller is
        // async by construction); outside one, the server reaper reclaims it.
        let client = self.client.clone();
        let database = std::mem::take(&mut self.database);
        let id = std::mem::take(&mut self.id);
        let _ = tokio::runtime::Handle::try_current().map(|rt| {
            rt.spawn(async move {
                if let Err(e) = client.rollback_transaction(&database, &id).await {
                    tracing::error!(tx = %id, "drop rollback failed: {e}");
                }
            });
        });
    }
}
