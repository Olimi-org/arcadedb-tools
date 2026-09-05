//! Transient KV cache over ArcadeDB's Redis plugin — the
//! [`ArcadeDbClient::cache`] facade (`cache` feature).
//!
//! `SET`/`GET`/`GETDEL` run through the command RPC with
//! `language: "redis"` — no RESP client, no extra port, the same gRPC
//! channel that serves SQL.
//!
//! - `GET`/`SET`/`GETDEL`/`EXISTS`/`INCR` work; results ride the first
//!   result row's `value` property (scalar, or a per-command array for
//!   newline-separated batches).
//! - `SET … EX` is **accepted but silently ignored** — expiry is enforced
//!   client-side through an in-band envelope and readers delete expired
//!   entries on sight (upstream ArcadeData/arcadedb#6884).
//! - `DEL`, `TTL`, `MSET`, `MGET` are **unsupported** — deletion goes
//!   through `GETDEL`, multi-key reads through the newline batch.
//! - The Redis-QL tokenizer splits on whitespace: an unencoded
//!   `SET k hello world` stores only `hello`. Values are therefore
//!   hex-encoded before `SET` and decoded after `GET`.
//! - Keys are transient (server-memory); a server restart empties the cache
//!   and every reader falls back to the live query path.
//!
//! Server prerequisite (one line in the host compose):
//! `-Darcadedb.server.plugins=…,Redis:com.arcadedb.redis.RedisProtocolPlugin`

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::ArcadeDbClient;
use crate::error::{ArcadeDbError, Result};
use crate::proto::com::arcadedb::grpc::ExecuteCommandResponse;
use crate::{GrpcValue, Kind};

/// Transient KV cache. Obtain via [`ArcadeDbClient::cache`].
#[derive(Clone)]
pub struct Cache {
    client: ArcadeDbClient,
}

impl Cache {
    pub(crate) fn new(client: ArcadeDbClient) -> Self {
        Self { client }
    }

    /// Run one raw Redis command over the gRPC command RPC; returns the
    /// first scalar of the first result row (`GET` → the value, `SET` →
    /// `"OK"`, `GETDEL` on a missing key → `None`).
    ///
    /// The escape hatch under the typed methods: commands this facade does
    /// not wrap run verbatim. The command is sent as-is, so caller
    /// data inside it must be encoded by the caller (the typed [`set`]
    /// path exists precisely because raw interpolation truncates on
    /// whitespace).
    ///
    /// [`set`]: Self::set
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let version = client.cache().raw("testdb", "GET schema:version").await?;
    /// # let _ = version; Ok(()) }
    /// ```
    pub async fn raw(&self, database: &str, command: &str) -> Result<Option<String>> {
        Ok(first_scalar(&self.command(database, command).await?))
    }

    /// Newline-separated batch — one roundtrip for N commands, per-command
    /// scalars in order (`"SET a 1\nGET a"` → `[Some("OK"), Some("1")]`]).
    ///
    /// The batch is all-or-nothing: a rejected command fails the
    /// whole call. A missed `GET` surfaces as `None` in its position.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let results = client.cache()
    ///     .raw_batch("testdb", "SET a 1\nGET a\nGET missing")
    ///     .await?; // [Some("OK"), Some("1"), None]
    /// # let _ = results; Ok(()) }
    /// ```
    pub async fn raw_batch(&self, database: &str, commands: &str) -> Result<Vec<Option<String>>> {
        let resp = self.command(database, commands).await?;
        // The batch response is ONE record whose `value` property carries
        // the array of per-command results.
        let Some(list) = resp.records.first().and_then(|rec| {
            rec.properties.values().next().and_then(|v| match &v.kind {
                Some(Kind::ListValue(list)) => Some(list),
                _ => None,
            })
        }) else {
            return Err(ArcadeDbError::InvalidInput {
                message: format!(
                    "raw_batch: unexpected response shape (expected a per-command \
                     result array, got {:?})",
                    resp.records
                        .first()
                        .and_then(|rec| rec.properties.values().next())
                ),
            });
        };
        Ok(list.values.iter().map(value_to_string).collect())
    }

