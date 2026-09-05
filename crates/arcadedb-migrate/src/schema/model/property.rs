//! [`Property`], [`Constraints`], and [`DefaultExpr`] — a property's type,
//! its attribute block, and the verbatim default-expression pocket.

use serde::{Deserialize, Serialize};

/// A property on a type.
///
/// Constraints are real fields (see [`Constraints`]) — the parser reads them
/// out of the `CREATE PROPERTY` attribute block and introspection reads them
/// off the `schema:types` row properties entry, so the diff compares
/// constraint-by-constraint and emits precise `ALTER PROPERTY` statements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    pub name: String,
    /// The ArcadeDB type name, normalized to the canonical UPPERCASE form:
    /// `"{type} OF {ofType}"` when the type has an of-type (`LIST OF STRING`),
    /// else the raw type (`STRING`, `INTEGER`, `ARRAY_OF_INTEGERS`, `MAP`).
    pub type_name: String,
    pub constraints: Constraints,
}

impl Property {
    /// The property's type declaration as it appears in `CREATE PROPERTY` DDL:
    /// the canonical type name plus the attribute block, e.g.
    /// `LIST OF STRING (NOTNULL true, DEFAULT "user")`.
    pub fn type_clause(&self) -> String {
        let attrs = self.constraints.render_block();
        if attrs.is_empty() {
            self.type_name.clone()
        } else {
            format!("{} {attrs}", self.type_name)
        }
    }
}

/// The property attribute set (`(MANDATORY true, NOTNULL true, MIN 1, ...)`)
/// from a `CREATE PROPERTY` block / a `schema:types` property entry — fully
/// structured, except `default` (see [`DefaultExpr`]).
///
/// Only the attributes the engine actually supports are modeled; anything
/// else is a parse error (strict parser). Boolean flags are plain `bool`:
/// the engine surfaces them only when `true` and resets them with `false`
/// (`ALTER PROPERTY t.x mandatory false`), so absent == false on both sides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    /// `MANDATORY true/false` — the engine surfaces the key only when `true`.
    pub mandatory: bool,
    /// `NOTNULL true/false`.
    pub not_null: bool,
    /// `READONLY true/false`.
    pub readonly: bool,
    /// `EXTERNAL true/false`. (Side effect: making a property external also
    /// creates an `externalBuckets` map on the type — not modeled.)
    pub external: bool,
    /// `MIN <string-value>` — introspection returns it as a string.
    pub min: Option<String>,
    /// `MAX <string-value>`.
    pub max: Option<String>,
    /// `REGEXP "<pattern>"`. The engine stores the pattern string (quote
    /// characters stripped), so the canonical value is the bare pattern.
    pub regexp: Option<String>,
    /// `DEFAULT <expr>` — the only verbatim pocket on a property (see
    /// [`DefaultExpr`]). Reconciled three-way against the
    /// [`super::SchemaSnapshot`].
    pub default: Option<DefaultExpr>,
}

impl Constraints {
    /// The `NAME value` keyword each constraint maps to in `ALTER PROPERTY`.
    /// (Pub for the diff engine's typed [`super::super::diff::DiffAction`]s.)
    pub fn boolean_keywords(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("mandatory", self.mandatory),
            ("notnull", self.not_null),
            ("readonly", self.readonly),
            ("external", self.external),
        ]
    }

    /// Render the attribute block — `(MANDATORY true, DEFAULT "user")` — in a
    /// canonical order, or `""` when no constraint is set. Values other than
    /// booleans and the default are **re-quoted** when they contain characters
    /// that would break an unquoted SQL token (a regexp like `[A-Za-z ]+`).
    pub fn render_block(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mandatory {
            parts.push("MANDATORY true".into());
        }
        if self.not_null {
            parts.push("NOTNULL true".into());
        }
        if self.readonly {
            parts.push("READONLY true".into());
        }
        if self.external {
            parts.push("EXTERNAL true".into());
        }
        for (kw, v) in [
            ("MIN", &self.min),
            ("MAX", &self.max),
            ("REGEXP", &self.regexp),
        ] {
            if let Some(val) = v {
                parts.push(format!("{kw} {}", quote_value(val)));
            }
        }
        if let Some(d) = &self.default {
            parts.push(d.render());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("({})", parts.join(", "))
        }
    }
}

/// A property `DEFAULT` expression — the **only** verbatim pocket left on a
/// property (it models one thing, the default expression, and is treated
/// as such).
///
/// On the desired side it is source text preserved byte-for-byte (`DEFAULT
/// "user"`, `DEFAULT date()`, `DEFAULT []`), re-emitted verbatim on `CREATE`.
/// On the *introspected* side it holds the stored text the engine surfaced
/// (`DEFAULT date()` introspects as `date()`; string literals keep their
/// quotes) — so telling drift apart from source edits requires the recorded
/// [`super::SchemaSnapshot`], never desired-text vs. introspected-value alone.
///
/// See the module doc for the three-way reconciliation rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultExpr(String);

impl DefaultExpr {
    /// Build from the source-text expression (edge-trimmed). Empty becomes a
    /// present-but-empty expression (`DEFAULT ""` round-trips: the source
    /// `(DEFAULT "")` parses to an empty-string expr, not no-default).
    pub fn new(text: &str) -> Self {
        Self(text.trim().to_string())
    }

    /// The raw expression text.
    pub fn text(&self) -> &str {
        &self.0
    }

    /// Render the full `DEFAULT <expr>` attribute.
    pub fn render(&self) -> String {
        format!("DEFAULT {}", self.0)
    }
}

/// Quote a min/max/regexp value for DDL if it contains characters that would
/// break an unquoted single token (whitespace, `[...]`, quotes, ...). The
/// canonical stored value is the bare string; this only affects rendering.
pub(crate) fn quote_value(v: &str) -> String {
    if v.is_empty()
        || v.chars()
            .any(|c| !(c.is_ascii_alphanumeric() || "_.@:-".contains(c)))
    {
        format!("\"{}\"", v.replace('"', "\\\""))
    } else {
        v.to_string()
    }
}
