//! Throwaway test databases for suites running against a live server.
//!
//! Two primitives, one per isolation model:
//!
//! - [`TestDatabase`] — one unique database per spawn, dropped when the
//!   guard goes away. The serial / single-database case.
//! - [`TestPool`] — a fixed pool of reusable databases with concurrency
//!   capped at the pool size, drop + recreate on every checkout, and a
//!   caller-supplied seed step. The parallel-suite case.
//!
//! Both guards own the lifecycle — including the crash path, where [`Drop`]
//! schedules the best-effort drop exactly like the
//! [`Transaction`](crate::Transaction) guard schedules rollback.
//!
//! ```no_run
//! # async fn demo() -> arcadedb_protocol::Result<()> {
//! use arcadedb_protocol::{ClientOptions, testutil::TestDatabase};
//!
//! let db = TestDatabase::spawn(ClientOptions::from_env()?, "mytest").await?;
//! // `db.name()` is e.g. `mytest_18f3a2b9c0_0` — unique per spawn.
//! let res = db.client().query(db.name(), "SELECT FROM schema:types").await?;
//! assert!(res.is_empty()); // fresh database
//! drop(db); // or db.remove().await? for the fallible, awaited drop
//! # Ok(())
//! # }
//! ```
//!
//! ```no_run
//! # async fn demo() -> arcadedb_protocol::Result<()> {
//! use arcadedb_protocol::{ClientOptions, testutil::TestPool};
//!
//! let pool = TestPool::builder(ClientOptions::from_env()?, "test_pool")
//!     .size(8)
//!     .seed(|client, db| async move {
//!         client.execute(&db, "CREATE DOCUMENT TYPE probe IF NOT EXISTS").await?;
//!         Ok(())
//!     })
//!     .build();
//!
//! let db = pool.checkout().await?; // recreated + seeded, exclusively ours
//! let res = db.client().query(db.name(), "SELECT FROM probe").await?;
//! assert!(res.is_empty());
//! drop(db); // slot released; the next checkout recreates it fresh
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{ArcadeDbError, Result};
use crate::{ArcadeDbClient, ClientOptions};

/// Monotonic disambiguator so same-nanosecond spawns (parallel tests within
/// one process) still differ.
static SPAWN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A uniquely-named, owned test database. Dropping it schedules a
/// best-effort drop of the database on the ambient runtime (when one is
/// live); [`TestDatabase::remove`] awaits the drop fallibly instead.
pub struct TestDatabase {
    // `Option` so `remove` can move both out without tripping the Drop
    // destructor (partial move out of a Drop type is rejected).
    client: Option<ArcadeDbClient>,
    name: Option<String>,
}

impl TestDatabase {
    /// Connect using `options`' endpoint/credentials/retry policy and create
    /// a database named `{sanitized prefix}_{unix nanos hex}_{seq}`. The
    /// `database` field of `options` is overridden by the generated name.
    pub async fn spawn(options: ClientOptions, prefix: &str) -> Result<Self> {
        let name = unique_name(prefix);
        let mut options = options;
        options.database = name.clone();
        let client = ArcadeDbClient::connect_with(options).await?;

        match client.admin().create_database(&name, "graph").await {
            Ok(()) => {}
            // Impossible to collide with the unique name; tolerate the race
            // anyway rather than fail a test run on a technicality.
            Err(crate::error::ArcadeDbError::AlreadyExists { .. }) => {}
            Err(e) => return Err(e),
        }

        Ok(Self {
            client: Some(client),
            name: Some(name),
        })
    }

    /// The generated database name.
    pub fn name(&self) -> &str {
        self.name.as_deref().expect("name present until removal")
    }

    /// The connected client — every data call is already authenticated and
    /// scoped to this database.
    pub fn client(&self) -> &ArcadeDbClient {
        self.client.as_ref().expect("client present until removal")
    }

