//! Integration tests for `bk txn import` format handling.
//!
//! Reproduces GitHub issue #11: `bk txn import` advertises OFX, CSV, and JSON
//! as first-class `--format` choices, but CSV and JSON are unimplemented stubs
//! that return a generic `GENERAL` "not yet implemented" runtime error.
//!
//! These tests assert the honest, documented behavior the issue calls for:
//! invoking an advertised format must NOT surface a generic `GENERAL`
//! "... import not yet implemented" error.

use std::error::Error;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn Error>>;

/// Convert a path to a UTF-8 `&str`, failing the test cleanly when it is not.
fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| Box::<dyn Error>::from(format!("non-utf8 path: {}", path.display())))
}

/// Initialize a demo database (which creates the `acme-consulting` company with
/// account code `1000`) inside a throwaway directory, returning that directory.
fn init_demo_db() -> Result<tempfile::TempDir, Box<dyn Error>> {
    let dir = tempdir()?;
    let db_path = dir.path().join("books.db");

    Command::cargo_bin("bk")?
        .args(["init", "--demo", "--path", path_str(&db_path)?])
        .assert()
        .success();

    Ok(dir)
}

/// Run `bk --json --company acme-consulting txn import` for the given file and
/// return the captured stdout and stderr concatenated as a string.
fn run_import(dir: &tempfile::TempDir, file_name: &str) -> Result<String, Box<dyn Error>> {
    let db_path = dir.path().join("books.db");
    let file_path = dir.path().join(file_name);

    let output = Command::cargo_bin("bk")?
        .args([
            "--json",
            "--company",
            "acme-consulting",
            "--db",
            path_str(&db_path)?,
            "txn",
            "import",
            "--file",
            path_str(&file_path)?,
            "--account",
            "1000",
            "--suspense",
            "9000",
        ])
        .output()?;

    // The JSON error envelope is emitted on stderr; success JSON on stdout.
    // Capture both so the assertions see whichever stream the CLI uses.
    let mut combined = String::from_utf8(output.stdout)?;
    combined.push_str(&String::from_utf8(output.stderr)?);
    Ok(combined)
}

#[test]
fn csv_import_is_not_a_general_not_implemented_stub() -> TestResult {
    let dir = init_demo_db()?;
    std::fs::write(
        dir.path().join("stmt.csv"),
        "date,amount,description\n2026-01-01,10.00,test\n",
    )?;

    let stdout = run_import(&dir, "stmt.csv")?;

    // Bug (#11): CSV is advertised as a supported `--format`, yet importing a
    // `.csv` file returns `{"error":{"code":"GENERAL","message":"Csv import
    // not yet implemented"}}`. The advertised format must not dead-end in a
    // generic GENERAL stub error.
    assert!(
        !stdout.contains("not yet implemented"),
        "CSV import returned a 'not yet implemented' stub error: {stdout}"
    );
    assert!(
        !stdout.contains("\"code\": \"GENERAL\""),
        "CSV import surfaced a generic GENERAL error code: {stdout}"
    );
    Ok(())
}

#[test]
fn json_import_is_not_a_general_not_implemented_stub() -> TestResult {
    let dir = init_demo_db()?;
    std::fs::write(
        dir.path().join("stmt.json"),
        "[{\"date\":\"2026-01-01\",\"amount\":1000,\"description\":\"test\"}]\n",
    )?;

    let stdout = run_import(&dir, "stmt.json")?;

    // Bug (#11): JSON is advertised as a supported `--format`, yet importing a
    // `.json` file returns `{"error":{"code":"GENERAL","message":"Json import
    // not yet implemented"}}`. The advertised format must not dead-end in a
    // generic GENERAL stub error.
    assert!(
        !stdout.contains("not yet implemented"),
        "JSON import returned a 'not yet implemented' stub error: {stdout}"
    );
    assert!(
        !stdout.contains("\"code\": \"GENERAL\""),
        "JSON import surfaced a generic GENERAL error code: {stdout}"
    );
    Ok(())
}
