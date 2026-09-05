//! Directory-integrity manifest for override migrations (`overrides.sum`).
//!
//! The DB registry (`schema_overrides_applied`) is the authority on what a
//! given environment *applied*; this manifest is the authority on what the
//! *directory* contains. The two are complementary:
//!
//! - the registry catches edits/deletions of **applied** migrations (per
//!   environment, needs a live DB);
//! - the manifest catches edits, deletions, additions, and renames of **any**
//!   listed file, offline and reviewable in git (one manifest for all envs).
//!
//! Format: one `h1:<sha256> <filename>` line per override file, then a final
//! `h1:<dirhash>` line where `dirhash` is the SHA-256 over the sorted
//! per-file lines. `#` comments and blank lines are ignored by the parser.
//!
//! Lifecycle: `--write-manifest` (re)generates it after adding/removing
//! override files; every sync/plan **verifies** it when present (absent → no
//! verification, so existing setups keep working until they opt in). Seeds
//! are deliberately not covered — they are re-run every sync and idempotent
//! by contract, so there is no applied-once integrity to protect.

use std::path::{Path, PathBuf};

use crate::error::{MigrateError, Result};

/// The manifest filename inside the overrides directory.
pub(crate) const MANIFEST_NAME: &str = "overrides.sum";

/// Path of the manifest for an overrides directory.
pub(crate) fn manifest_path(overrides_dir: &Path) -> PathBuf {
    overrides_dir.join(MANIFEST_NAME)
}

/// One parsed `h1:<hash> <name>` entry (the dirhash line has no name).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    hash: String,
    name: Option<String>,
}

/// Parse manifest text into entries. The final nameless entry is the
/// directory hash.
fn parse_manifest(text: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(hex) = line.strip_prefix("h1:") else {
            return Err(MigrateError::Parse {
                message: format!(
                    "{MANIFEST_NAME}:{}: expected `h1:<sha256> <filename>`, got {line:?}",
                    i + 1
                ),
            });
        };
        let mut parts = hex.splitn(2, char::is_whitespace);
        let hash = parts.next().unwrap_or_default().to_string();
        let name = parts.next().map(str::to_string);
        entries.push(Entry { hash, name });
    }
    Ok(entries)
}

/// Render the manifest text for `files` (already sorted by filename):
/// per-file lines plus the directory hash.
fn render_entries(files: &[(String, String)]) -> String {
    let mut out = String::from(
        "# arcadedb-migrate overrides manifest\n\
         # h1:<sha256> <filename> per override file; the last line hashes the set.\n\
         # Regenerate with: arcadedb-migrate --schema-dir <dir> --write-manifest\n",
    );
    for (name, hash) in files {
        out.push_str(&format!("h1:{hash} {name}\n"));
    }
    let dir_hash = dir_hash(files);
    out.push_str(&format!("h1:{dir_hash}\n"));
    out
}

/// SHA-256 over the sorted `h1:<hash> <name>` lines — changes when any file
/// is added, removed, edited, or renamed.
fn dir_hash(files: &[(String, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (name, hash) in files {
        h.update(format!("h1:{hash} {name}\n").as_bytes());
    }
    hex::encode(h.finalize())
}

/// SHA-256 a file's bytes.
fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| MigrateError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex::encode(h.finalize()))
}

/// Collect `(filename, sha256)` for every `.sql` file in `overrides_dir`,
/// sorted by filename (same order [`super::fs::read_overrides`] applies them).
fn scan_overrides(overrides_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut files: Vec<_> = std::fs::read_dir(overrides_dir)
        .map_err(|e| MigrateError::Io {
            path: overrides_dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("sql")).then_some(p)
        })
        .collect();
    files.sort();

    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| MigrateError::Parse {
                message: format!("non-UTF8 override filename: {}", path.display()),
            })?
            .to_string();
        out.push((name, file_hash(&path)?));
    }
    Ok(out)
}

/// (Re)generate `overrides.sum` from the current directory contents.
/// Returns the number of listed override files.
pub(crate) fn write_manifest(overrides_dir: &Path) -> Result<usize> {
    let files = scan_overrides(overrides_dir)?;
    let path = manifest_path(overrides_dir);
    std::fs::write(&path, render_entries(&files))
        .map_err(|e| MigrateError::Io { path, source: e })?;
    Ok(files.len())
}

