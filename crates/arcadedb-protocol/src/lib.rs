//! A lightweight gRPC client for [ArcadeDB](https://arcadedb.com).
//!
//! Generated tonic stubs for `com.arcadedb.grpc`, a typed record-decode path
//! (`RecordDecode` derive + runtime), a `serde_json` bridge for dynamic
//! access, and nothing else.
//!
//! ## What's here
//!
//! - [`client::ArcadeDbClient`] — wraps the data + admin gRPC services
//!   (connect with retry, auth headers, `execute`/`query`/`create_database`/
//!   `bulk_insert`, …). `Clone` (shares one tonic `Channel`).
//! - [`error::ArcadeDbError`] — the typed error for every fallible client
//!   call: match on the failure category (transport, server-rejected command,
//!   decode, invalid input) and inspect the gRPC code/metadata.
//! - JSON bridge ([`grpc_record_to_json`]):
//!   `GrpcRecord` → `serde_json::Value` for callers that need dynamic
//!   access. The *typed* decode path is
//!   [`RecordDecode`] (see below) — there is no serde `Deserializer` anymore.
//! - [`record`] — the typed-decode runtime: [`record::FromGrpcValue`],
//!   [`record::RecordDecodeError`], and the property-access helpers used by the
//!   generated code.
//! - [`RecordDecode`] (re-exported derive) — `#[derive(RecordDecode)]` turns a
//!   model struct into [`TryFrom<&GrpcRecord>`]` + `FromGrpcValue` with
//!   per-field type coercion, honoring `#[serde(rename/default/flatten)]` for
//!   drop-in adoption. Custom types plug in via the `#[record(with = "...")]`
//!   attribute or by implementing `FromGrpcValue` directly.
//! - [`RecordEncode`] (re-exported derive) — the write-side mirror: derives
//!   [`record::ToGrpcRecord`] so bulk writers call `dto.to_grpc_record(class)`
//!   instead of hand-assembling `GrpcValue`/`GrpcRecord` protos.
//! - Value builders ([`Params`], [`IntoGrpcValue`]) for fixture/test code.
//!
//! ## What's NOT here
//!
//! Identity types and query/repository layers. ArcadeDB's native RID
//! (`#bucket:pos`) is the only identifier this crate deals with; apps choose
//! their own logical identity scheme (e.g. `table:key` or an external id)
//! and build their repo + query functions on top of [`ArcadeDbClient`].
//!
//! ```no_run
//! # async fn demo() -> arcadedb_protocol::Result<()> {
//! use arcadedb_protocol::ArcadeDbClient;
//! let client = ArcadeDbClient::connect("127.0.0.1:50051", "root", "password", "mydb").await?;
//! let res = client.query("mydb", "SELECT FROM item LIMIT 10").await?;
//! println!("{} rows in {} ms", res.len(), res.execution_time_ms);
//! # Ok(()) }
//! ```

// ---------------------------------------------------------------------------
// Generated proto types (package `com.arcadedb.grpc`)
// ---------------------------------------------------------------------------
pub mod proto {
    pub mod com {
        pub mod arcadedb {
            pub mod grpc {
                tonic::include_proto!("com.arcadedb.grpc");
            }
        }
    }
}

mod client;
mod decode;
mod encode;
mod filter;
pub mod error;
pub mod options;
pub mod record;

// Domain facades: capability groups off the root client. The canonical
// spelling is the module path; the `ArcadeDbClient::admin`/`scan`/`batch`
// accessors are discovery sugar over the same implementations.
pub mod admin;
pub mod batch;
pub use batch::{Batch, ScriptBuffer};
#[cfg(feature = "cache")]
pub mod cache;
pub mod scan;
pub mod stream;
pub mod testutil;

pub use arcadedb_record_macros::{RecordDecode, RecordEncode};
pub use client::{
    consume_stream, ArcadeDbClient, DatabaseClient, QueryResult, QueryResultBatch, StreamSummary,
    Transaction, TxCommands,
};
pub use decode::{grpc_record_to_json, grpc_value_to_json, json_to_grpc_value};
pub use encode::{rec, IntoGrpcValue, Params, SetClause};
pub use filter::FilterClause;
pub use options::ClientOptions;
pub use stream::StreamOptions;

/// Value constructors for generated code (`RecordEncode` derives) — NOT a
/// public API. The fns stay `pub` in `encode` (macro codegen must name
/// them cross-crate) but are re-exported ONLY here, doc-hidden: the public
/// binding surface is `params!` / `rec()` / `SetClause` / `IntoGrpcValue`,
/// where the Rust types disambiguate the wire kinds.
#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "decimal")]
    pub use crate::encode::decimal_v;
    pub use crate::encode::{
        bool_v, bytes_v, date_v, embedded_v, f32_v, f64_v, i32_v, i64_v, link_v, list_v, map_v,
        str_v, timestamp_v,
    };
}
pub use error::{ArcadeDbError, Result};
/// The protobuf well-known `Timestamp` — the wire shape for date/datetime
/// values (`Kind::TimestampValue`), re-exported for `RecordEncode` DTOs.
pub use prost_types::Timestamp;
pub use proto::com::arcadedb::grpc::GrpcRecord;
pub use proto::com::arcadedb::grpc::{GrpcEmbedded, GrpcLink, GrpcList, GrpcMap, GrpcValue};
// And the grpc_value::Kind enum, for assembling list/map parameter values.
pub use proto::com::arcadedb::grpc::grpc_value::Kind;
pub use record::{
    changed_param_map, from_map_via_value, props_diff, DecodeFromMap, FromGrpcValue, Link,
    RecordDecodeError, ToGrpcRecord,
};

/// Implement [`DecodeFromMap`](record::DecodeFromMap) for a manual
/// [`FromGrpcValue`](record::FromGrpcValue) type through the clone-based
/// [`from_map_via_value`](record::from_map_via_value) path — for manual types
/// used as `#[serde(flatten)]` targets. Prefer deriving `RecordDecode` on
/// flatten targets (zero-copy).
#[macro_export]
macro_rules! impl_decode_from_map_via_value {
    ($ty:ty) => {
        impl<'de> $crate::record::DecodeFromMap<'de> for $ty {
            fn from_map(
                props: &'de ::std::collections::HashMap<::std::string::String, $crate::GrpcValue>,
                taken: &[&str],
                _syn_rid: ::core::option::Option<&::std::string::String>,
                _syn_type: ::core::option::Option<&::std::string::String>,
            ) -> ::std::result::Result<Self, $crate::record::RecordDecodeError> {
                $crate::record::from_map_via_value(props, taken)
            }
        }
    };
}

// Re-export proto service handles + types that downstream query code commonly
// needs when building typed wrappers around the client.
pub use proto::com::arcadedb::grpc::{
    arcade_db_admin_service_client::ArcadeDbAdminServiceClient,
    arcade_db_service_client::ArcadeDbServiceClient, ExecuteCommandResponse, InsertSummary,
};
