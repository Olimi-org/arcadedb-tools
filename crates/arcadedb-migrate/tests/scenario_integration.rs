//! Integration scenario tests against a live ArcadeDB container.
//!
//! These verify the *runtime* behavior the unit tests can't: that generated DDL
//! actually applies, that overrides are one-shot, and that a second sync is a
//! no-op against a real DB.
//!
//! Gated behind the `integration` feature: `cargo test -p arcadedb-migrate
//! --features integration`. Requires a live ArcadeDB server
//! (`ARCADEDB_ADDR`, default `127.0.0.1:50051`, root/password credentials).

use std::sync::OnceLock;

use arcadedb_migrate::migrator::rollout::{parse_rollout, render_rollout};
use arcadedb_migrate::schema::{DropStrategy, Migrator};
use arcadedb_protocol::ArcadeDbClient;

use tokio::sync::Mutex;

/// Serialize integration tests so they don't collide on the same test DB.
static GUARD: OnceLock<Mutex<()>> = OnceLock::new();

const TEST_DB: &str = "arcadedb_migrate_scenarios";

/// A temp schema dir built per-test, so each scenario starts clean.
struct TempSchema {
    dir: tempfile::TempDir,
}

impl TempSchema {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).expect("write");
        }
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }
}

async fn fresh_client() -> ArcadeDbClient {
    let addr = std::env::var("ARCADEDB_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());
    let c = ArcadeDbClient::connect(&addr, "root", "password", TEST_DB)
        .await
        .expect("connect");
    let _ = c.admin().drop_database(TEST_DB).await;
    c.admin()
        .create_database(TEST_DB, "graph")
        .await
        .expect("create db");
    c
}

/// Count the rows in `type_name` (test helper for the copy-phase assertions).
async fn count_rows(client: &ArcadeDbClient, type_name: &str) -> i64 {
    let res = client
        .query(TEST_DB, &format!("SELECT count(*) AS n FROM {type_name}"))
        .await
        .expect("count query");
    let rec = res.records.first().expect("count record");
    use arcadedb_protocol::proto::com::arcadedb::grpc::grpc_value::Kind;
    match rec.properties.get("n").and_then(|v| v.kind.clone()) {
        Some(Kind::Int64Value(n)) => n,
        Some(Kind::Int32Value(n)) => n as i64,
        Some(Kind::DoubleValue(n)) => n as i64,
        other => panic!("unexpected count kind: {other:?}"),
    }
}

/// Settle a `hero` doc type with 3 rows; returns the base schema tempdir.
async fn settle_hero_with_rows(migrator: &Migrator, client: &ArcadeDbClient) -> TempSchema {
    let base = TempSchema::new(&[(
        "hero.sql",
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )]);
    migrator
        .sync_dir(&base.path(), DropStrategy::Never)
        .await
        .expect("base sync");
    client
        .execute_language(
            TEST_DB,
            "sqlscript",
            "BEGIN;\nINSERT INTO hero SET name = 'a';\nINSERT INTO hero SET name = 'b'; \
             \nINSERT INTO hero SET name = 'c';\nCOMMIT;",
        )
        .await
        .expect("seed rows");
    assert_eq!(
        count_rows(client, "hero").await,
        3,
        "seed must produce 3 rows"
    );
    base
}

/// Copy-phase guarantee: a failing copy statement rolls back the DML, the
/// original types keep every row, and the override is not recorded.
#[cfg(feature = "integration")]
#[tokio::test]
async fn copy_phase_failure_rolls_back_and_leaves_originals_intact() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let base = settle_hero_with_rows(&migrator, &client).await;
    let before = count_rows(&client, "hero").await;

    // The second CREATE PROPERTY (no IF NOT EXISTS) hits an existing property —
    // a deterministic engine error inside the copy transaction.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("overrides")).unwrap();
    std::fs::write(
        dir.path().join("overrides/001_bad_copy.sql"),
        "CREATE DOCUMENT TYPE hero_lng IF NOT EXISTS;
         CREATE PROPERTY hero_lng.name TYPE STRING;
         CREATE PROPERTY hero_lng.name TYPE STRING;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect_err("engine-rejected copy must fail the sync");
    let msg = format!("{err}");
    assert!(
        msg.contains("copy statement failed"),
        "expected copy-phase error, got: {msg}"
    );
    assert!(
        msg.contains("rolled back"),
        "expected rollback mention, got: {msg}"
    );
    assert!(
        msg.contains("001_bad_copy.sql"),
        "expected the override filename in the error, got: {msg}"
    );

    // Original rows untouched; the override unrecorded.
    assert_eq!(
        count_rows(&client, "hero").await,
        before,
        "original rows must be untouched"
    );
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied overrides");
    assert!(
        applied.is_empty(),
        "failed override must not be recorded, got: {applied:?}"
    );
    let _ = base;
}

/// Copy-phase guarantee: a silent row-dropping copy (filtered INSERT) is
/// caught by the in-transaction verification and rolled back — never committed
/// as a partial copy, and never followed by a destructive swap.
#[cfg(feature = "integration")]
#[tokio::test]
async fn copy_verification_mismatch_fails_before_any_swap() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let base = settle_hero_with_rows(&migrator, &client).await;
    let before = count_rows(&client, "hero").await;

    // The filter matches nothing: the INSERT succeeds but copies 0 rows. The
    // in-transaction check (t_lng >= pre-copy source) must refuse the commit.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("overrides")).unwrap();
    std::fs::write(
        dir.path().join("overrides/001_filtered.sql"),
        "CREATE DOCUMENT TYPE hero_lng IF NOT EXISTS;
         CREATE PROPERTY hero_lng.name IF NOT EXISTS STRING;
         INSERT INTO hero_lng FROM SELECT * FROM hero WHERE name = 'zzz';
         DROP TYPE hero IF EXISTS;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect_err("verification mismatch must fail the sync");
    let msg = format!("{err}");
    assert!(
        msg.contains("copy verification FAILED"),
        "expected verification failure, got: {msg}"
    );
    assert!(
        msg.contains("rolled back"),
        "expected rollback mention, got: {msg}"
    );
    assert!(
        msg.contains("expected at least") && msg.contains("got 0"),
        "expected the count expectation in the message, got: {msg}"
    );

    // Original type intact — proving the swap (`DROP TYPE hero`) never ran.
    assert_eq!(
        count_rows(&client, "hero").await,
        before,
        "original type must survive the verification failure"
    );
    // The DML was rolled back (the type persists — DDL escapes the tx — but
    // holds none of the filtered rows).
    assert_eq!(
        count_rows(&client, "hero_lng").await,
        0,
        "partial copy must not remain committed"
    );
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied overrides");
    assert!(
        applied.is_empty(),
        "failed override must not be recorded, got: {applied:?}"
    );
    let _ = base;
}

/// Swap-phase guarantee: a reference copy that verified + committed stays
/// durable when a later swap statement fails — and the error reports exactly
/// how many swap statements already ran, with the override unrecorded.
#[cfg(feature = "integration")]
#[tokio::test]
async fn swap_phase_failure_leaves_copy_durable_by_swaps_unrun() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let base = settle_hero_with_rows(&migrator, &client).await;
    let before = count_rows(&client, "hero").await;

    // Copy phase fully commits (full untethered copy), then the swap's
    // `ALTER PROPERTY ... TYPE` is rejected by the engine.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("overrides")).unwrap();
    std::fs::write(
        dir.path().join("overrides/001_rebuild.sql"),
        "CREATE DOCUMENT TYPE hero_lng IF NOT EXISTS;
         CREATE PROPERTY hero_lng.name IF NOT EXISTS STRING;
         INSERT INTO hero_lng FROM SELECT * FROM hero;
         ALTER PROPERTY hero_lng.name TYPE INTEGER;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect_err("engine-rejected swap must fail the sync");
    let msg = format!("{err}");
    assert!(
        msg.contains("swap statement #1 failed"),
        "expected swap-phase error naming the statement index, got: {msg}"
    );
    assert!(
        msg.contains("after 0 already executed"),
        "expected the swap progress count (0), got: {msg}"
    );
    assert!(
        msg.contains("001_rebuild.sql"),
        "expected the override filename, got: {msg}"
    );

    // The copy is durable (verified + committed before the swap) and the
    // original type is untouched.
    assert_eq!(
        count_rows(&client, "hero_lng").await,
        before,
        "verified copy must be durable after a swap failure"
    );
    assert_eq!(
        count_rows(&client, "hero").await,
        before,
        "original type must remain intact"
    );
    // Override unrecorded — a retry re-runs the whole rebuild override.
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied overrides");
    assert!(
        applied.is_empty(),
        "failed override must not be recorded, got: {applied:?}"
    );
    let _ = base;
}

/// Full end-to-end: a base schema applies cleanly, a second sync is a no-op,
/// then an additive change produces only the new DDL.
#[cfg(feature = "integration")]
#[tokio::test]
async fn sync_then_additive_change_applies_only_the_diff() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Round 1: a base type with one property.
    let base = TempSchema::new(&[(
        "base.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;",
    )]);
    let r1 = migrator
        .sync_dir(&base.path(), DropStrategy::Never)
        .await
        .expect("sync 1");
    assert!(
        !r1.applied_statements.is_empty(),
        "first sync should apply DDL"
    );

    // Round 2: identical schema → no-op.
    let r2 = migrator
        .sync_dir(&base.path(), DropStrategy::Never)
        .await
        .expect("sync 2");
    assert!(r2.no_op, "second sync should be no-op, got: {r2:?}");

    // Verify the type actually exists.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(actual.types.contains_key("item"));
    assert!(actual.types["item"].properties.contains_key("title"));

    // Round 3: add a property.
    let evolved = TempSchema::new(&[(
        "base.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;
         CREATE PROPERTY item.year IF NOT EXISTS INTEGER;",
    )]);
    let r3 = migrator
        .sync_dir(&evolved.path(), DropStrategy::Never)
        .await
        .expect("sync 3");
    assert!(
        r3.applied_statements
            .iter()
            .any(|s| s.contains("item.year")),
        "expected item.year in: {:?}",
        r3.applied_statements
    );
    // And nothing else from the base schema.
    assert_eq!(
        r3.applied_statements.len(),
        1,
        "additive change should apply exactly 1 statement, got {:?}",
        r3.applied_statements
    );

    // Verify it's now in the DB.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(actual.types["item"].properties.contains_key("year"));
}

