//! The schema model — the shared representation used by the parser (desired),
//! introspection (actual), and the diff engine.
//!
//! Both "what the .sql files say we should have" and "what the database says
//! it has" normalize into [`Schema`], so diffing is structurally symmetric.
//!
//! The model is **structured**: type-level inheritance, property constraints
//! (mandatory / not-null / readonly / external / min / max / regexp) and index
//! shape are real fields, compared field-by-field by the diff engine. Only two
//! verbatim [`super::ddl::Tail`]s remain, and both exist for the same reason —
//! the engine cannot round-trip them:
//!
//! - [`type_def::Type::clause`] — create-time-only type clauses (`BUCKETS`,
//!   `PAGESIZE`, `UNIDIRECTIONAL`, ...) that ArcadeDB refuses to ALTER
//!   (verified: `ALTER TYPE t BUCKETS 4` is a syntax error).
//! - [`index::Index::metadata`] — the `METADATA { ... }` JSON body (not
//!   exposed by `schema:types`).
//!
//! Property `DEFAULT` expressions use the dedicated
//! [`property::DefaultExpr`] pocket: the engine resolves them at create
//! time (`DEFAULT date()` introspects as a timestamp), so they can never
//! round-trip in the [desired-text ↔ resolved-value] direction. Reconciling
//! them therefore needs the [`SchemaSnapshot`]: the migrator records what it
//! last applied (source text + the resolved state it produced) back into the
//! database, so the diff can compare like-with-like.

pub mod index;
pub mod property;
pub mod timeseries;
pub mod type_def;

pub use index::{Index, IndexKind};
pub(crate) use property::quote_value;
pub use property::{Constraints, DefaultExpr, Property};
pub use timeseries::{duration_ms, TimeseriesColumn, TimeseriesSpec, TsRole};
pub use type_def::{Type, TypeKind};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ddl::Tail;

/// A schema: a name-indexed set of types (documents / vertices / edges).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// type name → type definition. `BTreeMap` for deterministic iteration,
    /// so diffs are stable regardless of file/insertion order.
    pub types: BTreeMap<String, Type>,
}

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a type by name.
    pub fn type_(&self, name: &str) -> Option<&Type> {
        self.types.get(name)
    }

    /// Mutable access to a type, inserting an empty one if absent.
    pub fn type_or_insert(&mut self, name: &str, kind: TypeKind) -> &mut Type {
        self.types.entry(name.to_string()).or_insert_with(|| Type {
            name: name.to_string(),
            kind,
            extends: Vec::new(),
            clause: Tail::none(),
            properties: BTreeMap::new(),
            indexes: Vec::new(),
            timeseries: None,
        })
    }
}

/// A snapshot of the database's schema state as it stood **after** a
/// successful apply, recorded back into the DB (see [`super::revisions`]).
///
/// It is the reference for two comparisons the desired-vs-actual diff cannot
/// make on its own:
///
/// 1. **Drift detection** — an independent signal that the live schema changed
///    since we last applied it. `snapshot.state != actual` means someone (or an
///    override/seed) mutated the DB out from under the declarative schema, so
///    the sync must run even when the desired checksum is unchanged.
/// 2. **Default reconciliation** — the one field where desired *text* can never
///    equal the introspected *resolved* value (`DEFAULT date()` introspects as a
///    timestamp). The snapshot stores both sides of "what we applied":
///    - `state`: the resolved values the engine surfaced (like-with-like
///      comparison against the current actual),
///    - `default_exprs`: the desired source text we applied per property
///      (so a `.sql` edit like `date()` → `date('2020-01-01')` is detectable).
///
/// Default keys are `"{type}.{property}"` — unambiguous since type and
/// property names cannot contain `.` in ArcadeDB.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// The desired-schema checksum this snapshot corresponds to.
    pub checksum: String,
    /// The introspected schema right after the last apply (resolved values).
    pub state: Schema,
    /// `"{type}.{property}" → `DEFAULT` source text` applied to that property
    /// (empty map when the schema declares no defaults at all).
    pub default_exprs: BTreeMap<String, String>,
}

impl SchemaSnapshot {
    /// The default source-text we last applied to `type.property`, or `None`
    /// if the snapshot doesn't record one.
    pub fn applied_default_expr(&self, type_name: &str, property_name: &str) -> Option<&str> {
        self.default_exprs
            .get(&format!("{type_name}.{property_name}"))
            .map(String::as_str)
    }

    /// The resolved default value the engine surfaced for `type.property`
    /// after our last apply, or `None` if it had no default then.
    pub fn resolved_default(&self, type_name: &str, property_name: &str) -> Option<&str> {
        self.state
            .types
            .get(type_name)?
            .properties
            .get(property_name)?
            .constraints
            .default
            .as_ref()
            .map(|d| d.text())
    }
}
