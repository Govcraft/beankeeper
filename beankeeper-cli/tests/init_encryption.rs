//! Encryption initialization must support the same configured sources as reopening.

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
fn encrypted_init_uses_passphrase_file_and_reopens_only_with_correct_key() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("encrypted.db");
    let passphrase = dir.path().join("passphrase");
    std::fs::write(&passphrase, "test secret\n")?;

    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["init", "--encrypt"])
        .assert()
        .success();
    let bytes = std::fs::read(&path)?;
    assert!(!bytes.starts_with(b"SQLite format 3\0"));
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["company", "list"])
        .assert()
        .success();
    bk(&path)?.args(["company", "list"]).assert().code(4);
    std::fs::write(&passphrase, "wrong secret\n")?;
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["company", "list"])
        .assert()
        .code(4);
    Ok(())
}

#[cfg(unix)]
#[test]
fn encrypted_init_uses_passphrase_fd_without_a_tty() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("encrypted.db");
    let passphrase = dir.path().join("passphrase");
    std::fs::write(&passphrase, "fd secret\n")?;
    bk(&path)?
        .stdin(std::fs::File::open(&passphrase)?)
        .args(["--passphrase-fd", "0", "init", "--encrypt"])
        .assert()
        .success();
    bk(&path)?
        .stdin(std::fs::File::open(&passphrase)?)
        .args(["--passphrase-fd", "0", "company", "list"])
        .assert()
        .success();
    Ok(())
}

#[cfg(unix)]
#[test]
fn encrypted_init_uses_passphrase_command_without_a_tty() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("encrypted.db");
    bk(&path)?
        .env("BEANKEEPER_PASSPHRASE_CMD", "printf 'command secret\\n'")
        .args(["init", "--encrypt"])
        .assert()
        .success();
    bk(&path)?
        .env("BEANKEEPER_PASSPHRASE_CMD", "printf 'command secret\\n'")
        .args(["company", "list"])
        .assert()
        .success();
    Ok(())
}

#[test]
fn encrypted_init_without_a_source_or_tty_fails_without_creating_a_database() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("encrypted.db");
    bk(&path)?
        .args(["init", "--encrypt"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--passphrase-file"));
    assert!(!path.exists());
    Ok(())
}

#[test]
fn failed_forced_encrypted_init_preserves_the_existing_database() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    let original = std::fs::read(&path)?;
    bk(&path)?
        .args(["init", "--encrypt", "--force"])
        .assert()
        .code(2);
    assert_eq!(std::fs::read(&path)?, original);
    Ok(())
}

#[test]
fn forced_encrypted_init_can_replace_a_plaintext_ledger_with_enforcement_off() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let passphrase = dir.path().join("passphrase");
    std::fs::write(&passphrase, "replacement secret")?;
    bk(&path)?.arg("init").assert().success();
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["init", "--encrypt", "--force"])
        .assert()
        .success();
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["company", "list"])
        .assert()
        .success();
    assert!(!std::fs::read(&path)?.starts_with(b"SQLite format 3\0"));
    Ok(())
}
