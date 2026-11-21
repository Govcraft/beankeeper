//! Full snapshots preserve encryption, identity, and data outside accounting exports.

use std::error::Error;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use assert_cmd::prelude::*;
use beankeeper_cli::cli::Cli;
use beankeeper_cli::db::Db;
use beankeeper_cli::{commands, error::CliError};
use clap::Parser as _;
use secrecy::SecretString;
use serde_json::Value;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn Error>>;

fn bk(path: &Path, key: Option<&Path>) -> Result<Command, Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("bk")?;
    cmd.arg("--db").arg(path).arg("--json").stdin(Stdio::null());
    for var in [
        "BEANKEEPER_DB",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BEANKEEPER_PASSPHRASE_CMD",
        "BK_AGENT_TOKEN",
        "BK_ACTOR",
    ] {
        cmd.env_remove(var);
    }
    if let Some(key) = key {
        cmd.arg("--passphrase-file").arg(key);
    }
    Ok(cmd)
}

fn success(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn data(path: &Path, key: Option<&Path>, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    Ok(success(&bk(path, key)?.args(args).output()?)?["data"].clone())
}

fn seed(path: &Path, key: Option<&Path>) -> Result<Option<String>, Box<dyn Error>> {
    data(path, key, &["init", "--demo"])?;
    data(
        path,
        key,
        &[
            "--company",
            "acme-consulting",
            "schedule",
            "create",
            "-d",
            "Rent",
            "--start",
            "2027-01-01",
            "--frequency",
            "monthly",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
        ],
    )?;
    let token = if key.is_some() {
        data(path, key, &["key", "init"])?;
        let issued = data(
            path,
            key,
            &["agent", "issue", "--name", "snapshot", "--generate-key"],
        )?;
        Some(issued["token"].as_str().ok_or("missing token")?.to_owned())
    } else {
        None
    };
    Ok(token)
}

fn compare_ledgers(path: &Path, restored: &Path, key: Option<&Path>) -> TestResult {
    assert_eq!(
        data(path, key, &["verify"])?,
        data(restored, key, &["verify"])?
    );
    assert_eq!(
        data(path, key, &["agent", "list"])?,
        data(restored, key, &["agent", "list"])?
    );
    assert_eq!(
        data(
            path,
            key,
            &["--company", "acme-consulting", "schedule", "list"]
        )?,
        data(
            restored,
            key,
            &["--company", "acme-consulting", "schedule", "list"]
        )?
    );
    // Export's standalone format contains no envelope and must remain unchanged.
    let source_export = bk(path, key)?.arg("export").output()?;
    let restored_export = bk(restored, key)?.arg("export").output()?;
    assert!(source_export.status.success());
    assert!(restored_export.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&source_export.stdout)?,
        serde_json::from_slice::<Value>(&restored_export.stdout)?
    );
    let secret = SecretString::from("snapshot-test-secret");
    let source_db = Db::open_existing(path, key.map(|_| &secret))?;
    let restored_db = Db::open_existing(restored, key.map(|_| &secret))?;
    // Compare every table, including internal sequences and private identity state.
    let mut tables = source_db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    for table in tables.query_map([], |row| row.get::<_, String>(0))? {
        let table = table?;
        let sql = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let rows = |db: &Db| -> rusqlite::Result<Vec<Vec<rusqlite::types::Value>>> {
            let mut stmt = db.conn().prepare(&sql)?;
            let columns = stmt.column_count();
            stmt.query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })?
            .collect()
        };
        assert_eq!(rows(&source_db)?, rows(&restored_db)?, "table {table}");
    }
    Ok(())
}

