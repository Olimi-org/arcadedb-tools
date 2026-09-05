use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use crate::error::{MigrateError, Result};
use crate::schema::Plan;

// ---------------------------------------------------------------------------
// Output / confirmation
// ---------------------------------------------------------------------------

pub(super) fn print_plan(plan: &Plan) {
    if plan.no_op {
        println!("schema up to date (checksum {})", short(&plan.checksum));
        return;
    }
    println!(
        "planned changes ({} statement(s); checksum {}):",
        plan.statements.len(),
        short(&plan.checksum)
    );
    // Statements from overrides are tagged with their filename; remaining
    // statements (the diff) get `+`. The ordering in `plan.statements` is
    // overrides-first, so we can walk them in sequence.
    let mut idx = 0;
    for o in &plan.pending_overrides {
        for _ in &o.statements {
            if idx < plan.statements.len() {
                println!("  ~ [{}] {}", o.filename, plan.statements[idx]);
                idx += 1;
            }
        }
        // If the override had no statements (e.g. comment-only), still note it.
        if o.statements.is_empty() {
            println!("  ~ [{}] (no statements — records as applied)", o.filename);
        }
    }
    while idx < plan.statements.len() {
        println!("  + {}", plan.statements[idx]);
        idx += 1;
    }
    for w in &plan.warnings {
        eprintln!("warning: {w}");
    }
}

pub(super) fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// Interactive y/N confirmation for a destructive change. Returns Ok(true) if
/// the user confirmed. In a non-interactive context (no TTY) without --yes,
/// returns an error so CI doesn't silently destroy data.
pub(super) fn confirm_destructive(destructive: &[String]) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(MigrateError::Usage {
            message: format!(
                "destructive changes detected but stdin is not a TTY; pass --yes to apply \
                 non-interactively ({} destructive action(s))",
                destructive.len()
            ),
        });
    }
    eprintln!("⚠ destructive changes — these will modify or drop existing schema objects:");
    for s in destructive {
        eprintln!("  ! {s}");
    }
    eprint!("proceed? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| MigrateError::Io {
            path: PathBuf::from("<stdin>"),
            source: e,
        })?;
    Ok(line.trim().eq_ignore_ascii_case("y"))
}