/// An override migration runs once, then is never re-applied on subsequent syncs.
#[cfg(feature = "integration")]
#[tokio::test]
async fn override_migration_runs_once_then_is_skipped() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Round 1: settle the BASE schema alone. This records a snapshot, so the
    // override added next is a *live* migration against an initialized DB
    // (on a never-synced fresh DB the baseline rule would consume overrides
    // without executing — see fresh_database_records_historical_overrides).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.sql"),
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;",
    )
    .unwrap();
    migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("base sync");

    // Round 2: an override that creates a type the base doesn't declare.
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    std::fs::write(
        overrides_dir.join("0001_add_legacy_type.sql"),
        "CREATE DOCUMENT TYPE legacy IF NOT EXISTS;",
    )
    .unwrap();

    let r1 = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("sync 2");
    assert!(
        r1.applied_overrides
            .iter()
            .any(|o| o.contains("0001_add_legacy_type")),
        "override should run first time: {:?}",
        r1.applied_overrides
    );

    // Verify the override-created type exists.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(
        actual.types.contains_key("legacy"),
        "override-created type missing"
    );

    // Round 3: the override must NOT re-run.
    let r2 = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("sync 3");
    assert!(
        !r2.applied_overrides
            .iter()
            .any(|o| o.contains("0001_add_legacy_type")),
        "override should not re-run, got: {:?}",
        r2.applied_overrides
    );
    assert!(
        r2.no_op,
        "third sync (after override ran) should be schema no-op, got: {r2:?}"
    );
}

