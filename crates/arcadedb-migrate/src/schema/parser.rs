//! Hand-rolled DDL parser: `.sql` text → [`Schema`].
//!
//! Scope is deliberately narrow — it understands exactly the three statement
//! forms the schema files use:
//!
//! - `CREATE [DOCUMENT|VERTEX|EDGE] TYPE <name> [IF NOT EXISTS]
//!   [EXTENDS <a>, <b>] [<clause>]` — the super-type list is parsed into
//!   the `extends` field of [`Type`](super::model::Type); the remaining
//!   create-time-only clauses are kept verbatim (see [`ddl`](super::ddl)).
//! - `CREATE PROPERTY <type>.<prop> [IF NOT EXISTS] <type> [(<attr>...)]` —
//!   the attribute block is parsed into structured [`Constraints`] fields.
//!   Unknown attributes are a parse error (strict).
//! - `CREATE INDEX [<name>] [IF NOT EXISTS] ON <type> (<col>[, <col>]) <kind> [METADATA { ... }]`
//!
//! Anything else (ALTER, DROP, DML) is an error — those belong in override
//! migration files, which are passed through unparsed. Statement splitting is
//! quote- and brace-aware (shared [`super::ddl::parse_util`] primitives): a
//! `;` inside a string literal or a `METADATA { ... }` JSON block never splits
//! mid-statement, and `--` inside a literal is data, not a comment.

use crate::error::{MigrateError, Result};

/// Wrap a message as the [`MigrateError::Parse`] variant — the only error the
/// parser produces.
fn parse_error(message: String) -> MigrateError {
    MigrateError::Parse { message }
}

use super::ddl::{
    ci_starts_with, find_ci, split_at_first_paren, split_sql_statements, strip_if_not_exists_all,
    Tail,
};
use super::model::{
    Constraints, DefaultExpr, Index, IndexKind, Property, Schema, TimeseriesColumn, TimeseriesSpec,
    TsRole, TypeKind,
};

/// Parse a chunk of DDL text into a [`Schema`].
///
/// Multiple `parse()` results can be merged by inserting into the same
/// `Schema` (the parser is additive).
///
/// Strict: only `CREATE TYPE/PROPERTY/INDEX` statements are recognized; anything
/// else (DML, ALTER, DROP) is an error. Seed DML belongs in `seed/*.sql`, which
/// the migrator runs as a separate phase — not parsed here.
pub fn parse(text: &str) -> Result<Schema> {
    let mut schema = Schema::new();
    for (i, stmt) in split_sql_statements(text).into_iter().enumerate() {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        parse_statement(&mut schema, stmt)
            .map_err(|e| parse_error(format!("statement #{}: {e}", i + 1)))?;
    }
    Ok(schema)
}

/// The statement forms this parser understands, keyed by their leading
/// keywords. Dispatch is keyword-exact (see [`classify`]) rather than a
/// `starts_with` chain, so lookalike prefixes (`CREATE PROPERTY_EXTRA`, …) are
/// rejected as unsupported instead of being silently mis-parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The `CREATE` prefix is the semantic core of the enum (the parser only
// handles CREATE statements), so it stays even though clippy flags the
// shared prefix.
#[allow(clippy::enum_variant_names)]
enum StmtKind {
    CreateType,
    CreateTimeseriesType,
    CreateProperty,
    CreateIndex,
}

/// Match a statement's leading keywords against the known forms. `upper` must
/// be the ASCII-uppercased statement; only the first three whitespace tokens
/// are examined, so the rest (keywords, names, the `METADATA { ... }` body)
/// is left untouched and can be sliced verbatim by the parsers.
fn classify(upper: &str) -> Option<StmtKind> {
    let mut w = upper.split_whitespace();
    match (w.next(), w.next(), w.next()) {
        (Some("CREATE"), Some("DOCUMENT" | "VERTEX" | "EDGE"), Some("TYPE")) => {
            Some(StmtKind::CreateType)
        }
        (Some("CREATE"), Some("TIMESERIES"), Some("TYPE")) => Some(StmtKind::CreateTimeseriesType),
        (Some("CREATE"), Some("PROPERTY"), _) => Some(StmtKind::CreateProperty),
        (Some("CREATE"), Some("INDEX"), _) => Some(StmtKind::CreateIndex),
        _ => None,
    }
}

/// Case-insensitive (ASCII-only) prefix strip, without allocating. `None` if
/// the prefix doesn't match. Safe to slice at `prefix.len()` when it does,
/// because `prefix` is ASCII and the match is thus at a char boundary.
fn ci_strip_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    ci_starts_with(s, prefix).then(|| &s[prefix.len()..])
}

/// Parse a single DDL statement (already trimmed, no trailing `;`).
fn parse_statement(schema: &mut Schema, stmt: &str) -> Result<()> {
    let upper = stmt.to_ascii_uppercase();
    match classify(&upper) {
        Some(StmtKind::CreateType) => parse_create_type(schema, stmt),
        Some(StmtKind::CreateTimeseriesType) => parse_create_timeseries(schema, stmt),
        Some(StmtKind::CreateProperty) => parse_create_property(schema, stmt),
        Some(StmtKind::CreateIndex) => parse_create_index(schema, stmt),
        None => Err(parse_error(format!(
            "unsupported statement (only CREATE TYPE/PROPERTY/INDEX are parseable; use \
             seed/*.sql for DML or overrides/*.sql for ALTER/DROP): {stmt}"
        ))),
    }
}

