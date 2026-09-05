//! Filesystem I/O for the migrator: reading seed/override files from the
//! schema directory, and merging parsed schemas across files.
//!
//! Purely I/O + text plumbing — statement *semantics* (what is destructive,
//! how overrides split into phases) live in [`super::classification`], and
//! statement *splitting* uses the shared quote-aware primitives in
//! [`crate::schema::ddl::parse_util`], so a `;` or `--` inside a string
//! literal behaves identically in schema files, overrides, and seeds.

use std::path::Path;

use crate::error::{MigrateError, Result};

use crate::schema::ddl::{split_sql_statements, strip_comments};
use crate::schema::model::Schema;

/// A seed file: filename + its raw body (seed SQL is run verbatim in one
/// `sqlscript` transaction; statements are `UPDATE ... UPSERT`, so re-runs are
/// idempotent without per-statement tracking).
#[derive(Debug, Clone)]
pub struct SeedFile {
    pub filename: String,
    pub body: String,
}

/// An override migration file: filename + its parsed statements (kept as raw
/// strings — overrides may use any SQL, including ALTER/DROP/backfills).
#[derive(Debug, Clone)]
pub struct OverrideFile {
    pub filename: String,
    pub checksum: String,
    pub statements: Vec<String>,
}

/// Read seed files from a directory, in filename order. The body is wrapped in
/// `BEGIN; ... COMMIT;` so a seed file is atomic.
pub(crate) fn read_seeds(dir: &Path) -> Result<Vec<SeedFile>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("sql") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| MigrateError::Parse {
                message: format!("non-UTF8 seed filename: {}", path.display()),
            })?
            .to_string();
        let text = std::fs::read_to_string(&path).map_err(|e| MigrateError::Io {
            path: path.clone(),
            source: e,
        })?;
        // Comments stripped (quote-aware), wrapped in a transaction.
        let body = strip_comments(&text).trim().to_string();
        let body = format!("BEGIN;\n{body}\nCOMMIT;");
        out.push(SeedFile { filename, body });
    }
    Ok(out)
}

/// Read + parse override files from a directory, in filename order. Statements
/// are split by the shared quote-aware splitter (a `;` inside a string literal
/// never splits); the checksum is over the raw file text.
pub(crate) fn read_overrides(dir: &Path) -> Result<Vec<OverrideFile>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("sql") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| MigrateError::Parse {
                message: format!("non-UTF8 override filename: {}", path.display()),
            })?
            .to_string();
        let text = std::fs::read_to_string(&path).map_err(|e| MigrateError::Io {
            path: path.clone(),
            source: e,
        })?;
        let checksum = sha256_hex(&text);
        let statements = split_sql_statements(&text)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        out.push(OverrideFile {
            filename,
            checksum,
            statements,
        });
    }
    Ok(out)
}

/// Read + parse every `.sql` file in `schema_dir` (non-recursively; the
/// `overrides/` and `seed/` subdirs are handled separately). Files are sorted
/// by name for deterministic merge order.
pub(crate) fn read_schema_dir(schema_dir: &str) -> Result<Schema> {
    let dir = std::fs::read_dir(schema_dir).map_err(|e| MigrateError::Io {
        path: schema_dir.into(),
        source: e,
    })?;
    let mut files: Vec<_> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("sql") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();

    let mut schema = Schema::new();
    for path in files {
        let text = std::fs::read_to_string(&path).map_err(|e| MigrateError::Io {
            path: path.clone(),
            source: e,
        })?;
        let parsed = crate::schema::parser::parse(&text).map_err(|e| MigrateError::Parse {
            message: format!("parsing {}: {e}", path.display()),
        })?;
        merge(&mut schema, parsed);
    }
    Ok(schema)
}

