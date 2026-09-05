//! String-splitting utilities shared between the parser, the migrator's file
//! readers, and the statement classifier: the "tail" grammar unit (see
//! [`super`]) plus the quote-aware SQL text primitives every statement
//! splitter must agree on.
//!
//! The two `strip_comments` / `split_sql_statements` primitives are the
//! *only* sanctioned way to turn raw `.sql` text into statements — schema
//! parser, override reader, seed reader, and rollout extractor all route
//! through them, so a `;` or `--` inside a string literal behaves identically
//! everywhere (they never split/strip inside quotes).

/// Case-insensitive (ASCII-only) `starts_with`, without allocating an
/// uppercased copy of the haystack.
pub(crate) fn ci_starts_with(hay: &str, needle: &str) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Byte index of the first case-insensitive (ASCII-only) occurrence of
/// ASCII `needle`. A match can't straddle a multi-byte char — UTF-8
/// continuation/leading bytes are never ASCII letters — so the returned index
/// is always a char boundary.
pub(crate) fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Byte index of the last case-insensitive (ASCII-only) occurrence of ASCII
/// `needle`. Same char-boundary guarantee as [`find_ci`].
pub(crate) fn rfind_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(hay.len());
    }
    hay.as_bytes()
        .windows(needle.len())
        .rposition(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Remove `--` comments from SQL text, respecting string literals: a `--`
/// inside a single- or double-quoted string (with backslash escapes) is data,
/// not a comment. Newlines are preserved so line-based error reporting keeps
/// pointing at the right line.
pub(crate) fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut state = QuoteState::default();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if state.step(c) {
            out.push(c);
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            // Comment: discard through end of line, keeping the newline so
            // downstream line numbers stay honest.
            chars.next();
            for skipped in chars.by_ref() {
                if skipped == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Split SQL text into statements at top-level `;` — quote-aware ([`QuoteState`])
/// and brace-aware (`;` inside a `METADATA { ... }` JSON block does not split).
/// Comments are stripped first (see [`strip_comments`]).
///
/// Returned chunks are untrimmed but never whitespace-only; callers that need
/// tidy statements trim them.
pub(crate) fn split_sql_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut state = QuoteState::default();
    let mut brace_depth: i32 = 0;
    for c in strip_comments(text).chars() {
        if state.step(c) {
            cur.push(c);
            continue;
        }
        match c {
            '{' => {
                brace_depth += 1;
                cur.push(c);
            }
            '}' => {
                brace_depth -= 1;
                cur.push(c);
            }
            ';' if brace_depth == 0 => {
                let done = std::mem::take(&mut cur);
                // Whitespace-only fragments (empty statements) never surface.
                if !done.trim().is_empty() {
                    out.push(done);
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Shared scanner state for the quote-aware primitives: tracks single- and
/// double-quoted strings with backslash escapes. [`QuoteState::step`] consumes
/// one char and returns `true` when the char is *inside* a literal (the caller
/// must treat it as inert data — no splitting, no comment detection).
#[derive(Default)]
struct QuoteState {
    in_squote: bool,
    in_dquote: bool,
    escaped: bool,
}

impl QuoteState {
    fn step(&mut self, c: char) -> bool {
        if self.in_squote || self.in_dquote {
            if self.escaped {
                self.escaped = false;
            } else if c == '\\' {
                self.escaped = true;
            } else if c == '\'' && self.in_squote {
                self.in_squote = false;
            } else if c == '"' && self.in_dquote {
                self.in_dquote = false;
            }
            return true;
        }
        match c {
            '\'' => {
                self.in_squote = true;
                true
            }
            '"' => {
                self.in_dquote = true;
                true
            }
            _ => false,
        }
    }
}

/// Remove every (case-insensitive) `IF NOT EXISTS` token sequence from `s`.
///
/// wherever it sits — the clause may precede or follow it (`EXTENDS Person
/// IF NOT EXISTS` vs `IF NOT EXISTS EXTENDS Person`), and the renderer always
/// emits it canonically. Tokens are re-joined with single spaces: token
/// *values* (case, quoted content) are preserved, only inter-token whitespace
/// runs are normalized to single spaces.
pub(crate) fn strip_if_not_exists_all(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 3 {
        return tokens.join(" ");
    }
    let mut keep = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if i + 2 < tokens.len()
            && tokens[i].eq_ignore_ascii_case("IF")
            && tokens[i + 1].eq_ignore_ascii_case("NOT")
            && tokens[i + 2].eq_ignore_ascii_case("EXISTS")
        {
            i += 3;
        } else {
            keep.push(tokens[i]);
            i += 1;
        }
    }
    keep.join(" ")
}

/// Split a declaration tail at the first top-level `(` — the attribute block
/// of `CREATE PROPERTY`. Quote-aware (single `'...'` and double `"..."`
/// strings are skipped, with backslash escapes), so a `(` inside a quoted
/// default (`DEFAULT "a(b)"`) or a nested one inside a default expression
/// (`DEFAULT f(g(1))`, `DEFAULT ["a", date()]`) never corrupts the cut.
///
/// Returns `(before_the_paren, from_the_paren_on)` or `None` if there is no
/// unquoted paren.
///
/// Note: the cut is paren-scan only. It is the callers' job to reject
/// statements whose *core* (the property type name) is not a valid ArcadeDB
/// type name before the paren — ArcadeDB property types (`STRING`, `LIST OF
/// INTEGER`, `LINK`) never contain parens or quotes, which is what makes the
/// scan unambiguous in practice.
pub(crate) fn split_at_first_paren(s: &str) -> Option<(&str, &str)> {
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
                '(' => return Some((&s[..i], &s[i..])),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_if_not_exists_all_handles_leading_and_trailing() {
        for (input, expect) in [
            ("EXTENDS Person IF NOT EXISTS", "EXTENDS Person"),
            ("IF NOT EXISTS EXTENDS Person", "EXTENDS Person"),
            ("IF NOT EXISTS", ""),
            ("EXTENDS Person", "EXTENDS Person"),
            ("BUCKETS 4 PAGESIZE 4096", "BUCKETS 4 PAGESIZE 4096"),
            // Case-insensitive, in the middle, repeated.
            (
                "UNIDIRECTIONAL if not exists LIGHTWEIGHT IF NOT EXISTS",
                "UNIDIRECTIONAL LIGHTWEIGHT",
            ),
        ] {
            assert_eq!(strip_if_not_exists_all(input), expect, "input: {input}");
        }
    }

    #[test]
    fn split_at_first_paren_handles_nesting_and_quotes() {
        let (core, tail) = split_at_first_paren("STRING (MANDATORY true)").unwrap();
        assert_eq!(core, "STRING ");
        assert_eq!(tail, "(MANDATORY true)");

        // Nested parens in the default expression stay in the tail.
        let (core, tail) = split_at_first_paren("DATETIME (DEFAULT date())").unwrap();
        assert_eq!(core, "DATETIME ");
        assert_eq!(tail, "(DEFAULT date())");

        // Parens inside (single- and double-quoted) strings are skipped.
        let (_, tail) = split_at_first_paren("STRING (DEFAULT 'a(b)')").unwrap();
        assert_eq!(tail, "(DEFAULT 'a(b)')");
        let (_, tail) = split_at_first_paren("STRING (DEFAULT \"c(d\")").unwrap();
        assert_eq!(tail, "(DEFAULT \"c(d\")");

        // No paren → None.
        assert_eq!(split_at_first_paren("LIST OF STRING"), None);
    }

    #[test]
    fn strip_comments_respects_string_literals() {
        // `--` inside a quoted string is data, not a comment…
        assert_eq!(
            strip_comments("INSERT INTO t SET x = '--not-a-comment'"),
            "INSERT INTO t SET x = '--not-a-comment'"
        );
        assert_eq!(
            strip_comments(r#"SET x = "-- also data --""#),
            r#"SET x = "-- also data --""#
        );
        // …while real comments (before, after, and mid-line) are stripped,
        // keeping newlines for line-number stability.
        assert_eq!(
            strip_comments("-- lead\nCREATE TYPE t; -- trail\n-- tail"),
            "\nCREATE TYPE t; \n"
        );
        // Escaped quote inside a string doesn't terminate it.
        assert_eq!(
            strip_comments(r"SET x = 'it\'s -- fine' -- gone"),
            r"SET x = 'it\'s -- fine' "
        );
    }

    #[test]
    fn split_sql_statements_is_quote_and_brace_aware() {
        // `;` inside a string literal must not split…
        let stmts = split_sql_statements("INSERT INTO t SET x = 'a;b'; CREATE TYPE t2;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a;b'"));
        // …nor inside a METADATA brace block…
        let stmts = split_sql_statements(
            "CREATE INDEX i ON t(x) LSM_SPARSE_VECTOR METADATA { \"a\": 1; \"b\": 2 };\nCREATE TYPE u;",
        );
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("\"a\": 1; \"b\": 2"));
        // …and comments are gone before splitting.
        let stmts = split_sql_statements("-- c1\nCREATE TYPE a; -- c2\nCREATE TYPE b;");
        assert_eq!(stmts.len(), 2);
        // Whitespace-only fragments never surface; separators are consumed.
        assert_eq!(
            split_sql_statements("CREATE TYPE a;   \n  "),
            vec!["CREATE TYPE a"]
        );
        assert!(split_sql_statements("  ;  ; ").is_empty());
    }

    #[test]
    fn find_and_rfind_ci_locate_ascii_needles() {
        assert_eq!(find_ci("INSERT Into t FROM SELECT", "insert into"), Some(0));
        assert_eq!(find_ci("abc", "Z"), None);
        assert_eq!(rfind_ci("select 1 from a from b", " FROM "), Some(15));
        assert_eq!(rfind_ci("no match", " FROM "), None);
    }
}
