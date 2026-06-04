//! Architectural regression guard for the audit invariant.
//!
//! Every in-place mutation or deletion of persisted ledger state must flow
//! through `db::audit`, which records an `audit_log` row in the same savepoint
//! as the change (issue #16). The type system makes the audited functions the
//! only callable mutators, but it cannot stop someone from hand-writing a raw
//! `conn.execute("UPDATE ...")` against the shared `&Connection`. This test
//! closes that last gap: it asserts that the mutating SQL keywords appear
//! nowhere in `src/` except the two sanctioned files -- `db/audit.rs` (the
//! audited mutation API) and `db/schema.rs` (migrations).
//!
//! If this test fails, you added a mutation outside the audited path. Move it
//! into `db/audit.rs` so the change is recorded.

use std::fs;
use std::path::{Path, PathBuf};

/// SQL fragments that perform an in-place mutation or destructive overwrite.
/// Matched case-sensitively so prose like "Update the record" does not trip it.
const FORBIDDEN: [&str; 3] = ["UPDATE ", "DELETE FROM", "INSERT OR REPLACE"];

/// Files permitted to contain mutating SQL.
const ALLOWED: [&str; 2] = ["db/audit.rs", "db/schema.rs"];

#[test]
fn mutating_sql_lives_only_in_the_audit_and_schema_modules() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();

    for file in rust_files(&src) {
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&rel.as_str()) {
            continue;
        }

        let contents = fs::read_to_string(&file).unwrap_or_default();
        for (lineno, line) in contents.lines().enumerate() {
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    violations.push(format!("{rel}:{}: {}", lineno + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "mutating SQL found outside db/audit.rs and db/schema.rs -- \
         move it into the audited mutation path:\n{}",
        violations.join("\n")
    );
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
