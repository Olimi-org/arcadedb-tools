//! Override-statement classification — the **single** authority on what a raw
//! SQL statement means to the migrator.
//!
//! Everything routes through [`classify_statement`]:
//!
//! - the apply path ([`super::Migrator::sync`]) splits each override into
//!   copy/swap phases via [`split_override_phases`];
//! - the confirmation gates (`Plan::destructive_statements`, the rollout
//!   gate) use [`is_destructive_statement`];
//! - the copy verifier derives its `(source, target)` pairs via
//!   [`copy_verification_pairs`];
//! - plan time rejects overrides whose authored order the phase split would
//!   silently reorder ([`validate_phase_ordering`]).
//!
//! The typed diff engine keeps its own [`crate::schema::diff::DiffAction::
//! is_destructive`] — that one classifies *structured actions* the migrator
//! itself generated, not operator-authored SQL, so the two predicates answer
//! different questions and are documented separately.

use crate::error::{MigrateError, Result};

use crate::schema::ddl::{find_ci, rfind_ci};

/// How the apply path executes a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementClass {
    /// Additive/reversible: runs inside the explicit copy transaction
    /// (`CREATE *`, `INSERT … FROM SELECT`, `MOVE VERTEX`, constraint
    /// tweaks, `CREATE INDEX`, …).
    Copy,
    /// Destructive or irreversible: runs individually, auto-committed by the
    /// engine (`DROP *`, `DELETE FROM`, `UPDATE`, `ALTER TYPE … NAME`,
    /// `ALTER PROPERTY … TYPE`). Never runs before the copy phase has been
    /// verified.
    Swap,
}

/// Classify one statement. Keywords are matched case-insensitively on the
/// trimmed leading tokens.
pub fn classify_statement(stmt: &str) -> StatementClass {
    let t = stmt.trim_start().to_ascii_uppercase();
    if t.starts_with("DROP ") || t.starts_with("DELETE ") || t.starts_with("UPDATE ") {
        return StatementClass::Swap;
    }
    if t.starts_with("ALTER TYPE ") && t.contains(" NAME ") {
        return StatementClass::Swap;
    }
    if t.starts_with("ALTER PROPERTY ") && t.contains(" TYPE ") {
        return StatementClass::Swap;
    }
    StatementClass::Copy
}

/// Whether a statement mutates or removes existing schema/data — the predicate
/// behind every destructive-confirmation gate. Deliberately identical to
/// [`classify_statement`] `== Swap`: one definition, so what the operator
/// confirms is exactly what the phase engine treats as point-of-no-return.
pub fn is_destructive_statement(stmt: &str) -> bool {
    classify_statement(stmt) == StatementClass::Swap
}

/// Split an override's statements into a `(copy, swap)` pair, each preserving
/// the original relative order. The copy phase runs inside an explicit
/// transaction and must be verified before the swap phase executes.
///
/// NOTE: this reorders execution whenever the authored file interleaves the
/// two classes — which is why [`validate_phase_ordering`] rejects such files
/// at plan time instead of letting the reorder happen silently.
pub fn split_override_phases(statements: &[String]) -> (Vec<String>, Vec<String>) {
    let mut copy = Vec::new();
    let mut swap = Vec::new();
    for s in statements {
        if is_destructive_statement(s) {
            swap.push(s.clone());
        } else {
            copy.push(s.clone());
        }
    }
    (copy, swap)
}

/// Reject overrides whose authored order the copy/swap split would reorder:
/// any additive statement *after* a destructive one would jump ahead of it at
/// apply time (all copies run first), silently changing semantics — e.g. an
/// `INSERT … FROM SELECT` that snapshots a table would run before the `UPDATE`
/// the author wrote first.
///
/// Called at plan time (so `--dry-run` catches it before anything runs).
pub fn validate_phase_ordering(filename: &str, statements: &[String]) -> Result<()> {
    let mut first_swap: Option<(usize, &String)> = None;
    for (i, s) in statements.iter().enumerate() {
        match classify_statement(s) {
            StatementClass::Swap => {
                first_swap.get_or_insert((i, s));
            }
            StatementClass::Copy if first_swap.is_some() => {
                let (j, swap_stmt) = first_swap.expect("checked above");
                return Err(MigrateError::Parse {
                    message: format!(
                        "override {filename}: statement #{} (`{s}`) is additive but follows the \
                         destructive statement #{} (`{swap_stmt}`) — the copy/swap phase split \
                         would execute it BEFORE that statement, silently reordering the migration. \
                         Rewrite the file so additive statements come first, or split it into two \
                         override files.",
                        i + 1,
                        j + 1
                    ),
                });
            }
            StatementClass::Copy => {}
        }
    }
    Ok(())
}

