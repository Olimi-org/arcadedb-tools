//! [`Index`] and [`IndexKind`] — the index model.

use serde::{Deserialize, Serialize};

use super::super::ddl::Tail;

/// An index on a type. May be unnamed — ArcadeDB auto-names single-column
/// indexes as `Type[col]`, which is what `SEARCH_INDEX` looks up, so unnamed
/// indexes must be matched by `(columns, kind)` rather than by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// `None` for auto-named indexes (created without an explicit name).
    pub name: Option<String>,
    /// The indexed column(s), in declaration order.
    pub columns: Vec<String>,
    pub kind: IndexKind,
    /// Whether the index enforces uniqueness. Introspection surfaces this
    /// (`"unique": true`), so it's verified, not assumed.
    pub unique: bool,
    /// The `METADATA { ... }` body (for `LSM_SPARSE_VECTOR` etc.) — the raw
    /// text between the braces, preserved verbatim. Because the parser stores
    /// the *inner* JSON, the renderer re-wraps it as ` METADATA { ... }`.
    ///
    /// One of the two remaining verbatim pockets (the engine does not surface
    /// metadata via `schema:types`) — see [`super::super::ddl`] for the contract.
    pub tail: Tail,
}

impl Index {
    /// The `METADATA { ... }` body (text between the braces), or `None` if the
    /// `CREATE INDEX` had no metadata clause.
    pub fn metadata(&self) -> Option<&str> {
        self.tail.text()
    }
}

/// The ArcadeDB index kinds the parser + diff engine know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Unique,
    NotUnique,
    /// `UNIQUE_HASH` / `NOTUNIQUE_HASH` collapse into Unique/NotUnique +
    /// a hash variant — not represented separately for now (none of the
    /// current schemas use them).
    FullText,
    Geospatial,
    Hnsw,
    LsmVector,
    LsmSparseVector,
}

impl IndexKind {
    /// The ArcadeDB DDL keyword.
    pub fn ddl_keyword(self) -> &'static str {
        match self {
            IndexKind::Unique => "UNIQUE",
            IndexKind::NotUnique => "NOTUNIQUE",
            IndexKind::FullText => "FULL_TEXT",
            IndexKind::Geospatial => "GEOSPATIAL",
            IndexKind::Hnsw => "HNSW",
            IndexKind::LsmVector => "LSM_VECTOR",
            IndexKind::LsmSparseVector => "LSM_SPARSE_VECTOR",
        }
    }

    /// Parse from a DDL token or a `schema:indexes` `indexType` value.
    ///
    /// Introspection returns canonical names like `LSM_TREE`, `FULL_TEXT`,
    /// `LSM_SPARSE_VECTOR`; DDL uses the same keywords. Note: ArcadeDB reports
    /// plain `UNIQUE`/`NOTUNIQUE` indexes as `LSM_TREE` in introspection —
    /// uniqueness is a separate flag there, so callers should prefer
    /// [`IndexKind::from_introspect_with_unique`] for the introspection path.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "UNIQUE" | "UNIQUE_HASH" => Some(IndexKind::Unique),
            "NOTUNIQUE" | "NOTUNIQUE_HASH" => Some(IndexKind::NotUnique),
            "FULL_TEXT" => Some(IndexKind::FullText),
            "GEOSPATIAL" => Some(IndexKind::Geospatial),
            "HNSW" => Some(IndexKind::Hnsw),
            "LSM_VECTOR" => Some(IndexKind::LsmVector),
            "LSM_SPARSE_VECTOR" => Some(IndexKind::LsmSparseVector),
            _ => None,
        }
    }

    /// Parse the kind from an introspection `indexType`, given the `unique`
    /// flag (introspection collapses UNIQUE/NOTUNIQUE into `LSM_TREE`, and
    /// possibly `LSM_HASH`).
    pub fn from_introspect_with_unique(index_type: &str, unique: bool) -> Option<Self> {
        match index_type.to_ascii_uppercase().as_str() {
            "LSM_TREE" | "LSM_HASH" => Some(if unique {
                IndexKind::Unique
            } else {
                IndexKind::NotUnique
            }),
            other => IndexKind::from_keyword(other),
        }
    }
}