/// Baseline semantics on a brand-new database: with no snapshot and no user
/// types, overrides are historical — recorded as consumed WITHOUT executing
/// (a fresh DB is born at the current declarative schema; historical override
/// statements assume types only the schema phase creates). The override's
/// objects must NOT exist afterward.
#[cfg(feature = "integration")]
#[tokio::test]
async fn fresh_database_records_historical_overrides_without_executing() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // First-ever sync on an empty database, with an override already present.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("base.sql"),
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;",
    )
    .unwrap();
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    std::fs::write(
        overrides_dir.join("0001_add_legacy_type.sql"),
        "CREATE DOCUMENT TYPE legacy IF NOT EXISTS;",
    )
    .unwrap();

    let r1 = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("first sync");
    assert!(
        r1.applied_overrides
            .contains(&"0001_add_legacy_type.sql".to_string()),
        "historical override must be recorded as consumed: {:?}",
        r1.applied_overrides
    );

    // The declarative schema applied; the historical override did NOT execute.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(
        actual.types.contains_key("item"),
        "declarative type must exist"
    );
    assert!(
        !actual.types.contains_key("legacy"),
        "historical override must not execute on a fresh database"
    );

    // And it stays consumed: a second sync is a clean no-op.
    let r2 = migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("second sync");
    assert!(r2.no_op, "second sync must be a no-op, got: {r2:?}");
}

/// The snapshot enables drift recovery: a manual DB edit with an unchanged
/// `.sql` (no checksum change) is detected via `snapshot.state != actual` and
/// the default is *restored* from source.
#[cfg(feature = "integration")]
#[tokio::test]
async fn manual_default_edit_is_detected_and_restored() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    let dir = TempSchema::new(&[(
        "item.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING (DEFAULT \"Untitled\");",
    )]);

    // Round 1: apply, settle.
    let r1 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 1");
    assert!(r1
        .applied_statements
        .iter()
        .any(|s| s.contains("CREATE PROPERTY item.title")));
    let r2 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 2");
    assert!(r2.no_op, "second sync should be no-op, got: {r2:?}");

    // Round 3: someone flips the default by hand (engine resolves literals, so
    // introspection of both values is comparable).
    client
        .execute(TEST_DB, "ALTER PROPERTY item.title default \"Edited\"")
        .await
        .expect("manual alter");

    // Round 4: the sync must NOT be a no-op — the snapshot caught the drift —
    // and it restores the declared default.
    let r3 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 3");
    assert!(!r3.no_op, "manual edit must be detected, got: {r3:?}");
    assert!(
        r3.applied_statements
            .iter()
            .any(|s| s == "ALTER PROPERTY item.title default \"Untitled\""),
        "expected default restore in: {:?}",
        r3.applied_statements
    );

    // Round 5: restored state settles to a no-op.
    let r4 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 4");
    assert!(r4.no_op, "restored default must settle, got: {r4:?}");

    // And the DB really has the declared default back. The engine echoes
    // string defaults with their quotes (observed on 26.9.1) — what matters
    // is that the introspected text is the declared one again, not `"Edited"`.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert_eq!(
        actual.types["item"].properties["title"]
            .constraints
            .default
            .as_ref()
            .map(|d| d.text()),
        Some("\"Untitled\""),
        "default must be restored in the DB"
    );
}

