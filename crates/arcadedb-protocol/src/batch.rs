//! Multi-statement units — the [`ArcadeDbClient::batch`] facade.
//!
//! Wraps the `sqlscript` batch language and the create-or-update edge
//! keyed by its `(out, in)` pair.
//!
//! Type names are validated identifiers; rid strings come from parsed
//! [`Link`](crate::Link)s (format-validated), values are bound parameters.

use crate::client::{validate_identifier, ArcadeDbClient};
use crate::error::{ArcadeDbError, Result};
use crate::proto::com::arcadedb::grpc::ExecuteCommandResponse;

/// Multi-statement units. Obtain via [`ArcadeDbClient::batch`].
#[derive(Clone)]
pub struct Batch {
    client: ArcadeDbClient,
}

impl Batch {
    pub(crate) fn new(client: ArcadeDbClient) -> Self {
        Self { client }
    }

    /// Run a `sqlscript` script — multiple statements in one roundtrip.
    /// The script executes statement-by-statement server-side; an error
    /// aborts the remainder.
    pub async fn script(&self, database: &str, script: &str) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_language(database, "sqlscript", script)
            .await
    }

    /// Create-or-update an edge identified by its `(out, in)` endpoints —
    /// the typed shape `UPDATE … UPSERT` cannot express (unavailable
    /// on edges): find the existing edge, update it, or
    /// create it with the given properties.
    ///
    /// Two roundtrips on the update path (find + update), one on create.
    /// `props` keys must be valid identifiers; rid strings are
    /// format-validated [`Link`](crate::Link)s.
    pub async fn upsert_edge(
        &self,
        database: &str,
        edge_type: &str,
        from: &crate::Link,
        to: &crate::Link,
        props: impl Into<crate::encode::Params>,
    ) -> Result<()> {
        let e = validate_identifier(edge_type, "edge type")?;
        let props = props.into();
        let from_rid = from.to_rid_string();
        let to_rid = to.to_rid_string();

        // Endpoint matching uses the `@out`/`@in` record intrinsics — the
        // same family as `@rid`/`@type`: always available, no bare form.
        // Bare `out`/`in` are the graph FUNCTIONS (`out('EDGE')`/
        // `in('EDGE')`), not endpoint properties, and never match as
        // equality targets.
        let existing = self
            .client
            .query_with_values(
                database,
                &format!("SELECT FROM {e} WHERE @out = :__out AND @in = :__in LIMIT 1"),
                crate::params! {
                    __out: crate::encode::link_v(from_rid.clone()),
                    __in: crate::encode::link_v(to_rid.clone()),
                },
            )
            .await?;

        match existing.records.first() {
            Some(edge) => {
                let rid = edge.rid.clone();
                if props.0.is_empty() {
                    return Ok(());
                }
                let mut set = String::new();
                let mut params = std::collections::HashMap::new();
                for (i, (k, v)) in props.0.into_iter().enumerate() {
                    let k = validate_identifier(&k, "property name")?;
                    if i > 0 {
                        set.push_str(", ");
                    }
                    set.push_str(&format!("{k} = :{k}"));
                    params.insert(k, v);
                }
                self.client
                    .execute_with_values(database, &format!("UPDATE {rid} SET {set}"), params)
                    .await?;
                Ok(())
            }
            None => {
                let mut sql = format!("CREATE EDGE {e} FROM {from_rid} TO {to_rid}");
                if !props.0.is_empty() {
                    let mut set = String::new();
                    let mut params = std::collections::HashMap::new();
                    for (i, (k, v)) in props.0.into_iter().enumerate() {
                        let k = validate_identifier(&k, "property name")?;
                        if i > 0 {
                            set.push_str(", ");
                        }
                        set.push_str(&format!("{k} = :{k}"));
                        params.insert(k, v);
                    }
                    sql.push_str(&format!(" SET {set}"));
                    self.client
                        .execute_with_values(database, &sql, params)
                        .await?;
                } else {
                    self.client.execute(database, &sql).await?;
                }
                Ok(())
            }
        }
    }

    /// Apply DDL statements one-by-one. With `idempotent = true`,
    /// `AlreadyExists` is treated as success (re-runnable
    /// migrations). Returns how many statements were newly applied.
    pub async fn apply_ddl(
        &self,
        database: &str,
        statements: &[&str],
        idempotent: bool,
    ) -> Result<usize> {
        let mut applied = 0;
        for statement in statements {
            match self.client.execute(database, statement).await {
                Ok(_) => applied += 1,
                Err(e) => {
                    if idempotent && matches!(e, ArcadeDbError::AlreadyExists { .. }) {
                        tracing::debug!(statement = %statement, "DDL already applied");
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Ok(applied)
    }
}

/// A buffering [`Batch::script`] writer: statements accumulate and flush
/// as one roundtrip per `FLUSH_AT` statements, tail via [`Self::finish`].
pub struct ScriptBuffer {
    client: ArcadeDbClient,
    database: String,
    script: String,
    stmts: usize,
}

/// Statements per roundtrip.
const FLUSH_AT: usize = 500;

impl ScriptBuffer {
    pub(crate) fn new(client: ArcadeDbClient, database: impl Into<String>) -> Self {
        Self {
            client,
            database: database.into(),
            script: String::new(),
            stmts: 0,
        }
    }

    /// Append one statement; flushes automatically at the batch size.
    pub async fn push(&mut self, stmt: &str) -> Result<()> {
        use std::fmt::Write as _;
        let _ = writeln!(self.script, "{stmt}");
        self.stmts += 1;
        if self.stmts >= FLUSH_AT {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.script.is_empty() {
            return Ok(());
        }
        let script = std::mem::take(&mut self.script);
        self.stmts = 0;
        self.client
            .execute_language(&self.database, "sqlscript", &script)
            .await?;
        Ok(())
    }

    /// Flush the tail. Call once after the last push.
    pub async fn finish(&mut self) -> Result<()> {
        self.flush().await
    }
}

impl Batch {
    /// A buffering script writer bound to this client (see [`ScriptBuffer`]).
    pub fn script_buffer(&self, database: impl Into<String>) -> ScriptBuffer {
        ScriptBuffer::new(self.client.clone(), database)
    }
}