fn round_trip(encrypted: bool) -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("source.db");
    // A different directory simulates moving the standalone snapshot to another host.
    let second = tempdir()?;
    let restored = second.path().join("restored.db");
    let backup = dir.path().join("ledger.bk-backup");
    let key_path = dir.path().join("passphrase");
    std::fs::write(&key_path, "snapshot-test-secret\n")?;
    let key = encrypted.then_some(key_path.as_path());
    let token = seed(&path, key)?;
    // Keep a second connection open. A posting committed through the CLI remains in WAL.
    let secret = SecretString::from("snapshot-test-secret");
    let live = Db::open_existing(&path, encrypted.then_some(&secret))?;
    let post = [
        "--company",
        "acme-consulting",
        "txn",
        "post",
        "-d",
        "Live sale",
        "--debit",
        "1000:10",
        "--credit",
        "4000:10",
        "--reference",
        "live-sale",
    ];
    let mut cmd = bk(&path, key)?;
    if let Some(token) = &token {
        cmd.args(["--agent-token", token]);
    }
    success(&cmd.args(post).output()?)?;
    let snapshot_output = bk(&path, key)?
        .arg("backup")
        .arg("--output")
        .arg(&backup)
        .output()?;
    let snapshot = success(&snapshot_output)?;
    assert_eq!(snapshot["meta"]["command"], "backup");
    assert_eq!(snapshot["data"]["encrypted"], encrypted);
    assert_eq!(snapshot["data"]["schema_version"], 21);
    assert!(snapshot["data"]["bytes"].as_u64().ok_or("missing bytes")? > 0);
    assert_eq!(
        std::fs::read(&backup)?.starts_with(b"SQLite format 3\0"),
        !encrypted
    );
    let mut cmd = bk(&restored, key)?;
    if let Some(token) = &token {
        cmd.args(["--agent-token", token]);
    }
    let output = cmd.arg("restore").arg("--input").arg(&backup).output()?;
    let result = success(&output)?;
    assert_eq!(result["meta"]["command"], "restore");
    assert_eq!(result["data"]["encrypted"], encrypted);
    for output in [&snapshot_output, &output] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.contains("snapshot-test-secret"));
        assert!(!stderr.contains("snapshot-test-secret"));
        if let Some(token) = &token {
            assert!(!stdout.contains(token));
            assert!(!stderr.contains(token));
        }
    }
    compare_ledgers(&path, &restored, key)?;
    if let Some(token) = &token {
        bk(&restored, None)?.arg("verify").assert().code(4);
        bk(&restored, key)?
            .args([
                "--agent-token",
                token,
                "--company",
                "acme-consulting",
                "txn",
                "post",
                "-d",
                "Restored sale",
                "--debit",
                "1000:5",
                "--credit",
                "4000:5",
            ])
            .assert()
            .success();
        data(&restored, key, &["verify"])?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [&backup, &restored] {
            assert_eq!(std::fs::metadata(file)?.permissions().mode() & 0o777, 0o600);
        }
    }
    drop(live);
    Ok(())
}

#[test]
fn encrypted_round_trip_preserves_root_tokens_and_live_wal_postings() -> TestResult {
    round_trip(true)
}

#[test]
fn plaintext_round_trip_preserves_accounting_and_schedules() -> TestResult {
    round_trip(false)
}

#[test]
fn overwrite_requires_force_and_force_publishes_a_complete_snapshot() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    let destination = dir.path().join("destination.db");
    data(&source, None, &["init", "--demo"])?;
    std::fs::write(&backup, b"keep this")?;
    bk(&source, None)?
        .arg("backup")
        .arg("--output")
        .arg(&backup)
        .assert()
        .code(3);
    assert_eq!(std::fs::read(&backup)?, b"keep this");
    bk(&source, None)?
        .args(["backup", "--force", "--output"])
        .arg(&backup)
        .assert()
        .success();
    data(&destination, None, &["init"])?;
    let original = std::fs::read(&destination)?;
    bk(&destination, None)?
        .args(["restore", "--input"])
        .arg(&backup)
        .assert()
        .code(3);
    assert_eq!(std::fs::read(&destination)?, original);
    bk(&destination, None)?
        .args(["restore", "--force", "--input"])
        .arg(&backup)
        .assert()
        .success();
    assert_eq!(
        data(&source, None, &["verify"])?,
        data(&destination, None, &["verify"])?
    );
    assert_eq!(
        data(&source, None, &["company", "list"])?,
        data(&destination, None, &["company", "list"])?
    );
    Ok(())
}

#[test]
fn invalid_inputs_never_publish_or_replace_a_destination() -> TestResult {
    let dir = tempdir()?;
    let input = dir.path().join("input");
    let destination = dir.path().join("destination.db");
    let valid = dir.path().join("valid.db");
    data(&valid, None, &["init", "--demo"])?;
    // Include empty/random files, ordinary SQLite, truncated ledger, and forged version.
    let random_sqlite = dir.path().join("other.db");
    let conn = rusqlite::Connection::open(&random_sqlite)?;
    conn.execute_batch("CREATE TABLE unrelated (value TEXT)")?;
    drop(conn);
    let forged = dir.path().join("forged.db");
    let conn = rusqlite::Connection::open(&forged)?;
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER); INSERT INTO schema_version VALUES (21)",
    )?;
    drop(conn);
    let mut truncated = std::fs::read(&valid)?;
    truncated.truncate(truncated.len() / 2);
    for bytes in [
        vec![],
        b"not a ledger".to_vec(),
        std::fs::read(&random_sqlite)?,
        truncated,
        std::fs::read(&forged)?,
    ] {
        std::fs::write(&input, bytes)?;
        for existing in [false, true] {
            if existing {
                std::fs::write(&destination, b"preserve destination")?;
            }
            bk(&destination, None)?
                .args(["restore", "--force", "--input"])
                .arg(&input)
                .assert()
                .code(4);
            if existing {
                assert_eq!(std::fs::read(&destination)?, b"preserve destination");
                std::fs::remove_file(&destination)?;
            } else {
                assert!(!destination.exists());
            }
        }
        assert!(
            !std::fs::read_dir(dir.path())?.any(|entry| entry.is_ok_and(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".bk-snapshot-")))
        );
    }
    Ok(())
}