    /// Drop the database now, awaiting the server's answer. Prefer this over
    /// relying on [`Drop`] when the test wants to assert cleanup or fail
    /// loudly on it.
    pub async fn remove(mut self) -> Result<()> {
        let client = self.client.take().expect("client present");
        let name = self.name.take().expect("name present");
        client.admin().drop_database(&name).await
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Detached-task fallback, mirroring the Transaction guard's
        // drop-rollback: a panicked test still cleans up. No runtime, no
        // cleanup attempt (never panic in drop).
        let Some(client) = self.client.take() else {
            return;
        };
        let Some(name) = self.name.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = client.admin().drop_database(&name).await;
            });
        }
    }
}

/// `{sanitized prefix}_{unix nanos hex}_{seq}` — database-name-safe
/// (ASCII alphanumerics + `_`) and unique across spawns.
fn unique_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SPAWN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}_{nanos:x}_{seq}", sanitize_prefix(prefix))
}

/// Pool-name-safe prefix: ASCII alphanumerics + `_` (48 chars max),
/// falling back to `"test"` for empty/hostile input.
fn sanitize_prefix(prefix: &str) -> String {
    let sanitized: String = prefix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(48)
        .collect();
    if sanitized.is_empty() {
        "test".to_string()
    } else {
        sanitized
    }
}

// ---------------------------------------------------------------------------
// Pooled databases — bounded parallel-test isolation
// ---------------------------------------------------------------------------

/// Boxed per-checkout seed step: fresh client + pool database name → make
/// it test-ready (schema apply, fixture load). Stored boxed so any
/// `|client, db| async move { … }` closure fits.
type SeedFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type SeedFn = Arc<dyn Fn(ArcadeDbClient, String) -> SeedFuture + Send + Sync>;

/// A fixed pool of reusable test databases for parallel suites.
///
/// Each checkout gets exclusive use of one slot (`{prefix}_{owner}_{slot}`),
/// recreated via drop+create and seeded. Concurrency is bounded by the pool
/// size; slots are released on drop. Stale slots from dead owners are
/// reclaimed on first checkout (Linux only).
pub struct TestPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    options: ClientOptions,
    prefix: String,
    /// Per-process owner — avoids collisions with per-process runners (e.g. nextest).
    owner: String,
    seed: Option<SeedFn>,
    semaphore: Arc<Semaphore>,
    slots: Mutex<Vec<usize>>,
    size: usize,
    /// Ensures stale sweep runs at most once.
    swept: std::sync::atomic::AtomicBool,
}

impl TestPool {
    /// Start building a pool: `options` supplies endpoint/credentials/retry
    /// policy (its `database` field is the bootstrap database); `prefix`
    /// names the pool databases (`{prefix}_{owner}_{slot}`).
    pub fn builder(options: ClientOptions, prefix: &str) -> TestPoolBuilder {
        TestPoolBuilder {
            options,
            prefix: sanitize_prefix(prefix),
            size: 8,
            seed: None,
        }
    }

    /// Pool capacity — and max concurrent checkouts.
    pub fn size(&self) -> usize {
        self.inner.size
    }

    /// Check out an isolated database: acquire a slot, drop + recreate its
    /// database, connect a fresh client, run the seed step. The returned
    /// guard releases the slot on drop.
    pub async fn checkout(&self) -> Result<PooledDatabase> {
        self.sweep_stale_once().await;
        let permit = self
            .inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ArcadeDbError::InvalidInput {
                message: "test pool semaphore closed".into(),
            })?;
        let slot = {
            let mut slots = self.inner.slots.lock().unwrap();
            match slots.pop() {
                Some(slot) => slot,
                None => {
                    // Unreachable by construction (one slot per permit) —
                    // a loud error, never a panic, if the invariant breaks.
                    return Err(ArcadeDbError::InvalidInput {
                        message: "test pool: permit held but no slot free".into(),
                    });
                }
            }
        };
        let name = format!("{}_{}_{slot}", self.inner.prefix, self.inner.owner);