/// A `.sql`-level expression edit (`date()` style) is re-applied even though
/// the resolved value in the DB is unchanged — the snapshot records the
/// *source text* we applied, so a source edit is distinguishable.
#[cfg(feature = "integration")]
#[tokio::test]
async fn source_default_edit_reapplies_even_with_unchanged_db_value() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    let v1 = TempSchema::new(&[(
        "item.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.created_at IF NOT EXISTS DATETIME (DEFAULT date());",
    )]);
    migrator
        .sync_dir(&v1.path(), DropStrategy::Never)
        .await
        .expect("sync 1");

    // The DB resolved `date()` to a concrete timestamp; the snapshot remembers
    // the source was `date()`. Changing the .sql to a fixed literal must apply,
    // because the source-text half of the snapshot differs.
    let v2 = TempSchema::new(&[(
        "item.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.created_at IF NOT EXISTS DATETIME (DEFAULT date('2020-01-01'));",
    )]);
    let report = migrator
        .sync_dir(&v2.path(), DropStrategy::Never)
        .await
        .expect("sync 2");
    assert!(
        report
            .applied_statements
            .iter()
            .any(|s| s.contains("item.created_at") && s.contains("default")),
        "source default edit must re-apply, got: {:?}",
        report.applied_statements
    );

    // The re-applied expression stuck. The engine echoes the applied text
    // verbatim (observed on 26.9.1 — no resolution to a timestamp), so the
    // introspected value is the edited source text itself.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert_eq!(
        actual.types["item"].properties["created_at"]
            .constraints
            .default
            .as_ref()
            .map(|d| d.text()),
        Some("date('2020-01-01')"),
        "DB default should now be the literal from the edited source"
    );
}

/// The snapshot-armed no-op also catches drift on *structured* constraints: a
/// manual `min` edit with unchanged .sql used to be invisible (checksum-only
/// short-circuit) — now it's detected and restored.
#[cfg(feature = "integration")]
#[tokio::test]
async fn manual_min_edit_is_detected_and_restored() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    let dir = TempSchema::new(&[(
        "item.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.score IF NOT EXISTS INTEGER (MIN 0);",
    )]);
    migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 1");
    let r2 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 2");
    assert!(r2.no_op, "settle");

    client
        .execute(TEST_DB, "ALTER PROPERTY item.score min 5")
        .await
        .expect("manual alter");

    let r3 = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync 3");
    assert!(
        r3.applied_statements
            .iter()
            .any(|s| s == "ALTER PROPERTY item.score min 0"),
        "manual min edit must be restored, got: {:?}",
        r3.applied_statements
    );
}

/// Generated DDL for an LSM_SPARSE_VECTOR index with METADATA applies against
/// the real engine and is introspectable afterward.
#[cfg(feature = "integration")]
#[tokio::test]
async fn sparse_vector_index_applies_and_is_introspectable() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    let dir = TempSchema::new(&[(
        "item.sql",
        r#"CREATE DOCUMENT TYPE item IF NOT EXISTS;
           CREATE PROPERTY item.tag_tokens IF NOT EXISTS ARRAY_OF_INTEGERS;
           CREATE PROPERTY item.tag_weights IF NOT EXISTS ARRAY_OF_FLOATS;
           CREATE INDEX idx_item_sparse IF NOT EXISTS ON item(tag_tokens, tag_weights)
               LSM_SPARSE_VECTOR METADATA { "dimensions": 20000, "modifier": "IDF", "weightQuantization": "FP32" };"#,
    )]);

    let report = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync");
    assert!(
        report
            .applied_statements
            .iter()
            .any(|s| s.contains("LSM_SPARSE_VECTOR")),
        "expected sparse-vector DDL: {:?}",
        report.applied_statements
    );

    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    let g = &actual.types["item"];
    assert!(
        g.indexes.iter().any(
            |i| i.kind == arcadedb_migrate::schema::IndexKind::LsmSparseVector
                && i.columns == vec!["tag_tokens", "tag_weights"]
        ),
        "sparse index missing from introspection: {:?}",
        g.indexes
    );
}