#[test]
fn required_paths_missing_sources_and_self_overwrites_are_rejected() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    bk(&source, None)?.arg("backup").assert().code(2);
    bk(&source, None)?.arg("restore").assert().code(2);
    let mut command = Command::cargo_bin("bk")?;
    command
        .current_dir(dir.path())
        .env_remove("BEANKEEPER_DB")
        .env_remove("BK_AGENT_TOKEN")
        .arg("restore")
        .arg("--input")
        .arg(&backup)
        .assert()
        .code(2)
        .stderr(predicates::str::contains("explicit --db"));
    Command::cargo_bin("bk")?
        .current_dir(dir.path())
        .env("BEANKEEPER_DB", &source)
        .env_remove("BK_AGENT_TOKEN")
        .args(["restore", "--input"])
        .arg(&backup)
        .assert()
        .code(2);
    bk(&source, None)?
        .arg("backup")
        .arg("--output")
        .arg(&backup)
        .assert()
        .code(5);
    bk(&source, None)?
        .arg("restore")
        .arg("--input")
        .arg(&backup)
        .assert()
        .code(5);
    assert!(!source.exists());
    assert!(!backup.exists());
    data(&source, None, &["init"])?;
    for args in [
        ["backup", "--force", "--output"],
        ["restore", "--force", "--input"],
    ] {
        bk(&source, None)?.args(args).arg(&source).assert().code(3);
    }
    data(&source, None, &["verify"])?;
    Ok(())
}

#[test]
fn embedded_restore_requires_a_command_line_destination() -> TestResult {
    let dir = tempdir()?;
    let snapshot = dir.path().join("missing.db");
    let destination = dir.path().join("restore.db");
    let restore = [
        "bk".as_ref(),
        "restore".as_ref(),
        "--input".as_ref(),
        snapshot.as_os_str(),
    ];
    let cli = Cli::try_parse_from(restore)?;
    assert!(matches!(
        commands::dispatch(&cli, None),
        Err(CliError::Usage(message)) if message.contains("explicit --db")
    ));

    let explicit = restore
        .into_iter()
        .chain(["--db".as_ref(), destination.as_os_str()]);
    let cli = Cli::try_parse_from(explicit)?;
    assert!(matches!(
        commands::dispatch(&cli, None),
        Err(CliError::NotFound(_))
    ));
    assert!(!destination.exists());
    Ok(())
}

#[test]
fn force_rejects_sqlite_sidecars_and_source_aliases() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    data(&source, None, &["init"])?;
    bk(&source, None)?
        .arg("backup")
        .arg("--output")
        .arg(&backup)
        .assert()
        .success();
    let live = Db::open_existing(&source, None)?;
    bk(&source, None)?
        .args(["restore", "--force", "--input"])
        .arg(&backup)
        .assert()
        .code(3);
    let wal = dir.path().join("source.db-wal");
    bk(&source, None)?
        .args(["backup", "--force", "--output"])
        .arg(&wal)
        .assert()
        .code(3);
    data(&source, None, &["verify"])?;
    drop(live);
    #[cfg(unix)]
    {
        let alias = dir.path().join("alias.db");
        std::fs::hard_link(&source, &alias)?;
        bk(&source, None)?
            .args(["backup", "--force", "--output"])
            .arg(&alias)
            .assert()
            .code(3);
        std::fs::remove_file(&alias)?;
        std::os::unix::fs::symlink(&source, &alias)?;
        bk(&source, None)?
            .args(["backup", "--force", "--output"])
            .arg(&alias)
            .assert()
            .code(3);
    }
    Ok(())
}