/// Derive `(source, target, is_move)` copy pairs from the copy-phase
/// statements, for post-copy verification.
///
/// Recognized patterns (the sanctioned type-rebuild recipe):
/// - `INSERT INTO <target> FROM SELECT [* | <cols>] FROM <source>` — the source
///   stays intact, so `count(target) == count(source)` after the copy. A
///   `WHERE` clause still derives a pair, so a filter that silently drops rows
///   fails verification.
/// - `MOVE VERTEX <source> TO TYPE:<target> [SET|REMOVE …] [BATCH n]` (per the
///   SQL reference) — the source type is drained, so the *pre-copy* source
///   count must equal the target count.
///
/// Deliberately NOT recognized (no pair → no automated count check):
/// - `MOVE VERTEX … TO BUCKET:<bucket>` — a bucket move keeps type membership
///   unchanged, so there is no (source, target) type invariant to verify.
/// - `MOVE VERTEX #4:1 TO TYPE:t` (RID / RID-array sources) — moving
///   individual records is not a bulk copy; `ident_start` rejects the leading
///   `#`/`[` and no pair is derived.
///
/// Pattern keywords are matched case-insensitively, but the extracted type
/// names keep their **authored case**: ArcadeDB stores type names verbatim
/// and resolves them case-sensitively, so a query against an
/// uppercased name fails for a lowercase-authored type.
///
/// Statements that match neither pattern contribute no pair (they run inside
/// the copy tx without an automated count check).
///
/// Note on `UPDATE … UPSERT`: the SQL reference sanctions it as the only
/// idempotent create form ("CREATE VERTEX does not have an UPSERT clause"),
/// backed by a UNIQUE index for atomicity. It classifies as Swap (in-place
/// mutation), which is correct for overrides; seeds bypass classification
/// entirely (re-run verbatim, idempotent by contract).
pub fn copy_verification_pairs(statements: &[String]) -> Vec<(String, String, bool)> {
    let mut pairs = Vec::new();
    for s in statements {
        let t = s.trim_start();
        let u = t.to_ascii_uppercase();
        if u.starts_with("INSERT INTO ") {
            let rest = &t["INSERT INTO ".len()..];
            let Some(target) = ident_start(rest) else {
                continue;
            };
            // Everything after the LAST ` FROM ` is the select's table.
            let Some(fi) = rfind_ci(t, " FROM ") else {
                continue;
            };
            let Some(source) = ident_start(&t[fi + " FROM ".len()..]) else {
                continue;
            };
            pairs.push((source, target, false));
        } else if u.starts_with("MOVE VERTEX ") {
            let Some(ti) = find_ci(t, " TO TYPE:") else {
                continue;
            };
            let Some(target) = ident_start(&t[ti + " TO TYPE:".len()..]) else {
                continue;
            };
            let Some(fi) = find_ci(t, " FROM ") else {
                continue;
            };
            let Some(source) = ident_start(&t[fi + " FROM ".len()..]) else {
                continue;
            };
            pairs.push((source, target, true));
        }
    }
    pairs
}

/// The leading identifier of `text`, or `None` if the text doesn't start with
/// an identifier character.
fn ident_start(text: &str) -> Option<String> {
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    let ident = &text[..end];
    if ident.is_empty() {
        None
    } else {
        Some(ident.to_string())
    }
}