/// `CREATE [DOCUMENT|VERTEX|EDGE] TYPE <name> [IF NOT EXISTS] [EXTENDS <a>, <b>] [<clause>]`
///
/// The `EXTENDS <a>, <b>` super-type list is parsed into [`Type::extends`]
/// (it mirrors introspection's `parentTypes`, so it's diffable). The remaining
/// type-level clauses — `BUCKETS`, `PAGESIZE`, `UNIDIRECTIONAL`, ... — are
/// preserved verbatim as a [`Tail`] and re-emitted by `to_sql()`. They are
/// create-time-only (the engine has no ALTER form for them) and not diffed.
fn parse_create_type(schema: &mut Schema, stmt: &str) -> Result<()> {
    // "CREATE <kind> TYPE <name> [IF NOT EXISTS] [EXTENDS ...] <clause>".
    // `classify` already confirmed the leading keywords; strip them
    // case-insensitively so the name and tail can be sliced verbatim.
    let after_create = ci_strip_prefix(stmt, "CREATE")
        .ok_or_else(|| parse_error(format!("not a CREATE TYPE: {stmt}")))?
        .trim_start();
    let (kind_tok, rest) = split_first_token(after_create);
    let kind = match kind_tok.to_ascii_uppercase().as_str() {
        "DOCUMENT" => TypeKind::Document,
        "VERTEX" => TypeKind::Vertex,
        "EDGE" => TypeKind::Edge,
        other => {
            return Err(parse_error(format!(
                "expected DOCUMENT/VERTEX/EDGE, got {other}"
            )))
        }
    };
    let rest = ci_strip_prefix(rest.trim_start(), "TYPE")
        .ok_or_else(|| parse_error(format!("expected TYPE in: {stmt}")))?
        .trim_start();

    let (name, tail) = split_first_token(rest);
    if name.is_empty() {
        return Err(parse_error(format!("missing type name in: {stmt}")));
    }

    // Everything after the name, minus any `IF NOT EXISTS` (which may precede
    // OR follow the clause — the renderer always emits it canonically).
    let (extends, clause) = split_extends(&strip_if_not_exists_all(tail));

    let entry = schema.type_or_insert(name, kind);
    entry.extends = extends;
    entry.set_clause(clause);
    Ok(())
}

/// Keywords that may follow a super-type list inside a `CREATE TYPE` clause —
/// they terminate the `EXTENDS` list so the remainder can be kept verbatim.
/// (Type names are plain identifiers, so they never collide with these.)
const TYPE_CLAUSE_KEYWORDS: [&str; 7] = [
    "BUCKET",
    "BUCKETS",
    "PAGESIZE",
    "UNIDIRECTIONAL",
    "LIGHTWEIGHT",
    "UNIQUE",
    "CUSTOM",
];

/// Split a `CREATE TYPE` clause tail into the `EXTENDS` super-type list and
/// the remaining create-time-only clause. The clause part is re-joined from
/// tokens with single spaces (the same normalization `strip_if_not_exists_all`
/// already applied).
fn split_extends(tail: &str) -> (Vec<String>, Tail) {
    let tokens: Vec<&str> = tail.split_whitespace().collect();
    let mut extends: Vec<String> = Vec::new();
    let mut clause_tokens: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].eq_ignore_ascii_case("EXTENDS") {
            i += 1;
            while i < tokens.len()
                && !TYPE_CLAUSE_KEYWORDS
                    .iter()
                    .any(|k| tokens[i].eq_ignore_ascii_case(k))
            {
                let tok = tokens[i].trim_end_matches(',');
                if !tok.is_empty() && !tok.eq_ignore_ascii_case(",") {
                    extends.push(tok.to_string());
                }
                i += 1;
            }
        } else {
            clause_tokens.push(tokens[i]);
            i += 1;
        }
    }

    (extends, Tail::from_text(&clause_tokens.join(" ")))
}

