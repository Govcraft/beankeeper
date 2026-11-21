//! Normal commands must never turn a mistyped path into a new ledger.

use std::error::Error;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::prelude::*;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn Error>>;

fn bk(path: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command
        .arg("--db")
        .arg(path)
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .stdin(Stdio::null());
    Ok(command)
}

#[test]
fn missing_database_is_not_found_for_all_command_families_without_creating_files() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("typo.db");
    let commands: &[&[&str]] = &[
        &["company", "list"],
        &["company", "create", "acme", "Acme"],
        &["--company", "acme", "account", "list"],
        &["--company", "acme", "txn", "list"],
        &["--company", "acme", "budget", "list", "--year", "2026"],
        &["--company", "acme", "report", "trial-balance"],
        &["export"],
        &["verify"],
        &["audit", "log"],
        &["txn", "reconcile"],
    ];
    for args in commands {
        let output = bk(&path)?
            .arg("--json")
            .args(*args)
            .assert()
            .code(5)
            .get_output()
            .clone();
        assert_eq!(output.stdout, Vec::<u8>::new());
        let envelope: serde_json::Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(envelope["error"]["code"], "NOT_FOUND");
        let message = envelope["error"]["message"]
            .as_str()
            .ok_or("missing error message")?;
        assert!(message.contains("typo.db"));
        assert!(message.contains("bk init --path"));
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
    }
    Ok(())
}

#[test]
fn missing_environment_and_default_database_paths_are_not_created() -> TestResult {
    let dir = tempdir()?;
    for configured in [false, true] {
        let mut command = Command::cargo_bin("bk")?;
        command
            .current_dir(dir.path())
            .env_remove("BEANKEEPER_DB")
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .args(["company", "list"]);
        if configured {
            command.env("BEANKEEPER_DB", dir.path().join("env-typo.db"));
        }
        command.assert().code(5);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
    }
    Ok(())
}

#[test]
fn init_is_the_explicit_creation_path_and_existing_databases_reopen() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    assert!(path.exists());
    bk(&path)?
        .args(["company", "create", "acme", "Acme"])
        .assert()
        .success();
    bk(&path)?
        .args(["--company", "acme", "txn", "list"])
        .assert()
        .success();
    Ok(())
}

#[test]
fn existing_databases_still_receive_schema_migrations() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("legacy.db");
    // Existing SQLite databases at schema version zero remain eligible for
    // automatic migrations; only creation of a missing file is prohibited.
    drop(rusqlite::Connection::open(&path)?);
    bk(&path)?.args(["company", "list"]).assert().success();
    let connection = rusqlite::Connection::open(&path)?;
    let version: i64 =
        connection.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    assert_eq!(version, 22);
    Ok(())
}