/// The type name a statement **creates**, if it creates a type at all — used
/// to derive which schema objects are override-*owned* (so a destructive sync
/// doesn't drop them as drift).
///
/// Matches `CREATE [DOCUMENT|VERTEX|EDGE|TIMESERIES] TYPE <name>` only:
/// record-DML forms like `CREATE VERTEX <type>` (third token is not `TYPE`)
/// and property/index creates never match. Returns the authored-case name
/// (engine resolves names case-sensitively).
pub fn created_type_name(stmt: &str) -> Option<String> {
    let mut w = stmt.split_whitespace();
    if !w.next()?.eq_ignore_ascii_case("CREATE") {
        return None;
    }
    // Second token is optional kind or TYPE.
    let second = w.next()?;
    let third = w.next()?;
    let name_token = if matches!(
        second.to_ascii_uppercase().as_str(),
        "DOCUMENT" | "VERTEX" | "EDGE" | "TIMESERIES"
    ) && third.eq_ignore_ascii_case("TYPE")
    {
        skip_if_not_exists(w.next()?, &mut w)
    } else if second.eq_ignore_ascii_case("TYPE") {
        skip_if_not_exists(third, &mut w)
    } else {
        return None;
    };
    Some(name_token.trim_end_matches(';').to_string())
}

/// `IF NOT EXISTS` may precede the type name (`CREATE TIMESERIES TYPE
/// IF NOT EXISTS <name>`); skip all three keywords when present.
fn skip_if_not_exists<'a, I: Iterator<Item = &'a str>>(token: &'a str, w: &mut I) -> &'a str {
    if token.eq_ignore_ascii_case("IF") {
        let _ = w.next(); // NOT
        let _ = w.next(); // EXISTS
        w.next().unwrap_or(token) // the actual name
    } else {
        token
    }
}

/// The type name a statement drops (`DROP [TIMESERIES] TYPE <name> …`), for
/// ownership netting: an override that creates then drops its own scratch
/// type doesn't own anything afterwards. `IF EXISTS` may sit before the name
/// on TimeSeries drops (`DROP TIMESERIES TYPE IF EXISTS <name>` — docs form).
pub fn dropped_type_name(stmt: &str) -> Option<String> {
    let mut w = stmt.split_whitespace();
    if !w.next()?.eq_ignore_ascii_case("DROP") {
        return None;
    }
    // Optional TIMESERIES keyword between DROP and TYPE.
    let mut token = w.next()?;
    if token.eq_ignore_ascii_case("TIMESERIES") {
        token = w.next()?;
    }
    if !token.eq_ignore_ascii_case("TYPE") {
        return None;
    }
    let mut name = w.next()?;
    if name.eq_ignore_ascii_case("IF") {
        let _ = w.next()?; // EXISTS
        name = w.next()?;
    }
    Some(name.trim_end_matches(';').to_string())
}

/// Net schema objects owned by a set of overrides' statements: types created
/// minus types disposed of, per file in statement order, unioned across files.
/// A type is disposed of by `DROP TYPE` **or** by `ALTER TYPE … NAME` (a
/// rename moves ownership to the NEW name — the rebuild recipe
/// `CREATE t_lng … ALTER TYPE t_lng NAME t` therefore owns nothing net-new,
/// which is correct since the final name is declared in the desired schema;
/// a helper/scratch type an override leaves behind IS owned).
pub fn owned_types<'a>(
    statements: impl Iterator<Item = &'a [String]>,
) -> std::collections::BTreeSet<String> {
    let mut owned = std::collections::BTreeSet::new();
    for list in statements {
        for s in list {
            if let Some(name) = created_type_name(s) {
                owned.insert(name);
            }
            if let Some(name) = dropped_type_name(s) {
                owned.remove(&name);
            }
            if let Some((old, new)) = renamed_type_name(s) {
                owned.remove(&old);
                owned.insert(new);
            }
        }
    }
    owned
}