/// Verify the manifest against the freshly-read override files. `files` must
/// be the `(filename, checksum)` list [`super::fs::read_overrides`] computed.
/// No-op (Ok) when no manifest exists — verification is opt-in per directory.
pub(crate) fn verify_manifest(overrides_dir: &Path, files: &[(String, String)]) -> Result<()> {
    let path = manifest_path(overrides_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(()), // no manifest → nothing to verify against
    };

    let entries = parse_manifest(&text).map_err(|e| MigrateError::Parse {
        message: format!("parsing {}: {e}", path.display()),
    })?;
    let dirhash = entries
        .iter()
        .rev()
        .find(|e| e.name.is_none())
        .map(|e| e.hash.clone());
    let listed: std::collections::BTreeMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.name.as_deref().map(|n| (n, e.hash.as_str())))
        .collect();
    let on_disk: std::collections::BTreeMap<&str, &str> = files
        .iter()
        .map(|(n, h)| (n.as_str(), h.as_str()))
        .collect();

    // Every file on disk must be listed with a matching hash…
    for (name, hash) in &on_disk {
        match listed.get(name) {
            None => {
                return Err(MigrateError::Integrity {
                    message: format!(
                        "override `{name}` is not recorded in {} — run \
                         `arcadedb-migrate --write-manifest` after adding override files",
                        path.display()
                    ),
                })
            }
            Some(recorded) if recorded != hash => {
                return Err(MigrateError::Integrity {
                    message: format!(
                    "override `{name}` was edited since {} was written (manifest {}, file {}) — \
                         regenerate the manifest only if this edit is intentional and the override \
                         was never applied",
                    path.display(),
                    short_hash(recorded),
                    short_hash(hash)
                ),
                })
            }
            Some(_) => {}
        }
    }
    // …and every listed file must still exist (applied migrations must not
    // be deleted from the directory).
    for name in listed.keys() {
        if !on_disk.contains_key(name) {
            return Err(MigrateError::Integrity {
                message: format!(
                    "override `{name}` is listed in {} but missing from the directory — \
                     applied migrations must not be deleted",
                    path.display()
                ),
            });
        }
    }
    // Directory hash: catches any set-level anomaly the per-file checks
    // somehow missed (defence in depth).
    if let Some(expected) = dirhash {
        let actual = dir_hash(files);
        if expected != actual {
            return Err(MigrateError::Integrity {
                message: format!(
                    "{} directory hash mismatch (manifest {}, actual {}) — the override set \
                     does not match the manifest",
                    path.display(),
                    short_hash(&expected),
                    short_hash(&actual)
                ),
            });
        }
    }
    Ok(())
}

fn short_hash(s: &str) -> &str {
    &s[..s.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sql(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn write_then_verify_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        write_sql(dir.path(), "001_a.sql", "CREATE DOCUMENT TYPE a;");
        write_sql(dir.path(), "002_b.sql", "CREATE DOCUMENT TYPE b; -- note");

        let n = write_manifest(dir.path()).expect("write");
        assert_eq!(n, 2);

        let files = scan_overrides(dir.path()).expect("scan");
        verify_manifest(dir.path(), &files).expect("fresh manifest verifies");
    }

    #[test]
    fn edited_file_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        write_sql(dir.path(), "001_a.sql", "CREATE DOCUMENT TYPE a;");
        write_manifest(dir.path()).unwrap();

        write_sql(dir.path(), "001_a.sql", "CREATE DOCUMENT TYPE a; -- edited");
        let files = scan_overrides(dir.path()).unwrap();
        let err = verify_manifest(dir.path(), &files).unwrap_err().to_string();
        assert!(err.contains("was edited since"), "{err}");
    }

    #[test]
    fn deleted_file_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        write_sql(dir.path(), "001_a.sql", "A;");
        write_sql(dir.path(), "002_b.sql", "B;");
        write_manifest(dir.path()).unwrap();

        std::fs::remove_file(dir.path().join("002_b.sql")).unwrap();
        let files = scan_overrides(dir.path()).unwrap();
        let err = verify_manifest(dir.path(), &files).unwrap_err().to_string();
        assert!(err.contains("missing from the directory"), "{err}");
    }

    #[test]
    fn added_file_fails_verification_until_manifest_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        write_sql(dir.path(), "001_a.sql", "A;");
        write_manifest(dir.path()).unwrap();

        write_sql(dir.path(), "002_new.sql", "B;");
        let files = scan_overrides(dir.path()).unwrap();
        let err = verify_manifest(dir.path(), &files).unwrap_err().to_string();
        assert!(err.contains("not recorded in"), "{err}");

        // Regenerating fixes it.
        write_manifest(dir.path()).unwrap();
        let files = scan_overrides(dir.path()).unwrap();
        verify_manifest(dir.path(), &files).expect("regenerated manifest verifies");
    }

    #[test]
    fn missing_manifest_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        write_sql(dir.path(), "001_a.sql", "A;");
        let files = scan_overrides(dir.path()).unwrap();
        verify_manifest(dir.path(), &files).expect("no manifest → no verification");
    }

    #[test]
    fn parser_tolerates_comments_and_blank_lines() {
        let entries = parse_manifest("# c\n\nh1:aaa 001.sql\nh1:bbb\n").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name.as_deref(), Some("001.sql"));
        assert_eq!(
            entries[1].name, None,
            "trailing nameless entry is the dirhash"
        );
        assert!(parse_manifest("not-a-manifest-line\n").is_err());
    }

    #[test]
    fn dir_hash_changes_with_the_file_set() {
        let a = vec![("a.sql".to_string(), "h1".to_string())];
        let b = vec![
            ("a.sql".to_string(), "h1".to_string()),
            ("b.sql".to_string(), "h2".to_string()),
        ];
        assert_ne!(dir_hash(&a), dir_hash(&b));
        assert_eq!(dir_hash(&a), dir_hash(&a.clone()));
    }
}