/// Regression: an override whose swap statement fails must NOT be recorded as
/// applied.
///
/// `sync()` used to record pending overrides *before* executing anything; a
/// failed batch left a recorded-but-never-executed override that would be
/// skipped forever on subsequent syncs. Recording now happens only after the
/// override's copy phase verified+committed AND every swap statement ran.
/// (`ALTER PROPERTY ... TYPE` classifies as a swap statement — the engine
/// rejects retyping, so the swap phase fails standalone.)
#[cfg(feature = "integration")]
#[tokio::test]
async fn failed_override_swap_is_not_recorded_as_applied() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Round 1: settle a base schema (external_id as STRING).
    let base = TempSchema::new(&[(
        "item.sql",
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.external_id IF NOT EXISTS STRING;",
    )]);
    migrator
        .sync_dir(&base.path(), DropStrategy::Never)
        .await
        .expect("base sync");

    // Round 2: an override whose swap statement the engine rejects — the swap
    // phase must fail (reporting exactly which statement) and the override
    // must NOT be recorded as applied.
    let broken = tempfile::tempdir().unwrap();
    std::fs::write(
        broken.path().join("item.sql"),
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.external_id IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(broken.path().join("overrides")).unwrap();
    std::fs::write(
        broken.path().join("overrides/001_retype.sql"),
        "ALTER PROPERTY item.external_id TYPE LONG;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&broken.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect_err("engine-rejected override must fail the sync");
    let msg = format!("{err}");
    assert!(
        msg.contains("swap statement #1 failed"),
        "expected swap-phase error naming the statement, got: {msg}"
    );
    assert!(
        msg.contains("after 0 already executed"),
        "expected the swap progress count, got: {msg}"
    );

    // The bookkeeping must be untouched: no override row, schema unchanged.
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied overrides");
    assert!(
        applied.is_empty(),
        "failed override must not be recorded as applied, got: {applied:?}"
    );
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert_eq!(
        actual.types["item"].properties["external_id"].type_name, "STRING",
        "the engine must have rejected the in-place retype outright"
    );

    // Round 3: the same file (now valid) must run — proving the failed attempt
    // didn't permanently skip it.
    let fixed = tempfile::tempdir().unwrap();
    std::fs::write(
        fixed.path().join("item.sql"),
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.external_id IF NOT EXISTS STRING;
         CREATE PROPERTY item.external_id_lng IF NOT EXISTS LONG;",
    )
    .unwrap();
    std::fs::create_dir_all(fixed.path().join("overrides")).unwrap();
    std::fs::write(
        fixed.path().join("overrides/001_retype.sql"),
        "CREATE PROPERTY item.external_id_lng IF NOT EXISTS LONG;",
    )
    .unwrap();
    let report = migrator
        .sync_dir(&fixed.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("sync after fixing the override");
    assert!(
        report
            .applied_overrides
            .iter()
            .any(|o| o.contains("001_retype")),
        "fixed override must run on retry, got: {:?}",
        report.applied_overrides
    );
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied overrides");
    assert_eq!(applied.len(), 1, "exactly one override row expected");
}

/// Applied-migration immutability: an override that was edited
/// *after* being applied must be rejected outright — its statements already
/// changed the DB, so re-running the new content can never be safe. An
/// unchanged re-run stays a clean no-op.
#[cfg(feature = "integration")]
#[tokio::test]
async fn edited_override_is_rejected_after_apply() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let _base = settle_hero_with_rows(&migrator, &client).await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("overrides")).unwrap();
    let override_path = dir.path().join("overrides/001_add_type.sql");
    std::fs::write(&override_path, "CREATE DOCUMENT TYPE tagged IF NOT EXISTS;").unwrap();
    let dir_str = dir.path().to_string_lossy();

    // First sync applies the override.
    let r1 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("first sync");
    assert!(
        r1.applied_overrides
            .contains(&"001_add_type.sql".to_string()),
        "override must apply, got: {:?}",
        r1.applied_overrides
    );

    // Unchanged file → clean no-op (integrity passes, nothing pending).
    let r2 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("unchanged re-run");
    assert!(r2.no_op, "unchanged re-run must be a no-op");
    assert!(r2.applied_overrides.is_empty());

    // Edit the APPLIED override → hard error naming the file.
    std::fs::write(
        &override_path,
        "-- edited after apply\nCREATE DOCUMENT TYPE tagged IF NOT EXISTS;\n\
         CREATE PROPERTY tagged.label IF NOT EXISTS STRING;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect_err("edited applied override must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("was modified after it was applied"),
        "expected immutability error, got: {msg}"
    );
    assert!(
        msg.contains("001_add_type.sql"),
        "error must name the file: {msg}"
    );

    // The edited statements never ran: `tagged.label` must not exist.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(
        !actual.types["tagged"].properties.contains_key("label"),
        "edited override must not have been executed"
    );
}

/// Retry safety after a partial swap-phase failure: the committed copy phase
/// is journaled (in-transaction marker) and each executed swap statement is
/// recorded, so the re-run resumes at the failure point instead of repeating
/// durable work — the critical guard against duplicating every copied row.
#[cfg(feature = "integration")]
#[tokio::test]
async fn partial_swap_failure_resumes_without_recoping() {
    use arcadedb_protocol::grpc_record_to_json;

    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let base = settle_hero_with_rows(&migrator, &client).await;

    // Pre-create the rename target: swap statement #2 (`ALTER TYPE … NAME`)
    // fails while #1 (a harmless delete) already ran.
    client
        .execute(TEST_DB, "CREATE DOCUMENT TYPE hero2")
        .await
        .expect("create blocker");

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("overrides")).unwrap();
    std::fs::write(
        dir.path().join("overrides/001_rebuild.sql"),
        "CREATE DOCUMENT TYPE hero_lng IF NOT EXISTS;
         CREATE PROPERTY hero_lng.name IF NOT EXISTS STRING;
         INSERT INTO hero_lng FROM SELECT * FROM hero;
         DELETE FROM hero WHERE name = 'nonexistent';
         ALTER TYPE hero_lng NAME hero2;",
    )
    .unwrap();
    let dir_str = dir.path().to_string_lossy();

    // Round 1: copy commits + verifies, swap #1 runs, swap #2 fails.
    let err = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect_err("rename onto an existing type must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("swap statement #2 failed after 1 already executed"),
        "expected per-statement swap error, got: {msg}"
    );
    assert_eq!(
        count_rows(&client, "hero_lng").await,
        3,
        "verified copy durable after swap failure"
    );

    // Progress journaled: copy marker + swap ordinal 0; override unrecorded.
    let prog = client
        .query(
            TEST_DB,
            "SELECT kind, ordinal FROM schema_override_progress",
        )
        .await
        .expect("query progress");
    let mut kinds: Vec<String> = prog
        .records
        .iter()
        .filter_map(|rec| {
            grpc_record_to_json(rec)
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec!["copy".to_string(), "swap".to_string()]);
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied");
    assert!(applied.is_empty(), "unfinished override must be unrecorded");

    // Round 2: remove the blocker and re-run the SAME file unmodified.
    client
        .execute(TEST_DB, "DROP TYPE hero2")
        .await
        .expect("drop blocker");
    let r2 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("resumed sync");
    assert!(
        r2.applied_overrides
            .contains(&"001_rebuild.sql".to_string()),
        "override must complete on resume, got: {:?}",
        r2.applied_overrides
    );

    // THE assertion: the copy phase was skipped (marker inside the committed
    // transaction) — a naive retry would have doubled hero_lng's rows here.
    assert_eq!(
        count_rows(&client, "hero2").await,
        3,
        "resume must NOT re-execute the copy phase (rows duplicated otherwise)"
    );
    assert_eq!(
        count_rows(&client, "hero").await,
        3,
        "original type untouched by the rebuild"
    );

    // Bookkeeping settled: override recorded, progress cleared.
    let applied = client
        .query(TEST_DB, "SELECT filename FROM schema_overrides_applied")
        .await
        .expect("query applied");
    assert_eq!(applied.len(), 1, "exactly the resumed override expected");
    let prog = client
        .query(
            TEST_DB,
            "SELECT count(*) AS n FROM schema_override_progress",
        )
        .await
        .expect("query progress");
    let rec = prog.records.first().expect("count record");
    use arcadedb_protocol::proto::com::arcadedb::grpc::grpc_value::Kind;
    let n = match rec.properties.get("n").and_then(|v| v.kind.clone()) {
        Some(Kind::Int64Value(n)) => n,
        Some(Kind::Int32Value(n)) => n as i64,
        other => panic!("unexpected count kind: {other:?}"),
    };
    assert_eq!(n, 0, "progress rows must be cleared after completion");

    let _ = base;
}