/// The `(old, new)` names of an `ALTER TYPE <old> NAME <new>` rename.
fn renamed_type_name(stmt: &str) -> Option<(String, String)> {
    let mut w = stmt.split_whitespace();
    if !w.next()?.eq_ignore_ascii_case("ALTER") || !w.next()?.eq_ignore_ascii_case("TYPE") {
        return None;
    }
    let old = w.next()?.to_string();
    if !w.next()?.eq_ignore_ascii_case("NAME") {
        return None;
    }
    let new = w.next()?.trim_end_matches(';').to_string();
    Some((old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_verification_pairs_insert() {
        let stmts = vec![
            "INSERT INTO readings_lng FROM SELECT * FROM readings".to_string(),
            "CREATE DOCUMENT TYPE readings_lng IF NOT EXISTS".to_string(),
        ];
        let pairs = copy_verification_pairs(&stmts);
        assert_eq!(
            pairs,
            vec![(
                "readings".to_string(),
                "readings_lng".to_string(),
                false
            )]
        );
    }

    #[test]
    fn copy_verification_pairs_move() {
        let stmts = vec![
            "MOVE VERTEX (SELECT FROM item) TO TYPE:item_lng SET external_id = convert.toInteger(external_id) BATCH -1"
                .to_string(),
        ];
        let pairs = copy_verification_pairs(&stmts);
        assert_eq!(
            pairs,
            vec![("item".to_string(), "item_lng".to_string(), true)]
        );
    }

    #[test]
    fn copy_verification_pairs_filtered_select_still_derives() {
        let stmts =
            vec!["INSERT INTO t_lng FROM SELECT * FROM t WHERE external_id IS NOT NULL".to_string()];
        let pairs = copy_verification_pairs(&stmts);
        assert_eq!(pairs, vec![("t".to_string(), "t_lng".to_string(), false)]);
    }

    #[test]
    fn copy_verification_pairs_uppercased_statements_keep_authored_case() {
        // Pattern keywords in any case; the extracted names keep authored case
        // (ArcadeDB resolves type names case-sensitively).
        let stmts = vec![
            "insert into readings_lng from select * from readings".to_string(),
            "Move Vertex (select from item) to type:item_lng".to_string(),
        ];
        let pairs = copy_verification_pairs(&stmts);
        assert_eq!(
            pairs,
            vec![
                (
                    "readings".to_string(),
                    "readings_lng".to_string(),
                    false
                ),
                ("item".to_string(), "item_lng".to_string(), true),
            ]
        );
    }

    #[test]
    fn classify_detects_destructive_statements() {
        assert!(is_destructive_statement("DROP TYPE readings IF EXISTS"));
        assert!(is_destructive_statement("  DROP TYPE item IF EXISTS"));
        assert!(is_destructive_statement("DELETE FROM legacy"));
        assert!(is_destructive_statement("UPDATE user_shelf REMOVE title"));
        assert!(is_destructive_statement("ALTER TYPE item_lng NAME item"));
        assert!(is_destructive_statement(
            "ALTER PROPERTY item.external_id TYPE LONG"
        ));
        assert!(!is_destructive_statement(
            "CREATE VERTEX TYPE item_lng IF NOT EXISTS"
        ));
        assert!(!is_destructive_statement(
            "INSERT INTO readings_lng FROM SELECT * FROM readings"
        ));
        assert!(!is_destructive_statement(
            "MOVE VERTEX (SELECT FROM item) TO TYPE:item_lng"
        ));
        assert!(!is_destructive_statement(
            "ALTER PROPERTY item.title mandatory true"
        ));
    }

    #[test]
    fn destructive_gate_covers_the_swap_set() {
        // The confirmation gate and the phase classifier must agree.
        for s in [
            "DROP TYPE t",
            "DELETE FROM t",
            "UPDATE t SET x = 1",
            "ALTER TYPE a NAME b",
            "ALTER PROPERTY t.x TYPE LONG",
        ] {
            assert!(is_destructive_statement(s), "{s} must gate");
            assert_eq!(classify_statement(s), StatementClass::Swap);
        }
    }

    #[test]
    fn split_phases_preserve_relative_order() {
        let stmts = vec![
            "CREATE VERTEX TYPE item_lng IF NOT EXISTS".to_string(),
            "MOVE VERTEX (SELECT FROM item) TO TYPE:item_lng".to_string(),
            "DROP TYPE item IF EXISTS".to_string(),
            "ALTER TYPE item_lng NAME item".to_string(),
            "CREATE DOCUMENT TYPE readings_lng IF NOT EXISTS".to_string(),
            "INSERT INTO readings_lng FROM SELECT * FROM readings".to_string(),
            "DROP TYPE readings IF EXISTS".to_string(),
        ];
        let (copy, swap) = split_override_phases(&stmts);
        assert_eq!(
            copy,
            vec![
                "CREATE VERTEX TYPE item_lng IF NOT EXISTS",
                "MOVE VERTEX (SELECT FROM item) TO TYPE:item_lng",
                "CREATE DOCUMENT TYPE readings_lng IF NOT EXISTS",
                "INSERT INTO readings_lng FROM SELECT * FROM readings",
            ]
        );
        assert_eq!(
            swap,
            vec![
                "DROP TYPE item IF EXISTS",
                "ALTER TYPE item_lng NAME item",
                "DROP TYPE readings IF EXISTS",
            ]
        );
    }

    #[test]
    fn ownership_extractors_cover_timeseries_forms() {
        // TimeSeries types are created/dropped with their own keyword.
        assert_eq!(
            created_type_name(
                "CREATE TIMESERIES TYPE readings_ts TIMESTAMP ts FIELDS (x INTEGER)"
            ),
            Some("readings_ts".to_string())
        );
        // The docs/user-command form puts IF NOT EXISTS before the name.
        assert_eq!(
            created_type_name(
                "CREATE TIMESERIES TYPE IF NOT EXISTS readings_ts TIMESTAMP ts FIELDS (x INTEGER)"
            ),
            Some("readings_ts".to_string())
        );
        assert_eq!(
            created_type_name("create timeseries type if_not_needed"),
            Some("if_not_needed".to_string()),
            "authored case kept"
        );
        // Record-DML on a TS type must NOT count as a type create.
        assert_eq!(created_type_name("CREATE VERTEX t SET x = 1"), None);
        assert_eq!(
            created_type_name("INSERT INTO readings_ts SET x = 1"),
            None
        );
        assert_eq!(
            dropped_type_name("DROP TIMESERIES TYPE IF EXISTS readings_ts"),
            Some("readings_ts".to_string()),
            "IF EXISTS precedes the name on TS drops"
        );
        assert_eq!(
            dropped_type_name("DROP TYPE plain IF EXISTS"),
            Some("plain".to_string())
        );
        assert_eq!(dropped_type_name("DROP INDEX idx_x IF EXISTS"), None);

        // Netting: create → rename-away leaves nothing owned under old name.
        let stmts: Vec<String> = [
            "CREATE TIMESERIES TYPE metrics_tmp",
            "ALTER TYPE metrics_tmp NAME metrics_final",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let owned = owned_types([stmts.as_slice()].into_iter());
        assert!(!owned.contains("metrics_tmp"), "{owned:?}");
    }

    #[test]
    fn phase_ordering_rejects_interleaved_files() {
        // Copy after swap would be reordered by the phase split — reject.
        let bad = vec![
            "UPDATE stats SET migrated = true".to_string(),
            "INSERT INTO audit FROM SELECT * FROM stats".to_string(),
        ];
        let err = validate_phase_ordering("005_audit.sql", &bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("silently reordering"), "{err}");
        assert!(err.contains("#2"), "{err}");
        assert!(err.contains("#1"), "{err}");

        // All-copies-then-all-swaps is the sanctioned shape.
        let good = vec![
            "CREATE DOCUMENT TYPE t_lng IF NOT EXISTS".to_string(),
            "INSERT INTO t_lng FROM SELECT * FROM t".to_string(),
            "DROP TYPE t IF EXISTS".to_string(),
            "ALTER TYPE t_lng NAME t".to_string(),
        ];
        validate_phase_ordering("006_rebuild.sql", &good).unwrap();

        // Pure-swap and pure-copy files are trivially fine.
        validate_phase_ordering("x.sql", &["DELETE FROM t".to_string()]).unwrap();
        validate_phase_ordering("x.sql", &["CREATE TYPE a".to_string()]).unwrap();
        validate_phase_ordering("x.sql", &[]).unwrap();
    }
}