/// `CREATE TIMESERIES TYPE <name> [IF NOT EXISTS] TIMESTAMP <col>
/// [PRECISION <p>] [TAGS (<col TYPE>, …)] [FIELDS (<col TYPE>, …)]
/// [SHARDS <n>] [RETENTION <n> <unit>] [COMPACTION_INTERVAL <n> <unit>]
/// [BLOCK_SIZE <n>]`
///
/// Parsed structurally into [`TimeseriesSpec`]: the engine has no ALTER for
/// any of it, so the spec is rendered on CREATE and checksummed — never
/// diffed statement-by-statement (drift surfaces as a rebuild warning).
/// Columns are also flattened into ordinary properties so the checksum and
/// DTO-drift tooling see them like any other column.
fn parse_create_timeseries(schema: &mut Schema, stmt: &str) -> Result<()> {
    let mut after = ci_strip_prefix(stmt, "CREATE TIMESERIES TYPE")
        .ok_or_else(|| parse_error(format!("not a CREATE TIMESERIES TYPE: {stmt}")))?
        .trim_start();

    // IF NOT EXISTS may sit BEFORE the name (docs/user-command order).
    if let Some(rest) = ci_strip_prefix(after, "IF NOT EXISTS") {
        after = rest.trim_start();
    }

    let (name, mut rest) = split_first_token(after);
    if name.is_empty() {
        return Err(parse_error(format!("missing type name in: {stmt}")));
    }
    // …or AFTER the name (the renderer's canonical order).
    if rest
        .trim()
        .to_ascii_uppercase()
        .starts_with("IF NOT EXISTS")
    {
        rest = rest
            .trim()
            .strip_prefix("IF NOT EXISTS")
            .unwrap_or(rest)
            .trim_start();
    }

    // Atom walk over the clause: an "atom" is either a balanced parenthesized
    // group (`(a STRING, b LONG)`) or a single whitespace-delimited token.
    let mut spec = TimeseriesSpec::default();
    loop {
        let (keyword_raw, r) = next_atom(rest);
        if keyword_raw.is_empty() {
            break;
        }
        rest = r;
        match keyword_raw.to_ascii_uppercase().as_str() {
            "TIMESTAMP" => {
                let (atom, r) = next_atom(rest);
                if atom.is_empty() {
                    return Err(parse_error(format!(
                        "missing TIMESTAMP column name in: {stmt}"
                    )));
                }
                spec.timestamp_column = atom.to_string();
                rest = r;
            }
            "PRECISION" => {
                let (atom, r) = next_atom(rest);
                if atom.is_empty() {
                    return Err(parse_error(format!("missing PRECISION value in: {stmt}")));
                }
                spec.precision = Some(atom.to_string());
                rest = r;
            }
            "TAGS" | "FIELDS" => {
                let (group, r) = next_atom(rest);
                let role = if keyword_raw.eq_ignore_ascii_case("TAGS") {
                    TsRole::Tag
                } else {
                    TsRole::Field
                };
                *if role == TsRole::Tag {
                    &mut spec.tags
                } else {
                    &mut spec.fields
                } = parse_ts_columns(paren_group(group, stmt, keyword_raw)?, role)?;
                rest = r;
            }
            "SHARDS" => {
                let (atom, r) = next_atom(rest);
                spec.shards = Some(atom.parse().map_err(|_| {
                    parse_error(format!("SHARDS expects a number, got {atom:?}: {stmt}"))
                })?);
                rest = r;
            }
            "RETENTION" | "COMPACTION_INTERVAL" => {
                let (value, r1) = next_atom(rest);
                let (unit, r2) = next_atom(r1);
                if value.is_empty() || unit.is_empty() {
                    return Err(parse_error(format!(
                        "{keyword_raw} expects `<n> DAYS|HOURS|MINUTES|SECONDS`: {stmt}"
                    )));
                }
                let duration = format!("{value} {unit}");
                if crate::schema::model::duration_ms(&duration).is_none() {
                    return Err(parse_error(format!(
                        "{keyword_raw} expects `<n> DAYS|HOURS|MINUTES|SECONDS`, got {duration:?}"
                    )));
                }
                if keyword_raw.eq_ignore_ascii_case("RETENTION") {
                    spec.retention = Some(duration);
                } else {
                    spec.compaction_interval = Some(duration);
                }
                rest = r2;
            }
            "BLOCK_SIZE" => {
                let (atom, r) = next_atom(rest);
                spec.block_size = Some(atom.parse().map_err(|_| {
                    parse_error(format!("BLOCK_SIZE expects a number, got {atom:?}: {stmt}"))
                })?);
                rest = r;
            }
            other => {
                return Err(parse_error(format!(
                    "unexpected token in CREATE TIMESERIES TYPE: {other:?}"
                )))
            }
        }
    }

    if spec.timestamp_column.is_empty() {
        return Err(parse_error(format!(
            "CREATE TIMESERIES TYPE requires a TIMESTAMP column: {stmt}"
        )));
    }
    if spec.fields.is_empty() {
        return Err(parse_error(format!(
            "CREATE TIMESERIES TYPE requires at least one FIELDS column: {stmt}"
        )));
    }

    let entry = schema.type_or_insert(name, TypeKind::Timeseries);
    entry.timeseries = Some(spec.clone());
    // Flatten the declared columns into ordinary properties so the checksum,
    // diff-side property presence, and DTO-drift tooling treat them uniformly.
    for col in spec.columns() {
        entry.properties.insert(
            col.name.to_string(),
            Property {
                name: col.name.to_string(),
                type_name: col.data_type.to_ascii_uppercase(),
                constraints: Constraints::default(),
            },
        );
    }
    Ok(())
}

/// Take the next clause atom from `s`: either a balanced `(…)` group or one
/// whitespace-delimited token. Returns `(atom, rest)`; empty atom at EOS.
fn next_atom(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    if let Some(body) = s.strip_prefix('(') {
        let mut depth = 1usize;
        for (i, ch) in body.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return (&s[..i + 2], &s[i + 2..]);
                    }
                }
                _ => {}
            }
        }
        return (s, ""); // unbalanced — caller errors on the body
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// Validate and unwrap a `( … )` column-list group.
fn paren_group<'a>(group: &'a str, stmt: &str, keyword: &str) -> Result<&'a str> {
    if group.starts_with('(') && group.ends_with(')') && group.len() >= 2 {
        Ok(&group[1..group.len() - 1])
    } else {
        Err(parse_error(format!(
            "{keyword} expects a parenthesized column list like (a STRING, b LONG), \
             got {group:?} in: {stmt}"
        )))
    }
}

/// Parse `"name TYPE"` pairs from a TAGS/FIELDS list body; `role` labels every
/// column per its declaring section.
fn parse_ts_columns(body: &str, role: TsRole) -> Result<Vec<TimeseriesColumn>> {
    split_top_level_commas(body)
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            let item = item.trim();
            let mut parts = item.split_whitespace();
            let name = parts.next().unwrap_or_default().to_string();
            let data_type = parts.collect::<Vec<_>>().join(" ");
            if name.is_empty() || data_type.is_empty() {
                return Err(parse_error(format!(
                    "TimeSeries column expects `name TYPE`, got {item:?}"
                )));
            }
            Ok(TimeseriesColumn {
                name,
                data_type: data_type.to_ascii_uppercase(),
                role,
            })
        })
        .collect()
}