/// Rename guard: an override file whose content is byte-identical to an
/// already-applied override under a DIFFERENT filename must be rejected —
/// renaming an applied migration would re-execute it.
#[cfg(feature = "integration")]
#[tokio::test]
async fn renamed_override_with_applied_content_is_rejected() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let _base = settle_hero_with_rows(&migrator, &client).await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    const BODY: &str = "CREATE DOCUMENT TYPE tagged IF NOT EXISTS;";
    std::fs::write(overrides_dir.join("001_a.sql"), BODY).unwrap();
    let dir_str = dir.path().to_string_lossy();

    migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("first sync applies 001_a.sql");

    // Same content, new filename → the rename guard fires.
    std::fs::write(overrides_dir.join("002_renamed.sql"), BODY).unwrap();
    let err = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect_err("renamed duplicate of an applied override must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("identical content"),
        "expected rename-guard error, got: {msg}"
    );
    assert!(
        msg.contains("002_renamed.sql") && msg.contains("001_a.sql"),
        "error must name both files: {msg}"
    );
}

/// Directory manifest (`overrides.sum`): once present, every sync verifies the
/// overrides directory against it — edits and deletions are rejected until
/// the manifest is regenerated.
#[cfg(feature = "integration")]
#[tokio::test]
async fn manifest_tamper_is_detected_on_sync() {
    use sha2::{Digest, Sha256};

    fn sha(body: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(body);
        hex::encode(h.finalize())
    }

    // Mirror of migrator::manifest's format (tests can't reach pub(crate)).
    fn write_manifest_file(overrides_dir: &std::path::Path) {
        let mut names: Vec<String> = std::fs::read_dir(overrides_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        names.sort();
        let mut lines = Vec::new();
        for name in &names {
            let body = std::fs::read(overrides_dir.join(name)).unwrap();
            lines.push(format!("h1:{} {name}", sha(&body)));
        }
        let mut h = Sha256::new();
        for line in &lines {
            h.update(line.as_bytes());
            h.update(b"\n");
        }
        lines.push(format!("h1:{}", hex::encode(h.finalize())));
        std::fs::write(
            overrides_dir.join("overrides.sum"),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
    }

    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");
    let _base = settle_hero_with_rows(&migrator, &client).await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;
         CREATE PROPERTY hero.name IF NOT EXISTS STRING;",
    )
    .unwrap();
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    let override_path = overrides_dir.join("001_tag.sql");
    std::fs::write(&override_path, "CREATE DOCUMENT TYPE tagged IF NOT EXISTS;").unwrap();
    write_manifest_file(&overrides_dir);
    let dir_str = dir.path().to_string_lossy();

    // Manifest current → sync passes.
    migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("sync with current manifest");

    // Tamper with a listed override → MANIFEST layer fires first (directory
    // integrity is checked before the DB registry).
    std::fs::write(
        &override_path,
        "-- sneaky edit\nCREATE DOCUMENT TYPE tagged IF NOT EXISTS;",
    )
    .unwrap();
    let err = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect_err("tampered manifest must fail the sync");
    let msg = format!("{err}");
    assert!(
        msg.contains("reading overrides") && msg.contains("was edited since"),
        "expected manifest verification error, got: {msg}"
    );

    // Regenerate the manifest (acknowledging the directory change): now the
    // DB REGISTRY layer fires instead — the override was already APPLIED with
    // its original content, and applied migrations are immutable regardless
    // of what the manifest says. Two layers, two truths, both enforced.
    write_manifest_file(&overrides_dir);
    let err = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect_err("editing an applied override stays forbidden");
    let msg = format!("{err}");
    assert!(
        msg.contains("was modified after it was applied"),
        "expected immutability error after regeneration, got: {msg}"
    );
}

/// Rollout parity (F4): `--apply-rollout`'s engine — parse a rendered rollout
/// and apply it via [`Migrator::apply_rollout`] — must run overrides through
/// the SAME phase machinery as live sync (verified copy), record the desired
/// `default_exprs` into the snapshot (no default-wipe), and leave the next
/// direct sync a clean no-op.
#[cfg(feature = "integration")]
#[tokio::test]
async fn reviewed_rollout_applies_with_phase_safety_and_preserves_defaults() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Base: hero with a DEFAULT — the value whose recorded source text the old
    // rollout path used to wipe.
    let base = TempSchema::new(&[(
        "hero.sql",
        r#"CREATE DOCUMENT TYPE hero IF NOT EXISTS;
           CREATE PROPERTY hero.name IF NOT EXISTS STRING (DEFAULT "anon");"#,
    )]);
    migrator
        .sync_dir(&base.path(), DropStrategy::Never)
        .await
        .expect("base sync");
    client
        .execute_language(
            TEST_DB,
            "sqlscript",
            "BEGIN;\nINSERT INTO hero SET name = 'a';\nINSERT INTO hero SET name = 'b'; \
             \nINSERT INTO hero SET name = 'c';\nCOMMIT;",
        )
        .await
        .expect("seed rows");

    // Schema evolution: additive property + a swap-class override.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        r#"CREATE DOCUMENT TYPE hero IF NOT EXISTS;
           CREATE PROPERTY hero.name IF NOT EXISTS STRING (DEFAULT "anon");
           CREATE PROPERTY hero.tag IF NOT EXISTS STRING;"#,
    )
    .unwrap();
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    std::fs::write(
        overrides_dir.join("001_touch.sql"),
        "UPDATE hero SET name = name WHERE name = 'nonexistent';",
    )
    .unwrap();
    let dir_str = dir.path().to_string_lossy();

    // Write the reviewed artifact, then apply it through the library engine.
    let plan = migrator
        .plan_full(&dir_str, DropStrategy::Never)
        .await
        .expect("plan");
    let body = render_rollout(&plan);
    let parsed = parse_rollout(&body).expect("v2 rollout parses");
    assert_eq!(parsed.overrides.len(), 1, "one override section expected");

    // The caller (CLI) draws default_exprs from the desired schema; here we
    // build the same map explicitly.
    let mut exprs = std::collections::BTreeMap::new();
    exprs.insert("hero.name".to_string(), "\"anon\"".to_string());
    let report = migrator
        .apply_rollout(&parsed, &exprs)
        .await
        .expect("rollout apply");
    assert_eq!(report.applied_overrides, vec!["001_touch.sql"]);
    assert_eq!(count_rows(&client, "hero").await, 3, "data untouched");

    // THE regression assertion: because the snapshot now carries the desired
    // default text, the next direct sync is a CLEAN no-op — no spurious
    // `ALTER PROPERTY … default` churn from a wiped default_exprs map.
    let r2 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("post-rollout sync");
    assert!(
        r2.no_op,
        "next sync must be a clean no-op after a v2 rollout apply, got: {r2:?}"
    );
}