        let bootstrap = match ArcadeDbClient::connect_with(self.inner.options.clone()).await {
            Ok(client) => client,
            Err(e) => {
                release_slot(&self.inner, slot);
                return Err(e);
            }
        };
        // Fresh slate per checkout (ignore drop errors — first run): never
        // wipe + re-seed, which corrupts unique indexes.
        let _ = bootstrap.admin().drop_database(&name).await;
        match bootstrap.admin().create_database(&name, "graph").await {
            Ok(()) => {}
            // Tolerate the race rather than fail a test run on a technicality.
            Err(ArcadeDbError::AlreadyExists { .. }) => {}
            Err(e) => {
                release_slot(&self.inner, slot);
                return Err(e);
            }
        }
        let mut options = self.inner.options.clone();
        options.database = name.clone();
        let client = match ArcadeDbClient::connect_with(options).await {
            Ok(client) => client,
            Err(e) => {
                release_slot(&self.inner, slot);
                return Err(e);
            }
        };
        if let Some(seed) = &self.inner.seed {
            // Seed failure leaves the half-seeded database behind — the next
            // checkout recreates it, so freshness never depends on cleanup.
            if let Err(e) = seed(client.clone(), name.clone()).await {
                release_slot(&self.inner, slot);
                return Err(e);
            }
        }
        Ok(PooledDatabase {
            client,
            name,
            slot,
            pool: self.inner.clone(),
            _permit: permit,
        })
    }
}

fn release_slot(inner: &PoolInner, slot: usize) {
    inner.slots.lock().unwrap().push(slot);
}

/// Per-process token `p{pid}_{nanos:x}_{seq}`.
fn owner_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SPAWN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("p{}_{nanos:x}_{seq}", std::process::id())
}

impl TestPool {
    /// Reclaims stale databases for this prefix from dead owners (best-effort, once).
    async fn sweep_stale_once(&self) {
        if self.inner.swept.swap(true, Ordering::AcqRel) {
            return;
        }
        let bootstrap = match ArcadeDbClient::connect_with(self.inner.options.clone()).await {
            Ok(client) => client,
            Err(_) => return,
        };
        let dbs = match bootstrap.admin().list_databases().await {
            Ok(dbs) => dbs,
            Err(_) => return,
        };
        for db in dbs {
            if dead_owner_pid(&db, &self.inner.prefix).is_some() {
                let _ = bootstrap.admin().drop_database(&db).await;
            }
        }
    }
}

/// Returns dead owner's pid if `db` is a stale slot for `prefix`.
fn dead_owner_pid(db: &str, prefix: &str) -> Option<u32> {
    let rest = db.strip_prefix(prefix)?.strip_prefix('_')?;
    let owner = rest.split('_').next()?;
    let pid = owner.strip_prefix('p')?.parse::<u32>().ok()?;
    if pid == std::process::id() {
        return None;
    }
    (!owner_process_alive(pid)).then_some(pid)
}

#[cfg(target_os = "linux")]
fn owner_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn owner_process_alive(_pid: u32) -> bool {
    // No liveness probe outside Linux — never reclaims.
    true
}

/// Builder for [`TestPool`].
pub struct TestPoolBuilder {
    options: ClientOptions,
    prefix: String,
    size: usize,
    seed: Option<SeedFn>,
}

impl TestPoolBuilder {
    /// Pool databases (and max concurrent checkouts). Default 8.
    ///
    /// Each checkout drops and recreates its database. The server only
    /// finalizes a drop in the background, so when checkouts cycle faster
    /// than that cleanup, creating databases starts failing with
    /// connection resets — adding container memory or CPU does not help.
    pub fn size(mut self, size: usize) -> Self {
        self.size = size;
        self
    }

    /// Run `seed` (fresh client, pool database name) after every checkout's
    /// recreate, before the guard is handed out.
    pub fn seed<F, Fut>(mut self, seed: F) -> Self
    where
        F: Fn(ArcadeDbClient, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.seed = Some(Arc::new(move |client, name| {
            Box::pin(seed(client, name)) as SeedFuture
        }));
        self
    }

