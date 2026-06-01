//! Regression test for issue #12.
//!
//! `bk txn post -r/--reference <KEY>` must persist the user-supplied key as the
//! transaction's queryable `reference`, so that `bk txn list --reference <KEY>`
//! can match it. The bug stores an auto-generated `txnref_<hash>` token in the
//! `reference` column instead of the user key.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::tempdir;

/// The human-readable reference key the user posts with.
const USER_KEY: &str = "RENT-2026-03";

/// Run `bk` with the given args against the provided database path and return
/// (success, stdout) — capturing stdout so we can parse JSON.
fn run_bk(db_path: &str, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::cargo_bin("bk")
        .unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.arg("--db").arg(db_path);
    cmd.args(args);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("bk should execute: {e}"));
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("stdout is valid utf-8: {e}"));
    (output.status.success(), stdout)
}

/// Initialize a demo database and post a single transaction tagged with
/// `USER_KEY`, returning the database path (kept alive by the returned dir).
fn setup() -> (tempfile::TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("create tempdir: {e}"));
    let db_path = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("path is utf-8"))
        .to_string();

    let (ok, _) = run_bk(&db_path, &["init", "--demo"]);
    assert!(ok, "init --demo should succeed");

    // acme-consulting has account 5000 (Rent Expense) and 1000 (Operating Cash).
    let (ok, _) = run_bk(
        &db_path,
        &[
            "--company",
            "acme-consulting",
            "txn",
            "post",
            "-d",
            "rent",
            "--debit",
            "5000:2500",
            "--credit",
            "1000:2500",
            "-r",
            USER_KEY,
        ],
    );
    assert!(ok, "txn post with --reference should succeed");

    (dir, db_path)
}

/// The stored/queryable `reference` for a transaction posted with `-r KEY`
/// must equal `KEY`, not an auto-generated `txnref_...` token.
#[test]
fn posted_reference_is_stored_verbatim() {
    let (_dir, db_path) = setup();

    let (ok, stdout) = run_bk(
        &db_path,
        &["--json", "--company", "acme-consulting", "txn", "list"],
    );
    assert!(ok, "txn list --json should succeed");

    // The posted transaction is the only one (demo data adds no acme-consulting
    // txns), so its reference must be exactly USER_KEY.
    let references: Vec<String> = stdout
        .match_indices("\"reference\"")
        .filter_map(|(idx, _)| {
            let rest = &stdout[idx..];
            let colon = rest.find(':')?;
            let after = rest[colon + 1..].trim_start();
            if after.starts_with("null") {
                None
            } else {
                let after = after.strip_prefix('"')?;
                let end = after.find('"')?;
                Some(after[..end].to_string())
            }
        })
        .collect();

    assert!(
        references.iter().any(|r| r == USER_KEY),
        "expected stored reference to be {USER_KEY:?}, but found references {references:?} \
         (the bug stores an auto-generated txnref_... token instead)"
    );
    assert!(
        !references.iter().any(|r| r.starts_with("txnref_")),
        "reference column should not contain an auto-generated txnref_ token; got {references:?}"
    );
}

/// `txn list --reference KEY` must return the transaction posted with `-r KEY`.
#[test]
fn list_filter_matches_posted_reference() {
    let (_dir, db_path) = setup();

    let (ok, stdout) = run_bk(
        &db_path,
        &[
            "--json",
            "--company",
            "acme-consulting",
            "txn",
            "list",
            "--reference",
            USER_KEY,
            "--count",
        ],
    );
    assert!(ok, "txn list --reference --count should succeed");

    // Extract the integer following "count" in the JSON envelope.
    let count = stdout
        .find("\"count\"")
        .and_then(|idx| {
            let rest = &stdout[idx..];
            let colon = rest.find(':')?;
            let after = rest[colon + 1..].trim_start();
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
        .expect("count field present in JSON output");

    assert_eq!(
        count, 1,
        "txn list --reference {USER_KEY:?} should match the posted transaction, \
         but matched {count} (the user key is unfindable because a txnref_ token was stored)"
    );
}
