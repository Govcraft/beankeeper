//! Integration tests for the `--format` clap argument-id collision.
//!
//! Reproduces GitHub issue #10: the top-level output `--format` option was
//! declared `global = true`, so its clap argument id (`format`) was propagated
//! onto every subcommand. On `txn import` and `export` -- which each define
//! their own `--format` carrying a *different* value enum -- the two
//! definitions clashed. Accessing the subcommand's `format` then panicked with:
//!
//! ```text
//! Mismatch between definition and access of `format`. Could not downcast to
//! beankeeper_cli::cli::OutputFormat, need to downcast to ImportFormat
//! ```
//!
//! The fix gives the subcommand options distinct clap ids and removes
//! `global = true` from the output `--format`. These tests assert that
//! invoking the affected subcommands no longer panics on the downcast.

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

/// The clap panic message that issue #10 reproduces. Any stream containing this
/// indicates the downcast collision was hit.
const DOWNCAST_PANIC: &str = "Mismatch between definition and access";

/// Run a command and return stdout+stderr concatenated.
fn run(args: &[&str], dir: &tempfile::TempDir) -> Result<String, Box<dyn Error>> {
    let output = Command::cargo_bin("bk")?.args(args).current_dir(dir.path()).output()?;
    let mut combined = String::from_utf8(output.stdout)?;
    combined.push_str(&String::from_utf8(output.stderr)?);
    Ok(combined)
}

#[test]
fn txn_import_format_ofx_does_not_panic_on_arg_downcast() -> TestResult {
    let dir = init_demo_db()?;
    std::fs::write(
        dir.path().join("stmt.ofx"),
        "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX></OFX>\n",
    )?;

    let combined = run(
        &[
            "--company",
            "acme-consulting",
            "--db",
            "books.db",
            "txn",
            "import",
            "--file",
            "stmt.ofx",
            "--format",
            "ofx",
            "--account",
            "1000",
            "--suspense",
            "9000",
        ],
        &dir,
    )?;

    // Bug (#10): `--format ofx` on `txn import` panicked because the global
    // OutputFormat `format` id collided with the ImportFormat one. The command
    // may legitimately fail for other reasons, but it must not panic on the
    // clap argument-id downcast.
    assert!(
        !combined.contains(DOWNCAST_PANIC),
        "txn import --format ofx hit the clap arg-id downcast panic: {combined}"
    );
    Ok(())
}

#[test]
fn export_format_does_not_panic_on_arg_downcast() -> TestResult {
    let dir = init_demo_db()?;

    let combined = run(
        &["--db", "books.db", "export", "--format", "json"],
        &dir,
    )?;

    // Bug (#10): `--format json` on `export` panicked because the global
    // OutputFormat `format` id collided with the ExportFormat one.
    assert!(
        !combined.contains(DOWNCAST_PANIC),
        "export --format json hit the clap arg-id downcast panic: {combined}"
    );
    // A clean export emits a JSON envelope; sanity-check we reached real logic.
    assert!(
        combined.contains("beankeeper_export"),
        "export did not produce its JSON envelope: {combined}"
    );
    Ok(())
}