    /// Build the pool. Panics on `size == 0` — a zero-permit semaphore
    /// would deadlock the first checkout, so fail here, loudly, instead.
    /// Databases are created lazily, one per slot on first checkout.
    pub fn build(self) -> TestPool {
        assert!(
            self.size >= 1,
            "test pool size must be at least 1 (a zero-permit semaphore deadlocks checkout)"
        );
        let size = self.size;
        TestPool {
            inner: Arc::new(PoolInner {
                options: self.options,
                prefix: self.prefix,
                owner: owner_token(),
                seed: self.seed,
                semaphore: Arc::new(Semaphore::new(size)),
                slots: Mutex::new((0..size).rev().collect()),
                size,
                swept: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }
}

/// A checked-out pool database: connected client + slot name, exclusively
/// ours until dropped. Dropping releases the slot (and the concurrency
/// permit) back to the pool; the database itself is recreated on the next
/// checkout, so cleanup timing never matters.
pub struct PooledDatabase {
    client: ArcadeDbClient,
    name: String,
    slot: usize,
    pool: Arc<PoolInner>,
    // Declared last so it drops last, after `drop()` pushes the slot back.
    _permit: OwnedSemaphorePermit,
}

impl PooledDatabase {
    /// The slot's database name (`{prefix}_{owner}_{slot}`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Client connected to this database.
    pub fn client(&self) -> &ArcadeDbClient {
        &self.client
    }
}

impl Drop for PooledDatabase {
    fn drop(&mut self) {
        release_slot(&self.pool, self.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_tokens_are_process_tagged_and_unique() {
        let a = owner_token();
        let b = owner_token();
        assert!(a.starts_with(&format!("p{}_", std::process::id())), "{a}");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn dead_owner_pid_parses_only_foreign_owned_slots() {
        let prefix = "mytest";
        // Our own slots: never reclaimable.
        let ours = format!("{prefix}_p{}_{:x}_0", std::process::id(), 0xabc);
        assert_eq!(dead_owner_pid(&ours, prefix), None);
        // A foreign pid with no live process (beyond typical pid_max).
        let dead = format!("{prefix}_p4294967290_{:x}_3", 0xabc);
        if !owner_process_alive(4294967290) {
            assert_eq!(dead_owner_pid(&dead, prefix), Some(4294967290));
        }
        // Legacy fixed-name slots and malformed shapes: untouched.
        assert_eq!(dead_owner_pid(&format!("{prefix}_3"), prefix), None);
        assert_eq!(dead_owner_pid(&format!("{prefix}_x1_{:x}_0", 0xabc), prefix), None);
        assert_eq!(dead_owner_pid("other_p1_1_0", prefix), None);
        // A live foreign process (pid 1): recognized as owned, alive → None.
        assert_eq!(
            dead_owner_pid(&format!("{prefix}_p1_{:x}_0", 0xabc), prefix),
            None
        );
    }

    #[test]
    fn generated_names_are_unique_sanitized_and_prefixed() {
        let a = unique_name("my-test!");
        let b = unique_name("my-test!");
        assert!(a.starts_with("mytest_"), "{a}");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        // Empty/hostile prefixes still produce a valid name.
        assert!(unique_name("///").starts_with("test_"));
        // Absurdly long prefixes are truncated to something name-sized.
        assert!(unique_name(&"x".repeat(500)).len() < 80);
    }

    // Empty options throughout: these tests never connect (databases are
    // created lazily on checkout), and `new()` validates nothing.
    #[test]
    fn pool_builder_defaults_to_eight_and_sanitizes_prefix() {
        let pool =
            TestPool::builder(ClientOptions::new("", "", "", ""), "my-pool!").build();
        assert_eq!(pool.size(), 8);
        assert_eq!(pool.inner.prefix, "mypool");
        let sized = TestPool::builder(ClientOptions::new("", "", "", ""), "p")
            .size(3)
            .build();
        assert_eq!(sized.size(), 3);
        assert_eq!(sized.inner.slots.lock().unwrap().len(), 3);
    }

    #[test]
    #[should_panic(expected = "at least 1")]
    fn pool_builder_rejects_zero_size() {
        TestPool::builder(ClientOptions::new("", "", "", ""), "p")
            .size(0)
            .build();
    }
}