/// TimeSeries first-class support, end-to-end against a server WITH the TS
/// engine enabled: the declarative CREATE applies through the normal sync,
/// a second sync is a clean no-op, and the type is introspectable. Skips
/// (passes) on servers where the TS engine can't initialize — point
/// `ARCADEDB_ADDR` at an enabled server to exercise it.
#[cfg(feature = "integration")]
#[tokio::test]
async fn timeseries_type_declarative_sync_end_to_end() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;

    // Capability probe: does THIS server run the TimeSeries engine?
    match client
        .execute_language(
            TEST_DB,
            "sqlscript",
            "BEGIN;\n\
             CREATE TIMESERIES TYPE __probe_ts TIMESTAMP ts FIELDS (f DOUBLE);\n\
             COMMIT;",
        )
        .await
    {
        Ok(_) => {
            let _ = client
                .execute(TEST_DB, "DROP TIMESERIES TYPE IF EXISTS __probe_ts")
                .await;
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("TimeSeries") {
                eprintln!("skipping: server has no TimeSeries engine ({msg})");
                return;
            }
            panic!("capability probe failed unexpectedly: {msg}");
        }
    }

    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Declarative base + additive document change alongside the TS type.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("metrics.sql"),
        "CREATE TIMESERIES TYPE SensorReading \
         TIMESTAMP ts PRECISION SECOND \
         TAGS (sensor_id LONG) \
         FIELDS (temperature INTEGER, humidity INTEGER) \
         SHARDS 2;\n\
         CREATE DOCUMENT TYPE hero IF NOT EXISTS;",
    )
    .unwrap();
    let dir_str = dir.path().to_string_lossy();

    let r1 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("first sync creates the timeseries type");
    assert!(
        r1.applied_statements
            .iter()
            .any(|s| s.contains("CREATE TIMESERIES TYPE")),
        "expected TS DDL in report: {:?}",
        r1.applied_statements
    );

    // Ingested samples don't disturb the schema (records ≠ drift).
    client
        .execute_language(
            TEST_DB,
            "sqlscript",
            "BEGIN;\nINSERT INTO SensorReading (ts, external_id, temperature, humidity) VALUES \
             (1787270400, 1001, 34, 5);\nCOMMIT;",
        )
        .await
        .expect("insert sample");

    // Second sync: clean no-op (spec + flattened columns all settle).
    let r2 = migrator
        .sync_dir(&dir_str, DropStrategy::Never)
        .await
        .expect("second sync");
    assert!(r2.no_op, "TS types must reconcile to a no-op, got: {r2:?}");

    // Introspected spec is populated.
    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    let ts = &actual.types["SensorReading"];
    assert_eq!(ts.kind, arcadedb_migrate::schema::TypeKind::Timeseries);
    let spec = ts.timeseries.as_ref().expect("spec introspected");
    assert_eq!(spec.timestamp_column, "ts");
}

