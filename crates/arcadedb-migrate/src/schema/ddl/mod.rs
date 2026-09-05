//! The "declaration tail" grammar unit — the piece of a `CREATE TYPE` /
//! `CREATE INDEX` statement this migrator deliberately does **not** interpret,
//! because the engine cannot round-trip it.
//!
//! Only two model structs still carry a [`Tail`], both for the same reason:
//!
//! - [`super::model::Type`] — the create-time-only type clauses (`BUCKETS`,
//!   `PAGESIZE`, `UNIDIRECTIONAL`, `LIGHTWEIGHT`, `UNIQUE`, `CUSTOM`).
//!   ArcadeDB has **no ALTER form** for them (`ALTER TYPE t BUCKETS 4` is a
//!   syntax error — verified), so they can exist at CREATE time only.
//! - [`super::model::Index`] — the `METADATA { ... }` body (a multi-key JSON
//!   grab-bag), which introspection doesn't surface.
//!
//! Every [`Tail`] follows the same three rules, defined here once:
//!
//! 1. **Verbatim** — the tail is source text, preserved byte-for-byte
//!    (trimmed at the edges) and re-emitted on first sync so a fresh DB gets
//!    the same declaration as the `.sql` file.
//! 2. **Never interpreted, never diffed** — a create-time clause's value or an
//!    index's metadata makes it through introspection lossily or not at all,
//!    and there's no ALTER to reconcile with, so the diff skips tail content
//!    entirely.
//! 3. **Checksummed** — editing the tail in the `.sql` must change the
//!    checksum and force a re-run.
//!
//! Property `DEFAULT` expressions use [`super::model::DefaultExpr`] instead
//! (same verbatim/checksum contract, but diffed by presence: the engine
//! resolves expressions at create time).

pub mod parse_util;

// Re-export the parse utilities so callers can still write `super::ddl::*`.
pub(crate) use parse_util::{
    ci_starts_with, find_ci, rfind_ci, split_at_first_paren, split_sql_statements, strip_comments,
    strip_if_not_exists_all,
};

use sha2::{Digest, Sha256};

/// A verbatim declaration tail. Either absent (`None`) or the source text as
/// written (edge-trimmed, interior bytes untouched).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Tail(Option<String>);

impl Tail {
    /// No tail — the common case for introspection (see `schema:types`).
    pub fn none() -> Self {
        Self(None)
    }

    /// Build from source text. Empty/whitespace-only input becomes `None`.
    pub fn from_text(text: &str) -> Self {
        let t = text.trim();
        if t.is_empty() {
            Self(None)
        } else {
            Self(Some(t.to_string()))
        }
    }

    /// The tail text, or `None` if absent.
    pub fn text(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Render as a leading-space-separated suffix ready to append to rendered
    /// DDL (`""` when absent).
    pub fn render_suffix(&self) -> String {
        self.text().map(|t| format!(" {t}")).unwrap_or_default()
    }

    /// Fold into a checksum (no-op when absent).
    pub fn hash_into(&self, hasher: &mut Sha256) {
        if let Some(t) = &self.0 {
            hasher.update(t.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_roundtrips_and_renders() {
        let t = Tail::from_text("EXTENDS Person ");
        assert_eq!(t.text(), Some("EXTENDS Person"));
        assert_eq!(t.render_suffix(), " EXTENDS Person");

        let empty = Tail::from_text("   \n  ");
        assert_eq!(empty.text(), None);
        assert_eq!(empty.render_suffix(), "");
        assert_eq!(Tail::none(), empty);
    }
}