    /// Store `value` under `key`, hex-encoded (whitespace/quote-safe), with
    /// a client-enforced `ttl` — `None` = no expiry, [`Duration::ZERO`] =
    /// evict on first read. One `SET` roundtrip.
    ///
    /// This is the plain-string primitive; [`set_json`](Self::set_json) is
    /// the typed payload form. Both write the same envelope format, so a
    /// value written by one reads back through the other.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// use std::time::Duration;
    /// client.cache().set("testdb", "page:etag", &"v7 with spaces \"quoted\"", Some(Duration::from_secs(60))).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set(
        &self,
        database: &str,
        key: &str,
        value: &str,
        ttl: Option<Duration>,
    ) -> Result<()> {
        self.store(database, key, &value, ttl).await
    }

    /// Fetch the string stored under `key`. `Ok(None)` on a miss, an
    /// expired entry, or a corrupt entry — the latter two are consumed
    /// via `GETDEL` and logged, never surfaced as errors.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// if let Some(etag) = client.cache().get("testdb", "page:etag").await? {
    ///     // skip the expensive aggregate when the etag still matches
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn get(&self, database: &str, key: &str) -> Result<Option<String>> {
        self.load(database, key).await
    }

    /// [`set`](Self::set) for any JSON-serializable payload.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// # use std::time::Duration;
    /// client.cache().set_json("testdb", "stats:full", &vec![1i64, 2, 3], Some(Duration::from_secs(3600))).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set_json<T: Serialize>(
        &self,
        database: &str,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<()> {
        self.store(database, key, value, ttl).await
    }

    /// [`get`](Self::get) decoding the stored payload as `T`. Miss /
    /// expired / corrupt all read as `Ok(None)`.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let dataset: Option<Vec<i64>> = client.cache().get_json("testdb", "stats:full").await?;
    /// # let _ = dataset; Ok(()) }
    /// ```
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        database: &str,
        key: &str,
    ) -> Result<Option<T>> {
        self.load(database, key).await
    }

    /// Remove and return the string under `key` (Redis `GETDEL` — the
    /// plugin's only delete path). Consumed regardless
    /// of age: the atomic read-and-clear primitive.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let shown = client.cache().getdel("testdb", "queue:current_page").await?;
    /// # let _ = shown; Ok(()) }
    /// ```
    pub async fn getdel(&self, database: &str, key: &str) -> Result<Option<String>> {
        self.consume(database, key).await
    }

    /// [`getdel`](Self::getdel) decoding the consumed payload as `T`.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let page: Option<Vec<String>> = client.cache().getdel_json("testdb", "queue:page").await?;
    /// # let _ = page; Ok(()) }
    /// ```
    pub async fn getdel_json<T: DeserializeOwned>(
        &self,
        database: &str,
        key: &str,
    ) -> Result<Option<T>> {
        self.consume(database, key).await
    }

    /// Whether `key` exists. `false` on a miss.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// if client.cache().exists("testdb", "page:etag").await? { /* warm */ }
    /// # Ok(()) }
    /// ```
    pub async fn exists(&self, database: &str, key: &str) -> Result<bool> {
        validate_key(key)?;
        let v = self.raw(database, &format!("EXISTS {key}")).await?;
        Ok(v.as_deref() == Some("1"))
    }

    /// Increment the integer under `key` by one and return the new value.
    /// A missing key starts from zero.
    ///
    /// ```no_run
    /// # async fn demo(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_protocol::Result<()> {
    /// let hits = client.cache().incr("testdb", "counter:page").await?;
    /// # let _ = hits; Ok(()) }
    /// ```
    pub async fn incr(&self, database: &str, key: &str) -> Result<i64> {
        validate_key(key)?;
        let v = self
            .raw(database, &format!("INCR {key}"))
            .await?
            .ok_or_else(|| ArcadeDbError::CommandFailed {
                operation: "cache incr".into(),
                message: format!("INCR `{key}` returned no value"),
                affected: None,
            })?;
        v.parse().map_err(|_| ArcadeDbError::InvalidInput {
            message: format!("cache incr: `{key}` holds a non-integer value ({v:?})"),
        })
    }

    // -- internals ------------------------------------------------------------

    /// One command RPC roundtrip.
    async fn command(&self, database: &str, command: &str) -> Result<ExecuteCommandResponse> {
        self.client
            .execute_language(database, "redis", command)
            .await
    }

    /// Encode + envelope + `SET` — the write path shared by [`set`](Self::set)
    /// and [`set_json`](Self::set_json).
    async fn store<T: Serialize>(
        &self,
        database: &str,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<()> {
        validate_key(key)?;
        let envelope = Envelope {
            stored_at_epoch: unix_now(),
            ttl_secs: ttl.map_or(u64::MAX, |d| d.as_secs()),
            payload: value,
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|e| ArcadeDbError::InvalidInput {
            message: format!("cache set `{key}`: payload serialization failed: {e}"),
        })?;
        let encoded = encode_hex(&bytes);
        self.command(database, &set_command(key, &encoded, ttl))
            .await?;
        Ok(())
    }

    /// `GET` + decode + expiry — the read path shared
    /// by [`get`](Self::get) and [`get_json`](Self::get_json). Anything that
    /// does not decode cleanly self-heals into a miss.
    async fn load<T: DeserializeOwned>(&self, database: &str, key: &str) -> Result<Option<T>> {
        validate_key(key)?;
        let Some(encoded) = self.raw(database, &format!("GET {key}")).await? else {
            return Ok(None);
        };
        let bytes = match decode_hex(&encoded) {
            Ok(bytes) => bytes,
            Err(e) => return Ok(self.heal(database, key, format!("hex decode: {e}")).await),
        };
        match serde_json::from_slice::<Envelope<T>>(&bytes) {
            Ok(envelope) if !envelope.expired() => Ok(Some(envelope.payload)),
            // Expired: server-side EX is a no-op, so consume on sight.
            Ok(_) => Ok(self.heal(database, key, "expired".into()).await),
            // Corrupt or pre-envelope legacy entry: miss + cleanup.
            Err(e) => Ok(self
                .heal(database, key, format!("envelope decode: {e}"))
                .await),
        }
    }

    /// `GETDEL` + decode — the path shared by [`getdel`](Self::getdel) and
    /// [`getdel_json`](Self::getdel_json).
    async fn consume<T: DeserializeOwned>(&self, database: &str, key: &str) -> Result<Option<T>> {
        validate_key(key)?;
        let Some(encoded) = self.raw(database, &format!("GETDEL {key}")).await? else {
            return Ok(None);
        };
        let decoded = decode_hex(&encoded)
            .map_err(|e| format!("hex decode: {e}"))
            .and_then(|bytes| {
                serde_json::from_slice::<Envelope<T>>(&bytes)
                    .map_err(|e| format!("envelope decode: {e}"))
            });
        match decoded {
            Ok(envelope) => Ok(Some(envelope.payload)),
            Err(why) => {
                tracing::warn!(
                    key,
                    why,
                    "consumed cache entry undecodable — treating as miss"
                );
                Ok(None)
            }
        }
    }

    /// Corrupt/expired entry: consume it (`GETDEL`) and surface a miss.
    /// Cleanup failures are logged, never fatal.
    async fn heal<T>(&self, database: &str, key: &str, why: String) -> Option<T> {
        tracing::warn!(key, why, "cache entry self-heal: consuming via GETDEL");
        if let Err(e) = self.command(database, &format!("GETDEL {key}")).await {
            tracing::warn!(key, %e, "cache entry self-heal cleanup failed");
        }
        None
    }
}