/// `CREATE PROPERTY <type>.<prop> [IF NOT EXISTS] <type> [(<attr>...)]`
fn parse_create_property(schema: &mut Schema, stmt: &str) -> Result<()> {
    // After "CREATE PROPERTY", the next token is "<type>.<prop>". `classify`
    // already confirmed the leading keyword (case-insensitively).
    let after = ci_strip_prefix(stmt, "CREATE PROPERTY")
        .ok_or_else(|| parse_error(format!("not a CREATE PROPERTY: {stmt}")))?
        .trim();

    // The qualified name is the first whitespace-delimited token, but it may be
    // surrounded by optional `IF NOT EXISTS`. Find "<word>.<word>".
    let (qualified, rest) = split_first_token(after);
    let (type_name, prop_name) = qualified
        .split_once('.')
        .ok_or_else(|| parse_error(format!("expected <type>.<property>, got {qualified}")))?;

    // Rest: [IF NOT EXISTS] <type> [(<attr> ...)].
    let rest = strip_if_not_exists(rest.trim());
    // The type is everything up to the first top-level `(`, so multi-word
    // types ("LIST OF INTEGER") and single tokens ("STRING") both split
    // cleanly; the cut is quote- and paren-balanced (see [`ddl::split_at_first_paren`]),
    // so a `(` inside a default expression (`DEFAULT date()`,
    // `DEFAULT ["a", f(1,2)]`) never corrupts it.
    let (type_name_value, attributes) = match split_at_first_paren(rest) {
        Some((ty, block)) => (ty.trim().to_ascii_uppercase(), Some(block)),
        None => (rest.trim().to_ascii_uppercase(), None),
    };
    if type_name_value.is_empty() {
        return Err(parse_error(format!("missing property type in: {stmt}")));
    }

    let constraints = match attributes {
        Some(block) => parse_attribute_block(&block[1..block.len() - 1])?,
        None => Constraints::default(),
    };

    // Ensure the type exists (it may have been declared separately or not at
    // all — CREATE PROPERTY implicitly creates the type if absent in ArcadeDB).
    let ty = schema
        .types
        .get(type_name)
        .map(|t| t.kind)
        .unwrap_or(TypeKind::Document);
    let entry = schema.type_or_insert(type_name, ty);
    entry.properties.insert(
        prop_name.to_string(),
        Property {
            name: prop_name.to_string(),
            type_name: type_name_value,
            constraints,
        },
    );
    Ok(())
}

/// Parse the interior of a property attribute block — `MANDATORY true,
/// DEFAULT "user"` (outer parens already stripped) — into [`Constraints`].
///
/// Recognized attributes: `MANDATORY`, `NOTNULL`, `READONLY`, `EXTERNAL`
/// (boolean), `MIN`, `MAX`, `REGEXP` (string value), `DEFAULT` (raw expression,
/// kept verbatim in a [`DefaultExpr`]). Anything else is an error — the engine
/// supports no other property attributes, and a strict error beats silently
/// dropping a future one.
fn parse_attribute_block(block: &str) -> Result<Constraints> {
    let mut c = Constraints::default();
    for item in split_top_level_commas(block) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, value) = match item.find(char::is_whitespace) {
            Some(i) => (&item[..i], item[i..].trim()),
            None => (item, ""),
        };
        let try_bool = |name: &str, v: &str| -> Result<bool> {
            if v.eq_ignore_ascii_case("true") {
                Ok(true)
            } else if v.eq_ignore_ascii_case("false") {
                Ok(false)
            } else {
                Err(parse_error(format!("{name} expects true/false, got {v:?}")))
            }
        };
        match name.to_ascii_uppercase().as_str() {
            "MANDATORY" => c.mandatory = try_bool(name, value)?,
            "NOTNULL" => c.not_null = try_bool(name, value)?,
            "READONLY" => c.readonly = try_bool(name, value)?,
            "EXTERNAL" => c.external = try_bool(name, value)?,
            "MIN" => c.min = Some(parse_string_value(name, value)?),
            "MAX" => c.max = Some(parse_string_value(name, value)?),
            "REGEXP" => c.regexp = Some(parse_string_value(name, value)?),
            "DEFAULT" => c.default = Some(DefaultExpr::new(value)),
            other => {
                return Err(parse_error(format!(
                    "unsupported property attribute {other:?} (supported: MANDATORY, NOTNULL, \
                     READONLY, EXTERNAL, MIN, MAX, REGEXP, DEFAULT)"
                )))
            }
        }
    }
    Ok(c)
}

/// The canonical value for a `MIN`/`MAX`/`REGEXP` attribute: surrounding quote
/// characters stripped, because introspection (`schema:types`) surfaces these
/// as bare strings (`REGEXP "[A-Za-z ]+"` comes back as `[A-Za-z ]+`). The
/// renderer re-quotes when needed (see [`super::model::quote_value`]).
fn parse_string_value(name: &str, value: &str) -> Result<String> {
    if value.is_empty() {
        return Err(parse_error(format!("{name} expects a value")));
    }
    let stripped = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    Ok(stripped.to_string())
}

/// Split a string on top-level commas: commas at paren/bracket/brace depth zero
/// split, commas inside `(...)`, `[...]`, `{...}` or quoted strings don't. This
/// is what lets `DEFAULT date()` (parens) and `DEFAULT ["a", date()]` (brackets
/// + a nested comma) survive a multi-attribute block unchanged.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    let mut in_squote = false;
    let mut in_dquote = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if in_squote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_squote = false;
            }
        } else if in_dquote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_dquote = false;
            }
        } else {
            match c {
                '\'' => in_squote = true,
                '"' => in_dquote = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
    }
    out.push(&s[start..]);
    out
}