/// Advisory lock: while another run holds a live lease, sync refuses with an
/// error naming the holder; after release (or lease expiry) it proceeds.
#[cfg(feature = "integration")]
#[tokio::test]
async fn concurrent_sync_is_blocked_by_the_schema_lock() {
    use arcadedb_migrate::schema::revisions::{acquire_lock, release_lock};

    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // A foreign run grabs the lock with a long lease.
    acquire_lock(&client, TEST_DB, "intruder-run", 120_000)
        .await
        .expect("foreign run takes the lock");

    let dir = TempSchema::new(&[("hero.sql", "CREATE DOCUMENT TYPE hero IF NOT EXISTS;")]);
    let err = migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect_err("sync must refuse while the lock is held elsewhere");
    let msg = format!("{err}");
    assert!(
        msg.contains("holds the schema lock") && msg.contains("intruder-run"),
        "expected lock-conflict error naming the holder, got: {msg}"
    );

    // Released (or crashed + expired) → sync proceeds normally.
    release_lock(&client, TEST_DB, "intruder-run")
        .await
        .expect("release");
    migrator
        .sync_dir(&dir.path(), DropStrategy::Never)
        .await
        .expect("sync works once the lock is free");
}

/// Override ownership (F6): a helper type an override leaves behind is
/// intentional, not drift — a destructive sync drops genuine leftovers (`hero`)
/// but never override-owned objects (`helper_scratch`).
#[cfg(feature = "integration")]
#[tokio::test]
async fn override_owned_type_survives_destructive_sync() {
    let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().await;
    let client = fresh_client().await;
    let migrator = Migrator::new(client.clone(), TEST_DB)
        .await
        .expect("migrator");

    // Base schema settles FIRST (a snapshot must exist so the override below
    // is a live migration, not baseline-consumed).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hero.sql"),
        "CREATE DOCUMENT TYPE hero IF NOT EXISTS;",
    )
    .unwrap();
    migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("base sync");

    // Then an override that leaves a helper type behind.
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir_all(&overrides_dir).unwrap();
    std::fs::write(
        overrides_dir.join("001_helper.sql"),
        "CREATE DOCUMENT TYPE helper_scratch IF NOT EXISTS;",
    )
    .unwrap();
    migrator
        .sync_dir(&dir.path().to_string_lossy(), DropStrategy::Never)
        .await
        .expect("override applies");
    assert!(
        arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
            .await
            .expect("introspect")
            .types
            .contains_key("helper_scratch"),
        "precondition: helper type exists"
    );

    // Schema pivot: desired no longer declares `hero`. Under Explicit both
    // `hero` and `helper_scratch` exist in the DB but not in desired — yet
    // only `hero` may be dropped, because the registry's stored statements
    // prove `helper_scratch` is override-owned.
    let pivoted = tempfile::tempdir().unwrap();
    std::fs::write(
        pivoted.path().join("other.sql"),
        "CREATE DOCUMENT TYPE other IF NOT EXISTS;",
    )
    .unwrap();
    // Same override file content → same checksum → passes the immutability
    // check; its stored statements still prove `helper_scratch` ownership.
    std::fs::create_dir_all(pivoted.path().join("overrides")).unwrap();
    std::fs::write(
        pivoted.path().join("overrides/001_helper.sql"),
        "CREATE DOCUMENT TYPE helper_scratch IF NOT EXISTS;",
    )
    .unwrap();

    let report = migrator
        .sync_dir(&pivoted.path().to_string_lossy(), DropStrategy::Explicit)
        .await
        .expect("destructive sync");
    assert!(
        report
            .applied_statements
            .iter()
            .any(|s| s.contains("DROP TYPE hero")),
        "unowned leftover must be dropped: {:?}",
        report.applied_statements
    );
    assert!(
        !report
            .applied_statements
            .iter()
            .any(|s| s.contains("helper_scratch")),
        "override-owned type must survive: {:?}",
        report.applied_statements
    );

    let actual = arcadedb_migrate::schema::introspect::fetch_actual(&client, TEST_DB)
        .await
        .expect("introspect");
    assert!(!actual.types.contains_key("hero"), "unowned leftover gone");
    assert!(
        actual.types.contains_key("helper_scratch"),
        "owned type survives"
    );
}
