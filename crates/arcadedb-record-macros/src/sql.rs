//! `sql!` — write the statement as a literal, get a syntax check for free.
//!
//! Wrap any static ArcadeDB statement; the macro validates it at compile
//! time and expands to the `&str` unchanged:
//!
//! ```ignore
//! client.execute_with_values(
//!     sql!("UPDATE users SET status = :status UPSERT WHERE user_id = :user_id"),
//!     params! { status: "active", user_id },
//! ).await?;
//! ```
//!
//! It catches: UPDATE clause order (`SET … UPSERT … RETURN AFTER … WHERE`),
//! unbalanced delimiters, reserved-word placeholders (`:after`), pasted
//! multi-statement scripts, `DELETE` without `WHERE`, `INSERT` without
//! `INTO`, `CREATE EDGE` without `FROM`/`TO`.
//!
//! It does not check bound params (`params!` owns that) or anything needing
//! a live schema. If the statement cannot be a literal — runtime-built
//! fragments, scripts, full-table wipes — don't wrap it; `sql!` is opt-in
//! per statement.

use proc_macro::TokenStream;
use quote::quote_spanned;
use syn::LitStr;

// ---------------------------------------------------------------------------
// Entry points (called from the #[proc_macro] fns in lib.rs)
// ---------------------------------------------------------------------------