/// Read + verify the overrides for a schema dir: reads `overrides/*.sql`
/// (filename order) and, when an [`overrides.sum`][manifest] manifest is
/// present, verifies the directory against it. The single shared entry point
/// for sync/plan/rollout — so every path gets identical integrity checking.
pub(crate) fn read_overrides_checked(schema_dir: &str) -> Result<Vec<OverrideFile>> {
    let overrides_dir = Path::new(schema_dir).join("overrides");
    if !overrides_dir.is_dir() {
        return Ok(Vec::new());
    }
    let files = read_overrides(&overrides_dir)?;
    super::manifest::verify_manifest(
        &overrides_dir,
        &files
            .iter()
            .map(|o| (o.filename.clone(), o.checksum.clone()))
            .collect::<Vec<_>>(),
    )?;
    Ok(files)
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Compute the pending-override list from the applied registry, enforcing two
/// override-integrity invariants:
///
/// - an override **edited after it was applied** (recorded checksum ≠ file
///   checksum) is a hard error — its statements already changed the DB;
/// - a file whose **content duplicates an already-applied override under a
///   different filename** is a hard error — renaming an applied migration
///   would re-execute it.
///
/// Pure; unit-testable without a DB. `build_plan` calls this with the
/// `schema_overrides_applied` registry.
pub(crate) fn compute_pending(
    applied: &std::collections::BTreeMap<String, crate::schema::revisions::AppliedOverride>,
    overrides: &[OverrideFile],
) -> Result<Vec<OverrideFile>> {
    use std::collections::HashMap;

    // checksum → first filename that recorded it.
    let mut by_checksum: HashMap<&str, &str> = HashMap::with_capacity(applied.len());
    for (name, rec) in applied {
        by_checksum
            .entry(rec.checksum.as_str())
            .or_insert(name.as_str());
    }

    let mut pending = Vec::new();
    for o in overrides {
        match applied.get(&o.filename) {
            Some(recorded) if recorded.checksum != o.checksum => {
                return Err(MigrateError::Integrity {
                    message: format!(
                        "override `{}` was modified after it was applied (recorded checksum {}, \
                         file checksum {}). Applied migrations are immutable — revert the edit, \
                         or create a NEW override file with the fix.",
                        o.filename,
                        short_hash(&recorded.checksum),
                        short_hash(&o.checksum)
                    ),
                });
            }
            Some(_) => {} // applied and unchanged → skip
            None => {
                if let Some(original) = by_checksum.get(o.checksum.as_str()) {
                    return Err(MigrateError::Integrity {
                        message: format!(
                            "override `{}` has identical content to the applied override `{}` — \
                             renaming an applied migration would re-execute it. Restore the \
                             original filename, or write a new migration with different content.",
                            o.filename, original
                        ),
                    });
                }
                pending.push(o.clone());
            }
        }
    }
    Ok(pending)
}

/// 12-char checksum prefix for error messages.
fn short_hash(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// Merge `src` into `dst`. Types, properties, and indexes are additive
/// (a type split across two files merges cleanly); the first occurrence of a
/// type wins for its declaration-level state (`kind`, `extends`, `clause`).
pub(crate) fn merge(dst: &mut Schema, src: Schema) {
    for (name, ty) in src.types {
        use std::collections::btree_map::Entry;
        match dst.types.entry(name) {
            Entry::Vacant(e) => {
                // First declaration: keep the whole type, declaration state
                // (extends / create-time-only clause) included.
                e.insert(ty);
            }
            Entry::Occupied(mut e) => {
                let entry = e.get_mut();
                for (pname, prop) in ty.properties {
                    entry.properties.insert(pname, prop);
                }
                entry.indexes.extend(ty.indexes);
            }
        }
    }
}

/// Draw the `"{type}.{property}" → DEFAULT source text` map from a schema —
/// the desired-expression half of the snapshot (the engine stores only the
/// resolved value, never the source).
pub(crate) fn collect_default_exprs(schema: &Schema) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (tname, ty) in &schema.types {
        for (pname, prop) in &ty.properties {
            if let Some(d) = &prop.constraints.default {
                out.insert(format!("{tname}.{pname}"), d.text().to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn read_overrides_splits_with_quote_awareness() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "001_x.sql",
            "-- header comment\nINSERT INTO t SET x = 'a;b';\nCREATE TYPE t2;",
        );
        let files = read_overrides(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "001_x.sql");
        assert_eq!(
            files[0].statements,
            vec![
                "INSERT INTO t SET x = 'a;b'".to_string(),
                "CREATE TYPE t2".to_string(),
            ]
        );
        // Checksum covers the raw text (comments included): any edit — even a
        // comment edit — changes it, which is what the integrity check keys on.
        assert_eq!(files[0].checksum.len(), 64);
    }

    #[test]
    fn read_seeds_strip_comments_and_wrap_in_tx() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "seed.sql",
            "-- seed users\nUPDATE user SET name = '-- not a comment' UPSERT;",
        );
        let seeds = read_seeds(dir.path()).unwrap();
        assert_eq!(seeds.len(), 1);
        assert!(seeds[0].body.starts_with("BEGIN;\n"));
        assert!(seeds[0].body.ends_with("\nCOMMIT;"));
        assert!(
            seeds[0].body.contains("'-- not a comment'"),
            "quoted -- must survive: {}",
            seeds[0].body
        );
        assert!(!seeds[0].body.contains("-- seed users"));
    }

    #[test]
    fn merge_first_declaration_wins_and_later_props_add() {
        let a = crate::schema::parser::parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS BUCKETS 4;
             CREATE PROPERTY t.x IF NOT EXISTS STRING;",
        )
        .unwrap();
        let b = crate::schema::parser::parse(
            "CREATE VERTEX TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.y IF NOT EXISTS INTEGER;",
        )
        .unwrap();
        let mut dst = Schema::new();
        merge(&mut dst, a);
        merge(&mut dst, b);
        let t = dst.type_("t").unwrap();
        assert_eq!(
            t.kind,
            crate::schema::model::TypeKind::Document,
            "first wins"
        );
        assert_eq!(t.clause(), Some("BUCKETS 4"), "first wins");
        assert!(t.properties.contains_key("x") && t.properties.contains_key("y"));
    }

    #[test]
    fn collect_default_exprs_maps_qualified_names() {
        let s = crate::schema::parser::parse(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.a IF NOT EXISTS STRING (DEFAULT \"u\");
             CREATE PROPERTY t.b IF NOT EXISTS INTEGER;",
        )
        .unwrap();
        let exprs = collect_default_exprs(&s);
        assert_eq!(exprs.get("t.a").map(String::as_str), Some("\"u\""));
        assert!(!exprs.contains_key("t.b"));
    }

    // --- compute_pending (override integrity) ------------------------------

    fn ofile(name: &str, checksum: &str) -> OverrideFile {
        OverrideFile {
            filename: name.into(),
            checksum: checksum.into(),
            statements: vec!["CREATE DOCUMENT TYPE x".into()],
        }
    }

    fn applied_of(
        entries: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, crate::schema::revisions::AppliedOverride> {
        entries
            .iter()
            .map(|(name, cs)| {
                (
                    name.to_string(),
                    crate::schema::revisions::AppliedOverride {
                        checksum: cs.to_string(),
                        statements: vec![],
                    },
                )
            })
            .collect()
    }

    #[test]
    fn compute_pending_passes_through_unapplied_and_skips_applied() {
        let applied = applied_of(&[("001_a.sql", "aa")]);
        let files = [ofile("001_a.sql", "aa"), ofile("002_b.sql", "bb")];
        let pending = compute_pending(&applied, &files).expect("integrity ok");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].filename, "002_b.sql");
    }

    #[test]
    fn compute_pending_rejects_edited_applied_override() {
        let applied = applied_of(&[("001_a.sql", "recorded")]);
        let files = [ofile("001_a.sql", "edited")];
        let err = compute_pending(&applied, &files).unwrap_err().to_string();
        assert!(err.contains("was modified after it was applied"), "{err}");
        assert!(err.contains("001_a.sql"), "{err}");
    }

    #[test]
    fn compute_pending_rejects_renamed_duplicate_of_applied_content() {
        // Same content, new filename: renaming an applied migration must not
        // silently re-execute it.
        let applied = applied_of(&[("001_a.sql", "same")]);
        let files = [ofile("002_renamed.sql", "same")];
        let err = compute_pending(&applied, &files).unwrap_err().to_string();
        assert!(err.contains("identical content"), "{err}");
        assert!(
            err.contains("002_renamed.sql") && err.contains("001_a.sql"),
            "{err}"
        );
    }

    #[test]
    fn compute_pending_allows_identical_content_never_applied() {
        // Two unapplied files with the same content are both pending — the
        // guard only fires against the *applied* registry. (Applying both
        // would then be rejected on the next sync, correctly.)
        let files = [ofile("001_a.sql", "same"), ofile("002_b.sql", "same")];
        let pending = compute_pending(&Default::default(), &files).unwrap();
        assert_eq!(pending.len(), 2);
    }
}
