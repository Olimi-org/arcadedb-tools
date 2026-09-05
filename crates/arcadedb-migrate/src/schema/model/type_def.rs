//! [`Type`] and [`TypeKind`] — the document/vertex/edge type definition.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::ddl::Tail;
use super::index::Index;
use super::property::Property;
use super::timeseries::TimeseriesSpec;

/// A document / vertex / edge type and its properties + indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Type {
    pub name: String,
    pub kind: TypeKind,
    /// Super-type names from the `EXTENDS <a>, <b>` clause (mirrors
    /// introspection's `parentTypes`). Structured and diffed: structural adds
    /// emit `ALTER TYPE t SUPERTYPE +x` / `-x` (both verified to work).
    pub extends: Vec<String>,
    /// Create-time-only type clauses from `CREATE TYPE`, preserved **verbatim**
    /// as a [`Tail`]: `BUCKET <id>`/`BUCKETS <n>`, `PAGESIZE <size>`,
    /// `UNIDIRECTIONAL`/`LIGHTWEIGHT`/`UNIQUE`, `CUSTOM k = v`. These are
    /// deliberately **not** diffed — `ALTER TYPE` has no form for them
    /// (verified: `ALTER TYPE t BUCKETS 4` is a syntax error), so they're
    /// create-time-only (see [`super::super::ddl`] for the contract).
    pub clause: Tail,
    /// property name → definition.
    pub properties: BTreeMap<String, Property>,
    /// Indexes on this type. A `Vec` (not a map) because indexes can be
    /// unnamed (auto-named `Type[col]`), so there's no always-unique key.
    pub indexes: Vec<Index>,
    /// The TimeSeries declaration (`TIMESTAMP/TAGS/FIELDS/SHARDS/…`) when
    /// [`TypeKind::Timeseries`]. Create-time-only as a unit: the engine has
    /// no ALTER for any of it, so it's rendered on CREATE, checksummed, and
    /// compared desired-vs-actual to warn (never auto-altered) — see
    /// [`TimeseriesSpec`] and docs/migration-model.md.
    #[serde(default)]
    pub timeseries: Option<TimeseriesSpec>,
}

impl Type {
    /// The create-time-only clause tail (`BUCKETS 4`, `UNIDIRECTIONAL`, ...),
    /// or `None` if the `CREATE TYPE` statement had none after `EXTENDS`.
    pub fn clause(&self) -> Option<&str> {
        self.clause.text()
    }

    /// Set the create-time-only clause tail (used by the parser).
    pub fn set_clause(&mut self, tail: Tail) {
        self.clause = tail;
    }

    /// The raw clause tail — used by the diff engine when cloning a type's
    /// clause into a `CreateType` action.
    pub(crate) fn clause_tail(&self) -> &Tail {
        &self.clause
    }
}

/// The four ArcadeDB type kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeKind {
    Document,
    Vertex,
    Edge,
    /// Native TimeSeries type (`CREATE TIMESERIES TYPE …`, introspected as
    /// engine kind `"t"`).
    Timeseries,
}

impl TypeKind {
    /// The ArcadeDB DDL keyword (`DOCUMENT`, `VERTEX`, `EDGE`, `TIMESERIES`).
    pub fn ddl_keyword(self) -> &'static str {
        match self {
            TypeKind::Document => "DOCUMENT",
            TypeKind::Vertex => "VERTEX",
            TypeKind::Edge => "EDGE",
            TypeKind::Timeseries => "TIMESERIES",
        }
    }

    /// Parse from the `type` field of a `schema:types` row.
    pub fn from_introspect(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "document" => Some(TypeKind::Document),
            "vertex" => Some(TypeKind::Vertex),
            "edge" => Some(TypeKind::Edge),
            // TimeSeries types surface as a bare "t".
            "t" => Some(TypeKind::Timeseries),
            _ => None,
        }
    }
}