#[test]
fn wrong_passphrases_and_invalid_tokens_preserve_destination() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    let destination = dir.path().join("destination.db");
    let key = dir.path().join("key");
    let wrong = dir.path().join("wrong-key");
    std::fs::write(&key, "snapshot-test-secret")?;
    std::fs::write(&wrong, "incorrect-secret")?;
    seed(&source, Some(&key))?;
    bk(&source, Some(&key))?
        .args(["backup", "--output"])
        .arg(&backup)
        .assert()
        .success();
    std::fs::write(&destination, b"keep destination")?;
    for key in [None, Some(wrong.as_path())] {
        bk(&destination, key)?
            .args(["restore", "--force", "--input"])
            .arg(&backup)
            .assert()
            .code(4);
        assert_eq!(std::fs::read(&destination)?, b"keep destination");
    }
    bk(&destination, Some(&key))?
        .args([
            "--agent-token",
            "invalid-token",
            "restore",
            "--force",
            "--input",
        ])
        .arg(&backup)
        .assert()
        .code(6);
    assert_eq!(std::fs::read(&destination)?, b"keep destination");
    let original = std::fs::read(&backup)?;
    bk(&source, Some(&wrong))?
        .args(["backup", "--force", "--output"])
        .arg(&backup)
        .assert()
        .code(4);
    assert_eq!(std::fs::read(&backup)?, original);
    Ok(())
}

#[test]
fn verification_rejects_audit_tampering_on_both_backup_and_restore() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    let destination = dir.path().join("destination.db");
    data(&source, None, &["init", "--demo"])?;
    bk(&source, None)?
        .args(["backup", "--output"])
        .arg(&backup)
        .assert()
        .success();
    let conn = rusqlite::Connection::open(&source)?;
    conn.execute_batch(
        "DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE id = 1",
    )?;
    drop(conn);
    let good_backup = std::fs::read(&backup)?;
    bk(&source, None)?
        .args(["backup", "--force", "--output"])
        .arg(&backup)
        .assert()
        .code(4);
    assert_eq!(std::fs::read(&backup)?, good_backup);
    std::fs::write(&destination, b"keep destination")?;
    bk(&destination, None)?
        .args(["restore", "--force", "--input"])
        .arg(&source)
        .assert()
        .code(4);
    assert_eq!(std::fs::read(&destination)?, b"keep destination");
    Ok(())
}

#[test]
fn restore_rejects_unsupported_schema_without_migration_or_modifying_input() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let destination = dir.path().join("destination.db");
    for version in [20, 22] {
        let conn = rusqlite::Connection::open(&source)?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS schema_version; CREATE TABLE schema_version (version INTEGER)",
        )?;
        conn.execute("INSERT INTO schema_version VALUES (?1)", [version])?;
        drop(conn);
        let original = std::fs::read(&source)?;
        bk(&destination, None)?
            .args(["restore", "--input"])
            .arg(&source)
            .assert()
            .code(4)
            .stderr(predicates::str::contains("schema version"));
        assert_eq!(std::fs::read(&source)?, original);
        assert!(!destination.exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn restore_accepts_db_after_subcommand_and_one_shot_passphrase_source() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let backup = dir.path().join("backup");
    let destination = dir.path().join("restored.db");
    let key = dir.path().join("key");
    std::fs::write(&key, "snapshot-test-secret")?;
    data(&source, Some(&key), &["init", "--demo"])?;
    bk(&source, None)?
        .stdin(std::fs::File::open(&key)?)
        .args(["--passphrase-fd", "0", "backup", "--output"])
        .arg(&backup)
        .assert()
        .success();
    let mut cmd = Command::cargo_bin("bk")?;
    cmd.env_remove("BK_AGENT_TOKEN")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .stdin(std::fs::File::open(&key)?)
        .args(["restore", "--input"])
        .arg(&backup)
        .arg("--db")
        .arg(&destination)
        .args(["--passphrase-fd", "0"])
        .assert()
        .success();
    data(&destination, Some(&key), &["verify"])?;
    Ok(())
}

#[test]
fn restore_cannot_replace_an_enforced_destination_with_an_unenforced_snapshot() -> TestResult {
    let dir = tempdir()?;
    let source = dir.path().join("source.db");
    let destination = dir.path().join("destination.db");
    let snapshot = dir.path().join("snapshot.db");
    data(&source, None, &["init"])?;
    bk(&source, None)?
        .args(["backup", "--output"])
        .arg(&snapshot)
        .assert()
        .success();
    let db = Db::open(&destination, None)?;
    beankeeper_cli::db::audit::set_policy(
        db.conn(),
        &beankeeper_cli::db::Actor::new("owner"),
        true,
    )?;
    drop(db);
    let original = std::fs::read(&destination)?;
    bk(&destination, None)?
        .args(["restore", "--force", "--input"])
        .arg(&snapshot)
        .assert()
        .code(6)
        .stderr(predicates::str::contains(
            "cannot overwrite an enforced ledger",
        ));
    assert_eq!(std::fs::read(&destination)?, original);
    Ok(())
}
