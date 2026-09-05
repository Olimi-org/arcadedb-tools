//! The bookkeeping tables that record applied schema state, plus the desired-
//! schema checksum used to skip no-op syncs.
//!
//! Five tables (all created idempotently by [`ensure_tables`]):
//!
//! - `schema_revisions` — one row per declarative-sync run: a checksum of the
//!   desired schema, for the audit trail.
//! - `schema_overrides_applied` — one row per applied override-migration file,
//!   driving the "apply unapplied overrides in order" logic. The recorded
//!   checksum is re-verified on every sync: an override edited after it was
//!   applied is a hard error (applied migrations are immutable — write a new
//!   file instead). The file's statements are stored too, so the diff can
//!   derive which schema objects are override-*owned*.
//! - `schema_override_progress` — per-override resume state for the copy/swap
//!   phase engine; see `progress` for why this makes retries safe.
//! - `schema_snapshot` — a single row holding the
//!   [`SchemaSnapshot`](super::model::SchemaSnapshot): the
//!   **introspected** schema state as it stood after our last apply plus the
//!   desired `DEFAULT` source texts we applied. It is the drift detector
//!   (`snapshot.state != actual` → re-run even with an unchanged checksum) and
//!   the reference for three-way default reconciliation — see
//!   [`SchemaSnapshot`](super::model::SchemaSnapshot)
//!   and the diff module.
//! - `schema_migrate_lock` — singleton advisory lock (`lock`): a UNIQUE-
//!   indexed `key` row claimed with holder + lease expiry, so two concurrent
//!   syncs cannot interleave DDL against an auto-committing engine.

mod checksum;
mod lock;
mod overrides;
mod progress;
mod snapshot;
mod tables;

// Re-export everything that was previously `pub` in the old monolithic
// `revisions.rs`, so callers via `super::revisions::*` still work.
pub use checksum::checksum;
pub use lock::{acquire_lock, release_lock, LOCK_LEASE_MS};
pub use overrides::{applied_overrides, AppliedOverride};
pub use progress::{clear_progress, copy_marker_sql, mark_swap, read_progress, OverrideProgress};
pub use snapshot::{read_snapshot, write_snapshot};
pub use tables::{ensure_tables, record_override, record_revision};

/// The bookkeeping type name for declarative-sync state.
pub const REVISIONS_TYPE: &str = "schema_revisions";
/// The bookkeeping type name for applied override migrations.
pub const OVERRIDES_TYPE: &str = "schema_overrides_applied";
/// The bookkeeping type name for the applied-state snapshot.
pub const SNAPSHOT_TYPE: &str = "schema_snapshot";
/// The bookkeeping type name for per-override resume state.
pub const PROGRESS_TYPE: &str = "schema_override_progress";
/// The bookkeeping type name for the advisory migration lock.
pub const LOCK_TYPE: &str = "schema_migrate_lock";

#[cfg(test)]
mod tests {
    use crate::schema::model::{
        Constraints, DefaultExpr, Property, Schema, SchemaSnapshot, TypeKind,
    };
    use crate::schema::parser;

    #[test]
    fn snapshot_serde_roundtrips_the_full_model() {
        // The snapshot stores the whole introspected model as JSON — every
        // field must serialize and come back identical, or the drift signal
        // (`snapshot.state == actual`) would never fire correctly.
        let parsed = parser::parse(
            "CREATE VERTEX TYPE v EXTENDS vbase IF NOT EXISTS BUCKETS 8;
             CREATE PROPERTY v.name IF NOT EXISTS STRING (MANDATORY true, DEFAULT \"anon\");
             CREATE PROPERTY v.score IF NOT EXISTS INTEGER (MIN 0, MAX 100);
             CREATE INDEX idx_v_name IF NOT EXISTS ON v(name) UNIQUE;",
        )
        .unwrap();
        let mut defaults = std::collections::BTreeMap::new();
        defaults.insert("v.name".to_string(), "\"anon\"".to_string());
        let snap = SchemaSnapshot {
            checksum: "deadbeef".into(),
            state: parsed,
            default_exprs: defaults,
        };

        let json = serde_json::to_string(&snap).unwrap();
        let back: SchemaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap, "full-model serde round-trip must be lossless");
    }

    #[test]
    fn snapshot_roundtrip_preserves_resolved_default_text() {
        // The introspected (resolved-value) side is what drift compares
        // against; both `date()`-style resolved timestamps and literal values
        // must survive the JSON storage.
        let mut state = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("2026-08-09T19:34:04.747+00:00")),
            ..Constraints::default()
        };
        state
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "at".into(),
                Property {
                    name: "at".into(),
                    type_name: "DATETIME".into(),
                    constraints: c,
                },
            );
        let snap = SchemaSnapshot {
            checksum: "c".into(),
            state,
            default_exprs: Default::default(),
        };
        let back: SchemaSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(
            back.resolved_default("t", "at"),
            Some("2026-08-09T19:34:04.747+00:00")
        );
    }
}
