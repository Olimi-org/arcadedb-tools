# arcadedb-tools
Tooling for using ArcadeDB with the Rust programming language over gRPC.

Here's what you get:

- `arcadedb-protocol` — async gRPC client: SQL/Cypher queries, bound parameters, typed row decoding, transactions, streaming, batch writes.
- `arcadedb-record-macros` — `#[derive(RecordDecode)]` / `#[derive(RecordEncode)]` for model structs.
- `arcadedb-migrate` — declarative schema migrations plus a `migrate` binary.

## Requirements

A running ArcadeDB server with the gRPC plugin.

## Here's how you can start:

Add the dependency:

```toml
[dependencies]
arcadedb-protocol = "0.3"
serde = { version = "1", features = ["derive"] }
```

Connect and run a typed query:

```rust
use arcadedb_protocol::{ArcadeDbClient, ClientOptions, RecordDecode};
use serde::Deserialize;

#[derive(Debug, Deserialize, RecordDecode)]
struct Article {
    title: String,
}

// Credentials come from your environment (ARCADEDB_ADDR, ARCADEDB_USER, ...).
let client = ArcadeDbClient::connect_with(ClientOptions::from_env()?).await?;
let res = client.query("mydb", "SELECT FROM article LIMIT 10").await?;
for row in res.rows::<Article>() {
    println!("{}", row?.title);
}
```

Bound parameters:

```rust
use arcadedb_protocol::params;

let found: Option<Article> = client.fetch_optional(
    "mydb",
    "SELECT FROM article WHERE title = :title LIMIT 1",
    params! { title: "ArcadeDB features" },
).await?;
```

Writes and DDL go through `execute`:

```rust
client.execute("mydb", "CREATE DOCUMENT TYPE article IF NOT EXISTS").await?;
```

`ClientOptions` also carries TLS, timeouts, and retry policy when you need them.

## Going further

For large result sets, stream batches and stop early — the callback's `Break` cancels the query server-side:

```rust
use std::ops::ControlFlow;

let summary = client
    .stream_query("mydb", "SELECT FROM article", |batch| {
        // work with each batch as it arrives
        ControlFlow::Continue(())
    })
    .await?;
println!("{} rows in {} batches", summary.rows, summary.batches);
```

## Migrations

```rust
use arcadedb_migrate::schema::{DropStrategy, Migrator};

let migrator = Migrator::new(client, "mydb").await?;
let report = migrator.sync_dir("arcadedb/schema", DropStrategy::Never).await?;
println!("{report}");
```

Expected layout:

```text
arcadedb/schema/
  *.sql            declarative base schema
  overrides/       one-shot migrations, applied in filename order
    overrides.sum  integrity manifest (optional)
  seed/            idempotent seed statements, re-run every sync
  rollout/         review artifacts
```

## Optional features (`arcadedb-protocol`)

- `cache` — transient KV facade over the Redis plugin (`client.cache()`).
- `decimal` — exact `rust_decimal::Decimal` mapping for `DECIMAL` columns.
- `rayon` — parallel decode of large batches.
- `serde` — serde impls for wire-identity types.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