/// The SET command for a (key, encoded value, ttl) — the single place the
/// TTL policy lives. `EX` is accepted-but-ignored by the server, so ttl
/// rides the [envelope](Envelope) instead. When upstream honors `EX`:
/// append it here, drop the envelope from `store`, and let the self-heal
/// path retire old-format keys on first read.
fn set_command(key: &str, encoded: &str, _ttl: Option<Duration>) -> String {
    format!("SET {key} {encoded}")
}

/// In-band TTL envelope — client-side expiry. `ttl_secs` is `u64::MAX`
/// for no expiry; `0` means evict on first read.
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    stored_at_epoch: u64,
    ttl_secs: u64,
    payload: T,
}

impl<T> Envelope<T> {
    fn expired(&self) -> bool {
        self.stored_at_epoch.saturating_add(self.ttl_secs) <= unix_now()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keys may contain only `[A-Za-z0-9_:.]` and `-`: no whitespace (the
/// command tokenizer splits on it), no newline (the batch separator).
fn validate_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'));
    if valid {
        Ok(())
    } else {
        Err(ArcadeDbError::InvalidInput {
            message: format!(
                "cache key `{key}`: expected non-empty [A-Za-z0-9_:.] and `-` only \
                 (the Redis command tokenizer splits on whitespace; newline is the batch separator)"
            ),
        })
    }
}

/// One wire value coerced to an owned string: string results pass
/// through, integer results (`EXISTS` 0/1, `INCR` n) stringify.
fn value_to_string(v: &GrpcValue) -> Option<String> {
    match &v.kind {
        Some(Kind::StringValue(s)) => Some(s.clone()),
        Some(Kind::Int64Value(n)) => Some(n.to_string()),
        Some(Kind::Int32Value(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// The first scalar of the first result row.
fn first_scalar(resp: &ExecuteCommandResponse) -> Option<String> {
    resp.records
        .first()
        .and_then(|rec| rec.properties.values().find_map(value_to_string))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(s: &str) -> std::result::Result<Vec<u8>, String> {
    fn nibble(c: u8) -> std::result::Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(format!("invalid hex byte {c:#04x}")),
        }
    }
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(format!("odd hex length {}", bytes.len()));
    }
    bytes
        .chunks(2)
        .map(|pair| Ok(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_charset_rejects_tokenizer_breakers() {
        for good in ["feed:page:123", "k", "A-b.c_9", "probe_cache:abc"] {
            validate_key(good).unwrap_or_else(|e| panic!("`{good}` must be valid: {e}"));
        }
        for bad in [
            "",
            "has space",
            "line\nbreak",
            "tab\tchar",
            "quote\"d",
            "semi;colon",
        ] {
            assert!(
                validate_key(bad).is_err(),
                "`{bad}` must be rejected before the wire"
            );
        }
    }

    #[test]
    fn hex_round_trip_and_garbage_rejection() {
        // The exact payload class that forced hex: spaces AND embedded quotes.
        let payload = b"{\"title\":\"Space Game, \"quoted\"\":1}";
        assert_eq!(decode_hex(&encode_hex(payload)).unwrap(), payload);
        assert!(decode_hex("zz").is_err());
        assert!(decode_hex("abc").is_err());
    }

    #[test]
    fn envelope_expiry_logic() {
        let now = unix_now();
        let fresh = Envelope {
            stored_at_epoch: now - 10,
            ttl_secs: 60,
            payload: (),
        };
        let stale = Envelope {
            stored_at_epoch: now - 61,
            ttl_secs: 60,
            payload: (),
        };
        let zero_ttl = Envelope {
            stored_at_epoch: now,
            ttl_secs: 0,
            payload: (),
        };
        let no_expiry = Envelope {
            stored_at_epoch: now - 10_000,
            ttl_secs: u64::MAX,
            payload: (),
        };
        assert!(!fresh.expired());
        assert!(stale.expired());
        assert!(zero_ttl.expired(), "0 = evict on read");
        assert!(!no_expiry.expired(), "u64::MAX = never (saturating_add)");
    }

    #[test]
    fn envelope_json_round_trips_payload_types() {
        for bytes in [
            serde_json::to_vec(&Envelope {
                stored_at_epoch: 1,
                ttl_secs: 2,
                payload: "s",
            })
            .unwrap(),
            serde_json::to_vec(&Envelope {
                stored_at_epoch: 1,
                ttl_secs: 2,
                payload: vec![1i64, 2],
            })
            .unwrap(),
        ] {
            let re: Envelope<serde_json::Value> =
                serde_json::from_slice(&bytes).expect("envelope shape is self-describing");
            assert_eq!(re.stored_at_epoch, 1);
        }
    }

    #[test]
    fn set_command_omits_ex_today() {
        // The #6884 flip point: when upstream honors EX, this assertion is
        // the reminder that set_command (and the envelope) change together.
        assert_eq!(
            set_command("k", "deadbeef", Some(Duration::from_secs(60))),
            "SET k deadbeef"
        );
    }

    #[test]
    fn batch_shape_decodes_per_command_scalars() {
        use crate::encode::{list_v, str_v};

        // Batch shape: ONE record, `value` = per-command array.
        let resp = ExecuteCommandResponse {
            records: vec![crate::rec(
                "redis",
                vec![("value", list_v([str_v("OK"), str_v("3331")]))],
            )],
            ..Default::default()
        };
        // The raw_batch decode core (extracted so tests can drive it
        // without a server): the value list, element by element.
        let decoded: Vec<Option<String>> = resp
            .records
            .first()
            .and_then(|rec| rec.properties.values().next())
            .and_then(|v| match &v.kind {
                Some(Kind::ListValue(list)) => {
                    Some(list.values.iter().map(value_to_string).collect())
                }
                _ => None,
            })
            .expect("batch responses carry the per-command array");
        assert_eq!(decoded, vec![Some("OK".into()), Some("3331".into())]);
    }

    #[test]
    fn first_scalar_fishes_the_wire_shapes() {
        use crate::encode::{i64_v, str_v};
        let resp = |v| ExecuteCommandResponse {
            records: vec![crate::rec("redis", vec![("value", v)])],
            ..Default::default()
        };
        assert_eq!(first_scalar(&resp(str_v("OK"))), Some("OK".to_string()));
        // Integer results (EXISTS 0/1, INCR n) stringify — one shape for
        // every command result.
        assert_eq!(first_scalar(&resp(i64_v(1))), Some("1".to_string()));
        // No scalar, no first row: both miss.
        let empty = ExecuteCommandResponse::default();
        assert_eq!(first_scalar(&empty), None);
    }
}
