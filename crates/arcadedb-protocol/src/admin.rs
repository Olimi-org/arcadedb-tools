//! Admin operations — the [`ArcadeDbClient::admin`] facade.
//!
//! Server and database lifecycle calls (ping, databases, server info).

use crate::client::{map_rpc_error, ArcadeDbClient};
use crate::error::Result;
use crate::proto::com::arcadedb::grpc::{
    CreateDatabaseRequest, DropDatabaseRequest, ExistsDatabaseRequest, GetDatabaseInfoRequest,
    GetDatabaseInfoResponse, GetServerInfoRequest, GetServerInfoResponse, ListDatabasesRequest,
    PingRequest, PingResponse,
};

/// Server-lifecycle and inspection operations. Obtain via
/// [`ArcadeDbClient::admin`].
#[derive(Clone)]
pub struct Admin {
    client: ArcadeDbClient,
}

impl Admin {
    pub(crate) fn new(client: ArcadeDbClient) -> Self {
        Self { client }
    }

    /// Ping the server to verify admin connectivity.
    pub async fn ping(&self) -> Result<PingResponse> {
        let resp = self
            .client
            .admin_stub()
            .ping(PingRequest {
                credentials: Some(self.client.creds()),
            })
            .await
            .map_err(|e| map_rpc_error("ping", "", e))?
            .into_inner();
        Ok(resp)
    }

    /// Create a new database. `db_type` is `"graph"` or `"document"`
    /// (logical; see the proto spec).
    pub async fn create_database(&self, name: &str, db_type: &str) -> Result<()> {
        self.client
            .admin_stub()
            .create_database(CreateDatabaseRequest {
                credentials: Some(self.client.creds()),
                name: name.into(),
                r#type: db_type.into(),
            })
            .await
            .map_err(|e| map_rpc_error("create database", name, e))?;
        tracing::info!(db = %name, "database created");
        Ok(())
    }

    /// Drop a database by name.
    pub async fn drop_database(&self, name: &str) -> Result<()> {
        self.client
            .admin_stub()
            .drop_database(DropDatabaseRequest {
                credentials: Some(self.client.creds()),
                name: name.into(),
            })
            .await
            .map_err(|e| map_rpc_error("drop database", name, e))?;
        tracing::info!(db = %name, "database dropped");
        Ok(())
    }

    /// Whether a database with this name exists on the server.
    pub async fn exists_database(&self, name: &str) -> Result<bool> {
        let resp = self
            .client
            .admin_stub()
            .exists_database(ExistsDatabaseRequest {
                credentials: Some(self.client.creds()),
                name: name.into(),
            })
            .await
            .map_err(|e| map_rpc_error("exists database", name, e))?
            .into_inner();
        Ok(resp.exists)
    }

    /// All database names the credentials can see.
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .admin_stub()
            .list_databases(ListDatabasesRequest {
                credentials: Some(self.client.creds()),
            })
            .await
            .map_err(|e| map_rpc_error("list databases", "", e))?
            .into_inner();
        Ok(resp.databases)
    }

    /// Server version / edition / uptime / ports.
    pub async fn get_server_info(&self) -> Result<GetServerInfoResponse> {
        Ok(self
            .client
            .admin_stub()
            .get_server_info(GetServerInfoRequest {
                credentials: Some(self.client.creds()),
            })
            .await
            .map_err(|e| map_rpc_error("get server info", "", e))?
            .into_inner())
    }

    /// Schema/types/statistics summary for one database.
    pub async fn get_database_info(&self, name: &str) -> Result<GetDatabaseInfoResponse> {
        Ok(self
            .client
            .admin_stub()
            .get_database_info(GetDatabaseInfoRequest {
                credentials: Some(self.client.creds()),
                name: name.into(),
            })
            .await
            .map_err(|e| map_rpc_error("get database info", name, e))?
            .into_inner())
    }
}