/// `CREATE INDEX [<name>] [IF NOT EXISTS] ON <type> (<col>[, <col>]) <kind> [METADATA { ... }]`
fn parse_create_index(schema: &mut Schema, stmt: &str) -> Result<()> {
    let after = ci_strip_prefix(stmt, "CREATE INDEX")
        .ok_or_else(|| parse_error(format!("not a CREATE INDEX: {stmt}")))?
        .trim();

    // Optional name: if the next token isn't "IF" or "ON", it's the index name.
    let (name, rest) = if ci_starts_with(after, "ON ") || ci_starts_with(after, "IF ") {
        (None, after)
    } else {
        let (n, r) = split_first_token(after);
        (Some(n.to_string()), r.trim())
    };

    // Strip optional IF NOT EXISTS.
    let rest = strip_if_not_exists(rest);

    // Expect "ON <type> (cols) <kind> [METADATA {...}]"
    let rest = ci_strip_prefix(rest, "ON")
        .ok_or_else(|| parse_error(format!("expected ON in: {stmt}")))?
        .trim();

    // The type name + column list: `item` and `(tag_tokens, tag_weights)` are
    // NOT whitespace-separated, so find the `(` first and take the type name
    // as everything before it.
    let open = rest
        .find('(')
        .ok_or_else(|| parse_error(format!("expected '(' for columns in: {stmt}")))?;
    let type_name = rest[..open].trim();
    if type_name.is_empty() {
        return Err(parse_error(format!(
            "missing type name before '(' in: {stmt}"
        )));
    }
    let close = rest
        .find(')')
        .ok_or_else(|| parse_error(format!("expected ')' for columns in: {stmt}")))?;
    let cols_inner = &rest[open + 1..close];
    let columns: Vec<String> = cols_inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if columns.is_empty() {
        return Err(parse_error(format!("index has no columns: {stmt}")));
    }

    let mut tail = rest[close + 1..].trim().to_string();

    // METADATA { ... } — peel off the trailing block (if any) verbatim.
    let metadata = if let Some(pos) = find_ci(&tail, "METADATA") {
        let after_meta = tail[pos + "METADATA".len()..].trim();
        // after_meta starts with '{' — find the matching '}'.
        let open_b = after_meta
            .find('{')
            .ok_or_else(|| parse_error(format!("METADATA without '{{': {stmt}")))?;
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in after_meta[open_b..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open_b + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.ok_or_else(|| parse_error(format!("unbalanced '{{' in METADATA: {stmt}")))?;
        // The tail is the *inner* JSON (between the braces), preserved
        // verbatim; the renderer re-wraps it as ` METADATA { ... }`.
        let block = Tail::from_text(&after_meta[open_b + 1..end]);
        // Truncate tail so the kind keyword is what remains before METADATA.
        tail = tail[..pos].trim().to_string();
        block
    } else {
        Tail::none()
    };

    // The kind keyword is whatever's left in tail.
    let kind_token = tail
        .split_whitespace()
        .next()
        .ok_or_else(|| parse_error(format!("missing index kind in: {stmt}")))?;
    let kind = IndexKind::from_keyword(kind_token)
        .ok_or_else(|| parse_error(format!("unknown index kind {kind_token:?} in: {stmt}")))?;

    // UNIQUE/NOTUNIQUE carry the uniqueness in the keyword; FULL_TEXT etc. do not.
    let unique = matches!(kind, IndexKind::Unique);

    let entry = schema.type_or_insert(type_name, TypeKind::Document);
    entry.indexes.push(Index {
        name,
        columns,
        kind,
        unique,
        tail: metadata,
    });
    Ok(())
}

/// Take the first whitespace-delimited token from `s`, returning `(token, rest)`.
fn split_first_token(s: &str) -> (&str, &str) {
    let trimmed = s.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    trimmed.split_at(end)
}

/// Strip a leading `IF NOT EXISTS` (case-insensitive) if present.
/// (The "everywhere" variant lives in [`super::ddl`].)
fn strip_if_not_exists(s: &str) -> &str {
    let trimmed = s.trim();
    if ci_starts_with(trimmed, "IF NOT EXISTS") {
        trimmed["IF NOT EXISTS".len()..].trim_start()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_type() {
        let s = parse("CREATE DOCUMENT TYPE account IF NOT EXISTS;").unwrap();
        assert_eq!(s.type_("account").unwrap().kind, TypeKind::Document);
    }

    #[test]
    fn parses_vertex_and_edge_types() {
        let s =
            parse("CREATE VERTEX TYPE node IF NOT EXISTS;\nCREATE EDGE TYPE link IF NOT EXISTS;")
                .unwrap();
        assert_eq!(s.type_("node").unwrap().kind, TypeKind::Vertex);
        assert_eq!(s.type_("link").unwrap().kind, TypeKind::Edge);
    }

    #[test]
    fn type_clause_extends_structured_clause_verbatim() {
        // Real forms from the ArcadeDB docs (sql-types): inheritance and
        // bucket tuning. EXTENDS is parsed structurally; the remaining
        // create-time-only clauses stay verbatim.
        let s = parse(
            "CREATE VERTEX TYPE Employee EXTENDS Person IF NOT EXISTS;
             CREATE DOCUMENT TYPE audit BUCKETS 4 PAGESIZE 4096;
             CREATE EDGE TYPE link UNIDIRECTIONAL LIGHTWEIGHT IF NOT EXISTS;
             create document type cfg custom resource = \"RateLimit\" if not exists;",
        )
        .unwrap();

        let emp = s.type_("Employee").unwrap();
        assert_eq!(emp.extends, vec!["Person".to_string()]);
        assert_eq!(
            emp.clause(),
            None,
            "only EXTENDS remains in this declaration"
        );
        assert_eq!(
            s.type_("audit").unwrap().clause(),
            Some("BUCKETS 4 PAGESIZE 4096")
        );
        assert_eq!(
            s.type_("link").unwrap().clause(),
            Some("UNIDIRECTIONAL LIGHTWEIGHT")
        );
        // Lowercase keywords + quoted CUSTOM value keep their case.
        assert_eq!(
            s.type_("cfg").unwrap().clause(),
            Some("custom resource = \"RateLimit\"")
        );
        assert_eq!(
            emp.kind,
            TypeKind::Vertex,
            "kind must still parse with a trailing clause"
        );
    }

    #[test]
    fn extends_parses_multiple_supers_and_leaves_clause() {
        let s = parse("CREATE VERTEX TYPE A EXTENDS B, C IF NOT EXISTS BUCKETS 4;").unwrap();
        let a = s.type_("A").unwrap();
        assert_eq!(a.extends, vec!["B".to_string(), "C".to_string()]);
        assert_eq!(a.clause(), Some("BUCKETS 4"));
    }

    #[test]
    fn type_without_clause_has_none() {
        let s = parse("CREATE DOCUMENT TYPE page IF NOT EXISTS;").unwrap();
        assert_eq!(s.type_("page").unwrap().clause(), None);
        assert!(s.type_("page").unwrap().extends.is_empty());
    }

    #[test]
    fn parses_create_property() {
        let s = parse(
            "CREATE DOCUMENT TYPE item IF NOT EXISTS;
             CREATE PROPERTY item.external_id IF NOT EXISTS STRING;
             CREATE PROPERTY item.label_ids IF NOT EXISTS LIST OF INTEGER;",
        )
        .unwrap();
        let g = s.type_("item").unwrap();
        assert_eq!(g.properties.get("external_id").unwrap().type_name, "STRING");
        assert_eq!(
            g.properties.get("label_ids").unwrap().type_name,
            "LIST OF INTEGER"
        );
    }

    #[test]
    fn property_attribute_block_parses_into_structured_constraints() {
        // Every constraint kind at once, including a nested-paren default.
        let s = parse(
            "CREATE DOCUMENT TYPE page IF NOT EXISTS;
             CREATE PROPERTY page.slug IF NOT EXISTS STRING (MANDATORY true);
             CREATE PROPERTY page.checksum IF NOT EXISTS STRING (DEFAULT \"\");
             CREATE PROPERTY account.role IF NOT EXISTS STRING (DEFAULT \"user\", MANDATORY true);
             CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date());
             CREATE PROPERTY article.sections IF NOT EXISTS LIST (DEFAULT []);
             CREATE PROPERTY member.score IF NOT EXISTS INTEGER (MIN 1, MAX 200, NOTNULL true, READONLY true, EXTERNAL false, REGEXP \"[0-9]+\");",
        )
        .unwrap();

        let slug = s.type_("page").unwrap().properties.get("slug").unwrap();
        assert_eq!(slug.type_name, "STRING");
        assert!(slug.constraints.mandatory);
        assert!(!slug.constraints.not_null);
        assert!(slug.constraints.default.is_none());

        let checksum_p = s
            .type_("page")
            .unwrap()
            .properties
            .get("checksum")
            .unwrap();
        assert_eq!(checksum_p.type_name, "STRING");
        let d = checksum_p.constraints.default.as_ref().unwrap();
        assert_eq!(
            d.text(),
            "\"\"",
            "DEFAULT \"\" is an empty-string expr, not no-default"
        );

        let role = s.type_("account").unwrap().properties.get("role").unwrap();
        assert!(role.constraints.mandatory);
        assert_eq!(
            role.constraints.default.as_ref().unwrap().text(),
            "\"user\"",
            "value case preserved: \"user\" not \"USER\""
        );

        let synced = s
            .type_("page")
            .unwrap()
            .properties
            .get("synced_at")
            .unwrap();
        // SQL-function default with nested parens survives the comma scan.
        assert_eq!(synced.type_name, "DATETIME");
        assert_eq!(
            synced.constraints.default.as_ref().unwrap().text(),
            "date()"
        );

        let sections = s.type_("article").unwrap().properties.get("sections").unwrap();
        assert_eq!(sections.type_name, "LIST");
        assert_eq!(sections.constraints.default.as_ref().unwrap().text(), "[]");

        // A kitchen-sink block parses every field, quote-stripped.
        let score = s.type_("member").unwrap().properties.get("score").unwrap();
        let c = &score.constraints;
        assert_eq!(c.min.as_deref(), Some("1"));
        assert_eq!(c.max.as_deref(), Some("200"));
        assert_eq!(c.regexp.as_deref(), Some("[0-9]+"), "quotes stripped");
        assert!(c.not_null);
        assert!(c.readonly);
        assert!(!c.external);
        assert!(!c.mandatory);
    }

    #[test]
    fn unknown_attribute_is_a_parse_error() {
        let err = parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING (FANCY true);",
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("unsupported property attribute"),
            "unknown attribute must be a clear parse error, got: {err}"
        );
    }

    #[test]
    fn default_expression_with_commas_and_parens_survives() {
        // A default expression containing nested parens AND commas must not be
        // chopped by the top-level comma scan.
        let s = parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.z IF NOT EXISTS LIST (DEFAULT [\"a\", date(), 3]);",
        )
        .unwrap();
        let def = s.type_("t").unwrap().properties["z"]
            .constraints
            .default
            .as_ref()
            .unwrap();
        assert_eq!(def.text(), "[\"a\", date(), 3]");
    }

    #[test]
    fn parses_create_index_named() {
        let s = parse(
            "CREATE DOCUMENT TYPE tag IF NOT EXISTS;
             CREATE INDEX idx_tag_id IF NOT EXISTS ON tag(id) UNIQUE;",
        )
        .unwrap();
        let i = &s.type_("tag").unwrap().indexes[0];
        assert_eq!(i.name.as_deref(), Some("idx_tag_id"));
        assert_eq!(i.columns, vec!["id"]);
        assert_eq!(i.kind, IndexKind::Unique);
        assert!(i.unique);
    }

    #[test]
    fn parses_create_index_unnamed_fulltext() {
        let s = parse(
            "CREATE DOCUMENT TYPE item IF NOT EXISTS;
             CREATE INDEX ON item(title) FULL_TEXT;",
        )
        .unwrap();
        let i = &s.type_("item").unwrap().indexes[0];
        assert!(i.name.is_none(), "FULL_TEXT index should be unnamed");
        assert_eq!(i.columns, vec!["title"]);
        assert_eq!(i.kind, IndexKind::FullText);
    }

    #[test]
    fn parses_sparse_vector_with_metadata() {
        let s = parse(
            "CREATE DOCUMENT TYPE item IF NOT EXISTS;
             CREATE INDEX idx_item_sparse IF NOT EXISTS ON item(tag_tokens, tag_weights)
                 LSM_SPARSE_VECTOR
                 METADATA { \"dimensions\": 20000, \"modifier\": \"IDF\", \"weightQuantization\": \"FP32\" };",
        )
        .unwrap();
        let i = &s.type_("item").unwrap().indexes[0];
        assert_eq!(i.columns, vec!["tag_tokens", "tag_weights"]);
        assert_eq!(i.kind, IndexKind::LsmSparseVector);
        assert!(i.metadata().unwrap().contains("\"dimensions\": 20000"));
    }

    #[test]
    fn metadata_with_semicolon_does_not_split() {
        // A `;` inside the JSON must not split the statement.
        let s = parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE INDEX i IF NOT EXISTS ON t(x) LSM_SPARSE_VECTOR
                 METADATA { \"a\": 1; \"b\": 2 };",
        )
        .unwrap();
        let i = &s.type_("t").unwrap().indexes[0];
        assert!(i.metadata().unwrap().contains("\"a\": 1; \"b\": 2"));
    }

    #[test]
    fn semicolon_and_comment_markers_inside_strings_are_data() {
        // Regression for the quote-blind splitter: a `;` or `--` inside a
        // quoted DEFAULT is column data, not a statement separator/comment.
        let s = parse(
            r#"CREATE DOCUMENT TYPE t IF NOT EXISTS;
               CREATE PROPERTY t.a IF NOT EXISTS STRING (DEFAULT 'a;b');
               CREATE PROPERTY t.b IF NOT EXISTS STRING (DEFAULT '-- not a comment');"#,
        )
        .unwrap();
        assert_eq!(
            s.type_("t").unwrap().properties["a"]
                .constraints
                .default
                .as_ref()
                .unwrap()
                .text(),
            "'a;b'"
        );
        assert_eq!(
            s.type_("t").unwrap().properties["b"]
                .constraints
                .default
                .as_ref()
                .unwrap()
                .text(),
            "'-- not a comment'"
        );
    }

    #[test]
    fn comments_are_stripped() {
        let s = parse(
            "-- this is a comment
             CREATE DOCUMENT TYPE t IF NOT EXISTS; -- trailing comment
             -- another
             CREATE PROPERTY t.x IF NOT EXISTS STRING;",
        )
        .unwrap();
        assert!(s.type_("t").is_some());
        assert!(s.type_("t").unwrap().properties.contains_key("x"));
    }

    #[test]
    fn unsupported_statement_errors() {
        let err = parse("ALTER TYPE foo CUSTOM x = 1;").unwrap_err();
        // "unsupported statement" lives in the cause chain (the outer context
        // adds the statement number).
        let full = format!("{err:?}");
        assert!(
            full.contains("unsupported statement"),
            "expected 'unsupported statement' in error chain, got: {err}"
        );
    }

    #[test]
    fn keyword_lookalikes_are_rejected_not_misdispatched() {
        // Prefix lookalikes of a real keyword must error as unsupported, not be
        // mis-dispatched into a parser (e.g. `CREATE INDEXLESS ...` used to
        // silently build a bogus type named "LESS ON T").
        for bad in [
            "CREATE PROPERTY_EXTRA foo.bar STRING;",
            "CREATE DOCUMENTARY TYPE d;",
            "CREATE INDEXLESS ON t(x) UNIQUE;",
        ] {
            let err = parse(bad).unwrap_err();
            let full = format!("{err:?}");
            assert!(
                full.contains("unsupported statement"),
                "expected 'unsupported statement' for {bad:?}, got: {err}"
            );
        }
    }

    #[test]
    fn parses_lowercase_keywords() {
        let s = parse(
            "create document type t if not exists;
             create property t.x if not exists string;
             create index on t(x) full_text;",
        )
        .unwrap();
        assert_eq!(s.type_("t").unwrap().kind, TypeKind::Document);
        assert_eq!(
            s.type_("t").unwrap().properties.get("x").unwrap().type_name,
            "STRING"
        );
        assert_eq!(s.type_("t").unwrap().indexes[0].kind, IndexKind::FullText);
    }

    #[test]
    fn parses_full_item_sql_like_real_schema() {
        // A condensed version of item.sql exercising all three forms.
        let ddl = r#"
            CREATE DOCUMENT TYPE item IF NOT EXISTS;
            CREATE PROPERTY item.external_id IF NOT EXISTS STRING;
            CREATE PROPERTY item.tag_tokens IF NOT EXISTS ARRAY_OF_INTEGERS;
            CREATE INDEX idx_item_external_id IF NOT EXISTS ON item(external_id) UNIQUE;
            CREATE INDEX ON item(title) FULL_TEXT;
            CREATE INDEX idx_item_sparse IF NOT EXISTS ON item(tag_tokens, tag_weights)
                LSM_SPARSE_VECTOR METADATA { "dimensions": 20000, "modifier": "IDF" };
        "#;
        let s = parse(ddl).unwrap();
        let g = s.type_("item").unwrap();
        assert_eq!(g.kind, TypeKind::Document);
        assert_eq!(g.properties.len(), 2);
        assert_eq!(g.indexes.len(), 3);
        assert!(g
            .indexes
            .iter()
            .any(|i| i.name.as_deref() == Some("idx_item_external_id") && i.unique));
        assert!(g
            .indexes
            .iter()
            .any(|i| i.kind == IndexKind::FullText && i.name.is_none()));
        assert!(g
            .indexes
            .iter()
            .any(|i| i.kind == IndexKind::LsmSparseVector));
    }

    // --- TimeSeries -------------------------------------------------------

    #[test]
    fn parses_full_timeseries_declaration() {
        let s = parse(
            "CREATE TIMESERIES TYPE SensorReading \
             TIMESTAMP ts PRECISION NANOSECOND \
             TAGS (sensor_id STRING, location STRING) \
             FIELDS (temperature DOUBLE, humidity DOUBLE) \
             SHARDS 8 RETENTION 90 DAYS COMPACTION_INTERVAL 1 HOURS BLOCK_SIZE 65536;",
        )
        .unwrap();
        let ty = s.type_("SensorReading").unwrap();
        assert_eq!(ty.kind, TypeKind::Timeseries);
        let spec = ty.timeseries.as_ref().unwrap();
        assert_eq!(spec.timestamp_column, "ts");
        assert_eq!(spec.precision.as_deref(), Some("NANOSECOND"));
        assert_eq!(spec.tags.len(), 2);
        assert_eq!(spec.tags[0].name, "sensor_id");
        assert_eq!(spec.tags[0].data_type, "STRING");
        assert_eq!(spec.fields.len(), 2);
        assert_eq!(spec.shards, Some(8));
        assert_eq!(spec.retention_ms(), Some(7_776_000_000));
        assert_eq!(spec.compaction_ms(), Some(3_600_000));
        assert_eq!(spec.block_size, Some(65536));
        // Columns are flattened into properties (checksum/DTO-drift visibility).
        for name in ["ts", "sensor_id", "location", "temperature", "humidity"] {
            assert!(ty.properties.contains_key(name), "{name} missing");
        }
        // Canonical re-render matches the docs form.
        assert_eq!(
            spec.render_suffix(),
            "TIMESTAMP ts PRECISION NANOSECOND \
             TAGS (sensor_id STRING, location STRING) \
             FIELDS (temperature DOUBLE, humidity DOUBLE) \
             SHARDS 8 RETENTION 90 DAYS COMPACTION_INTERVAL 1 HOURS BLOCK_SIZE 65536"
        );
    }

    #[test]
    fn parses_minimal_and_if_not_exists_timeseries() {
        // Minimal documented form.
        let s = parse(
            "CREATE TIMESERIES TYPE SensorReading TIMESTAMP ts TAGS (sensor_id STRING) FIELDS (temperature DOUBLE);",
        )
        .unwrap();
        let spec = s
            .type_("SensorReading")
            .unwrap()
            .timeseries
            .as_ref()
            .unwrap();
        assert_eq!(spec.precision, None);
        assert_eq!(spec.retention_ms(), None);

        // The user-command order: IF NOT EXISTS right after TYPE.
        let s = parse(
            "CREATE TIMESERIES TYPE IF NOT EXISTS SensorReading TIMESTAMP ts PRECISION SECOND TAGS (sensor_id LONG) FIELDS (temperature INTEGER) SHARDS 2;",
        )
        .unwrap();
        assert!(s.type_("SensorReading").is_some());

        // Lowercase keywords.
        let s = parse("create timeseries type t timestamp ts fields (f double);").unwrap();
        assert_eq!(
            s.type_("t")
                .unwrap()
                .timeseries
                .as_ref()
                .unwrap()
                .timestamp_column,
            "ts"
        );
    }

    #[test]
    fn timeseries_requires_timestamp_and_fields() {
        let err = parse("CREATE TIMESERIES TYPE t TAGS (a STRING);").unwrap_err();
        assert!(format!("{err:?}").contains("TIMESTAMP"), "{err}");

        let err = parse("CREATE TIMESERIES TYPE t TIMESTAMP ts TAGS (a STRING);").unwrap_err();
        assert!(format!("{err:?}").contains("FIELDS"), "{err}");
    }
}
