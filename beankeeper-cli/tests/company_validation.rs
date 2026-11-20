//! Company-scoped commands must distinguish an unknown company from empty books.

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
        .env_remove("BEANKEEPER_COMPANY")
        .stdin(Stdio::null());
    Ok(command)
}

#[test]
fn unknown_company_is_not_found_for_every_scoped_command_family() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    let commands: &[&[&str]] = &[
        &["txn", "list"],
        &["account", "list"],
        &["budget", "list", "--year", "2026"],
        &["report", "trial-balance"],
        &["account", "create", "1000", "Cash", "--type", "asset"],
        &[
            "txn", "post", "-d", "test", "--debit", "1000:1", "--credit", "3000:1",
        ],
    ];
    for args in commands {
        let output = bk(&path)?
            .args(["--json", "--company", "missing"])
            .args(*args)
            .assert()
            .code(5)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty());
        let envelope: serde_json::Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(envelope["error"]["code"], "NOT_FOUND");
        assert_eq!(envelope["error"]["message"], "company 'missing' not found");
    }
    Ok(())
}

#[test]
fn existing_empty_company_succeeds_and_cross_company_reconcile_needs_no_slug() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    bk(&path)?
        .args(["company", "create", "acme", "Acme"])
        .assert()
        .success();
    for args in [
        vec!["account", "list"],
        vec!["txn", "list"],
        vec!["budget", "list", "--year", "2026"],
        vec!["report", "trial-balance"],
    ] {
        bk(&path)?
            .args(["--company", "acme"])
            .args(args)
            .assert()
            .success();
    }
    bk(&path)?.args(["txn", "reconcile"]).assert().success();
    Ok(())
}

#[cfg(unix)]
#[test]
fn encrypted_company_validation_consumes_passphrase_fd_once() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let passphrase = dir.path().join("passphrase");
    std::fs::write(&passphrase, "secret\n")?;
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["init", "--encrypt", "--demo"])
        .assert()
        .success();
    bk(&path)?
        .stdin(std::fs::File::open(&passphrase)?)
        .args([
            "--passphrase-fd",
            "0",
            "--company",
            "acme-consulting",
            "txn",
            "list",
        ])
        .assert()
        .success();
    Ok(())
}