/// Expand `sql!("LITERAL")` — validate, then expand to the literal as `&str`.
pub(crate) fn expand_sql(input: TokenStream) -> TokenStream {
    let lit: LitStr = match syn::parse(input) {
        Ok(l) => l,
        Err(e) => return e.to_compile_error().into(),
    };
    let span = lit.span();
    let errors = validate(&lit.value());
    if !errors.is_empty() {
        return syn::Error::new(span, errors.join("\n")).to_compile_error().into();
    }
    quote_spanned!(span => #lit).into()
}

// ---------------------------------------------------------------------------
// Pure validation (unit-testable, no proc-macro types)
// ---------------------------------------------------------------------------

/// All problems found in `sql`; empty means valid.
pub(crate) fn validate(sql: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if sql.trim().is_empty() {
        return vec!["empty SQL statement".to_string()];
    }
    let cleaned = strip_literals_and_comments(sql, &mut errors);
    check_balance(&cleaned, &mut errors);
    check_single_statement(&cleaned, &mut errors);
    let w = words(&cleaned);
    match w.first().map(String::as_str) {
        Some("UPDATE") => check_update_order(&w, &mut errors),
        Some("DELETE") => check_delete(&w, &mut errors),
        Some("INSERT") => check_insert(&w, &mut errors),
        Some("CREATE") if w.get(1).map(String::as_str) == Some("EDGE") => {
            check_create_edge(&w, &mut errors)
        }
        _ => {}
    }
    check_reserved_placeholders(sql, &mut errors);
    errors
}

/// Placeholder names (`:name`) outside literals/comments, deduplicated.
/// Used for the reserved-word check (param binding itself is `params!`'s
/// job, not this macro's).
pub(crate) fn placeholders(sql: &str) -> Vec<String> {
    let mut discard = Vec::new();
    let cleaned = strip_literals_and_comments(sql, &mut discard);
    let mut out = Vec::new();
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let prev_colon = i > 0 && bytes[i - 1] == b':';
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let is_ident_start =
                j > i + 1 && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_');
            if is_ident_start && !prev_colon {
                let name = &cleaned[i + 1..j];
                if !out.iter().any(|n: &String| n == name) {
                    out.push(name.to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

// --- internals ---

/// `sql` with literals/comments blanked (positions preserved) so keyword
/// and delimiter scans never see inside `'strings'` or `-- comments`.
fn strip_literals_and_comments(sql: &str, errors: &mut Vec<String>) -> String {
    #[derive(PartialEq)]
    enum State {
        Normal,
        Single,
        Double,
        /// Backtick-quoted spans (ArcadeDB extension-function names like
        /// `` `vector.sparseNeighbors` ``) — keywords/delimiters inside
        /// them are not statement syntax.
        Backtick,
        LineComment,
        BlockComment,
    }
    let mut out = String::with_capacity(sql.len());
    let mut state = State::Normal;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '\'' => {
                    state = State::Single;
                    out.push(' ');
                }
                '"' => {
                    state = State::Double;
                    out.push(' ');
                }
                '`' => {
                    state = State::Backtick;
                    out.push(' ');
                }
                '-' if chars.peek() == Some(&'-') => {
                    chars.next();
                    state = State::LineComment;
                    out.push_str("  ");
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = State::BlockComment;
                    out.push_str("  ");
                }
                _ => out.push(c),
            },
            State::Single => {
                out.push(' ');
                if c == '\'' {
                    // '' is an escaped quote, not the terminator.
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        out.push(' ');
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::Double => {
                out.push(' ');
                if c == '\\' {
                    // Backslash escape: swallow the next char.
                    if chars.next().is_some() {
                        out.push(' ');
                    }
                } else if c == '"' {
                    state = State::Normal;
                }
            }
            State::Backtick => {
                out.push(if c == '\n' { '\n' } else { ' ' });
                if c == '`' {
                    state = State::Normal;
                }
            }
            State::LineComment => {
                out.push(' ');
                if c == '\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                out.push(if c == '\n' { '\n' } else { ' ' });
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    out.push(' ');
                    state = State::Normal;
                }
            }
        }
    }
    match state {
        State::Single => errors.push("unterminated string literal".to_string()),
        State::Double => errors.push("unterminated quoted identifier".to_string()),
        State::Backtick => errors.push("unterminated backtick span".to_string()),
        State::BlockComment => errors.push("unterminated block comment".to_string()),
        _ => {}
    }
    out
}

fn check_balance(cleaned: &str, errors: &mut Vec<String>) {
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (idx, c) in cleaned.char_indices() {
        match c {
            '(' | '[' | '{' => stack.push((c, idx)),
            ')' | ']' | '}' => {
                let want = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                match stack.pop() {
                    Some((open, _)) if open == want => {}
                    Some((open, _)) => errors.push(format!(
                        "mismatched delimiters: `{open}` closed by `{c}`"
                    )),
                    None => errors.push(format!("unmatched closing `{c}`")),
                }
            }
            _ => {}
        }
    }
    for (open, _) in stack {
        errors.push(format!("unclosed `{open}`"));
    }
}

/// Words (uppercased) outside literals, in order.
fn words(cleaned: &str) -> Vec<String> {
    cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// One statement per `sql!`: an interior `;` means a pasted script —
/// split it or leave it a plain literal. A single trailing `;` is fine.
fn check_single_statement(cleaned: &str, errors: &mut Vec<String>) {
    let trimmed = cleaned.trim_end();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed);
    if body.contains(';') {
        errors.push(
            "multiple statements — pass one statement per sql! (split the script or leave it a plain literal)".to_string(),
        );
    }
}

/// ArcadeDB UPDATE clause order: `UPDATE … SET … [UPSERT] [RETURN
/// BEFORE|AFTER] WHERE …`. Only applies when the statement starts with
/// UPDATE; every other statement gets balance/placeholder checks only.
fn check_update_order(w: &[String], errors: &mut Vec<String>) {
    let pos = |kw: &str| w.iter().position(|x| x == kw);
    let set = pos("SET");
    let upsert = pos("UPSERT");
    let ret = pos("RETURN");
    let wher = pos("WHERE");

    if set.is_none() {
        errors.push("UPDATE without SET — nothing to write".to_string());
        return;
    }
    let set = set.unwrap();

    // `SET` with no body (`UPDATE t SET WHERE …`) — a hand-written empty.
    let next_clause = [upsert, ret, wher]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(w.len());
    if next_clause == set + 1 {
        errors.push("UPDATE with empty SET — no columns to write".to_string());
    }

    if let Some(wh) = wher {
        if wh < set {
            errors.push(
                "WHERE before SET — ArcadeDB order is UPDATE … SET … WHERE …".to_string(),
            );
        }
    }

    if let Some(u) = upsert {
        if u < set {
            errors.push("UPSERT before SET — ArcadeDB order is UPDATE … SET … UPSERT WHERE …".to_string());
        }
        match wher {
            Some(wh) if u < wh => {}
            Some(_) => errors.push(
                "UPSERT after WHERE — ArcadeDB order is UPDATE … SET … UPSERT WHERE …".to_string(),
            ),
            None => errors.push(
                "UPSERT without WHERE — an unconditional UPSERT would upsert every row".to_string(),
            ),
        }
    }

    if let Some(r) = ret {
        let after = w.get(r + 1).map(String::as_str);
        if after != Some("BEFORE") && after != Some("AFTER") {
            errors.push("RETURN must be followed by BEFORE or AFTER".to_string());
        }
        if upsert.is_none() {
            errors.push(
                "RETURN without UPSERT — RETURN BEFORE|AFTER belongs to the UPSERT shape (`… UPSERT RETURN AFTER WHERE …`)".to_string(),
            );
        }
        if r < set {
            errors.push("RETURN before SET — ArcadeDB order is … UPSERT RETURN AFTER WHERE …".to_string());
        }
        if let Some(u) = upsert {
            if r < u {
                errors.push(
                    "RETURN before UPSERT — ArcadeDB order is … UPSERT RETURN AFTER WHERE …".to_string(),
                );
            }
        }
        match wher {
            Some(wh) if r < wh => {}
            Some(_) => errors.push(
                "RETURN AFTER at the end — it sits BETWEEN UPSERT and WHERE (`… UPSERT RETURN AFTER WHERE …`)".to_string(),
            ),
            None => errors.push("RETURN without WHERE — the RETURN clause needs its predicate".to_string()),
        }
    }
}

/// A `DELETE` without `WHERE` wipes the type — almost never what a
/// checked literal means. Full wipes stay plain literals.
fn check_delete(w: &[String], errors: &mut Vec<String>) {
    if !w.iter().any(|x| x == "WHERE") {
        errors.push(
            "DELETE without WHERE — this wipes the whole type; leave it a plain literal if that is really intended"
                .to_string(),
        );
    }
}

/// `INSERT` always names its target (`INSERT INTO …`).
fn check_insert(w: &[String], errors: &mut Vec<String>) {
    if w.get(1).map(String::as_str) != Some("INTO") {
        errors.push("INSERT must be followed by INTO (`INSERT INTO …`)".to_string());
    }
}

/// `CREATE EDGE … FROM … TO …` — a missing endpoint is a syntax error.
/// `CREATE EDGE TYPE …` is DDL (declares the edge type) and is exempt.
fn check_create_edge(w: &[String], errors: &mut Vec<String>) {
    if w.get(2).map(String::as_str) == Some("TYPE") {
        return;
    }    let pos = |kw: &str| w.iter().position(|x| x == kw);
    match (pos("FROM"), pos("TO")) {
        (Some(_), Some(_)) => {}
        (None, _) => errors.push(
            "CREATE EDGE without FROM — edges need both endpoints (`CREATE EDGE … FROM … TO …`)"
                .to_string(),
        ),
        (Some(_), None) => errors.push(
            "CREATE EDGE without TO — edges need both endpoints (`CREATE EDGE … FROM … TO …`)"
                .to_string(),
        ),
    }
}

/// Placeholder names colliding with the statement lexer (`:after` fails
/// to *parse* — AFTER is reserved by `RETURN BEFORE|AFTER`).
///
/// Deliberately narrow: `LIMIT :limit` / `SKIP :skip` demonstrably bind
/// fine (live queries use them), so they are NOT on this list despite
/// being keywords. Only names known or strongly suspected to break the
/// parser belong here — an over-broad list rejects working statements.
fn check_reserved_placeholders(sql: &str, errors: &mut Vec<String>) {
    const RESERVED: &[&str] = &[
        "AFTER", "BEFORE", "WHERE", "UPSERT", "RETURN", "SET", "UPDATE", "SELECT", "FROM", "AS",
        "AND", "OR", "NOT", "NULL", "TRUE", "FALSE", "LIKE", "ILIKE", "IN", "CONTAINS",
        "CONTAINSALL", "CONTAINSANY", "BETWEEN", "IS", "LET", "TRAVERSE",
        "MATCH", "INSERT", "DELETE", "CREATE", "ORDER", "BY", "GROUP", "ASC", "DESC",
    ];
    for ph in placeholders(sql) {
        if RESERVED.contains(&ph.to_ascii_uppercase().as_str()) {
            errors.push(format!(
                "placeholder `:{ph}` collides with a reserved keyword and fails to parse — rename it (e.g. `:{ph}_v`)"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> bool {
        validate(sql).is_empty()
    }

    #[test]
    fn accepts_the_three_update_shapes() {
        assert!(ok("UPDATE users SET status = :status WHERE user_id = :user_id"));
        assert!(ok(
            "UPDATE users SET status = :status, score = :score UPSERT WHERE user_id = :user_id"
        ));
        assert!(ok(
            "UPDATE users SET status = :status UPSERT RETURN AFTER WHERE user_id = :user_id"
        ));
        assert!(ok("SELECT FROM orders WHERE user_id IN :ids"));
        assert!(ok("SELECT * FROM orders WHERE total > 0 ORDER BY total DESC"));
    }

    #[test]
    fn rejects_the_clause_order_traps() {
        let errs = validate("UPDATE users SET status = :s WHERE user_id = :c UPSERT");
        assert!(
            errs.iter().any(|e| e.contains("UPSERT after WHERE")),
            "unexpected: {errs:?}"
        );
        let errs = validate("UPDATE users SET status = :s UPSERT WHERE user_id = :x RETURN AFTER");
        assert!(
            errs.iter().any(|e| e.contains("RETURN AFTER at the end")),
            "unexpected: {errs:?}"
        );
        let errs = validate("UPDATE users SET status = :s UPSERT RETURN AFTER");
        assert!(
            errs.iter().any(|e| e.contains("without WHERE")),
            "unexpected: {errs:?}"
        );
    }

    #[test]
    fn rejects_unbalanced_and_unterminated() {
        assert!(validate("SELECT FROM orders WHERE (user_id = :s").iter().any(|e| e.contains("unclosed")));
        assert!(validate("SELECT 'oops FROM orders").iter().any(|e| e.contains("unterminated")));
        assert!(validate("UPDATE t SET a = :a WHERE b = :b))").iter().any(|e| e.contains("unmatched")));
    }

    #[test]
    fn rejects_return_without_upsert_and_misordered_clauses() {
        // RETURN belongs to the UPSERT shape only.
        let errs = validate("UPDATE t SET a = :a RETURN AFTER WHERE b = :b");
        assert!(
            errs.iter().any(|e| e.contains("RETURN without UPSERT")),
            "unexpected: {errs:?}"
        );
        // WHERE before SET is never valid.
        let errs = validate("UPDATE t WHERE b = :b SET a = :a");
        assert!(
            errs.iter().any(|e| e.contains("WHERE before SET")),
            "unexpected: {errs:?}"
        );
        // Hand-written empty SET.
        let errs = validate("UPDATE t SET WHERE b = :b");
        assert!(
            errs.iter().any(|e| e.contains("empty SET")),
            "unexpected: {errs:?}"
        );
    }

    #[test]
    fn rejects_multi_statement_and_guards_destructive_shapes() {
        // Pasted scripts stay plain literals (trailing `;` is fine).
        assert!(ok("UPDATE t SET a = :a WHERE b = :b;"));
        let errs = validate("UPDATE t SET a = :a WHERE b = :b; DELETE FROM t WHERE b = :b");
        assert!(
            errs.iter().any(|e| e.contains("multiple statements")),
            "unexpected: {errs:?}"
        );
        // Whole-type wipe.
        let errs = validate("DELETE FROM users");
        assert!(
            errs.iter().any(|e| e.contains("DELETE without WHERE")),
            "unexpected: {errs:?}"
        );
        assert!(ok("DELETE FROM users WHERE user_id = :id"));
        // Subquery-carried WHERE still counts.
        assert!(ok(
            "DELETE FROM (SELECT expand(outE('LIKES')) FROM users WHERE user_id = :id)"
        ));
        // INSERT / CREATE EDGE shapes.
        let errs = validate("INSERT users SET a = :a");
        assert!(
            errs.iter().any(|e| e.contains("INTO")),
            "unexpected: {errs:?}"
        );
        assert!(ok("INSERT INTO users SET a = :a"));
        let errs = validate("CREATE EDGE LIKES FROM (SELECT FROM users WHERE user_id = :id)");
        assert!(
            errs.iter().any(|e| e.contains("without TO")),
            "unexpected: {errs:?}"
        );
        assert!(ok(
            "CREATE EDGE LIKES FROM (SELECT FROM users WHERE user_id = :a) TO (SELECT FROM posts WHERE post_id = :b)"
        ));
    }

    #[test]
    fn create_edge_type_ddl_is_exempt() {
        // `CREATE EDGE TYPE …` declares the type — no instance endpoints.
        assert!(ok("CREATE EDGE TYPE links IF NOT EXISTS"));
        assert!(ok("CREATE DOCUMENT TYPE users IF NOT EXISTS"));
        assert!(ok("CREATE VERTEX TYPE node_a IF NOT EXISTS"));
        assert!(ok("CREATE PROPERTY users.user_id IF NOT EXISTS LONG"));
        assert!(ok("CREATE INDEX idx_users_uid IF NOT EXISTS ON users(user_id) UNIQUE"));
    }

    #[test]
    fn backtick_spans_are_invisible() {
        // Extension-function names carry dots and parens around them —
        // keywords/delimiters *inside* the span must not confuse the scan.
        assert!(ok(
            "SELECT *, $score AS score FROM posts WHERE SEARCH_INDEX('posts[title]', :keywords) = true ORDER BY score DESC"
        ));
        let errs = validate("SELECT `oops FROM posts");
        assert!(
            errs.iter().any(|e| e.contains("backtick")),
            "unexpected: {errs:?}"
        );
    }

    #[test]
    fn rejects_reserved_placeholders() {
        let errs = validate("UPDATE t SET a = :a UPSERT RETURN AFTER WHERE b = :after");
        assert!(
            errs.iter().any(|e| e.contains(":after")),
            "unexpected: {errs:?}"
        );
    }

    #[test]
    fn limit_and_skip_placeholders_are_allowed() {
        // Live queries bind these (LIMIT :limit ships in production) —
        // the denylist must stay narrow or it rejects working statements.
        assert!(ok("SELECT a FROM t ORDER BY b ASC LIMIT :limit"));
        assert!(ok("SELECT a FROM t ORDER BY b ASC LIMIT :limit SKIP :skip"));
    }

    #[test]
    fn literals_and_comments_are_invisible() {
        // Keywords and colons inside strings/comments must not confuse
        // the order scan, the balance scan, or placeholder extraction.
        assert!(ok(
            "UPDATE t SET note = 'UPSERT WHERE :fake' WHERE id = :id -- trailing UPSERT ( comment"
        ));
        assert_eq!(placeholders("SELECT '-- :not_a_param' FROM t WHERE a = :a"), vec!["a"]);
    }

    #[test]
    fn extracts_placeholders() {
        assert_eq!(
            placeholders("UPDATE t SET a = :a, b = :b WHERE c = :c AND d = :a"),
            vec!["a", "b", "c"]
        );
        // Casts (`::int`) are not placeholders.
        assert_eq!(placeholders("SELECT count(*)::int FROM t"), Vec::<String>::new());
    }
}
