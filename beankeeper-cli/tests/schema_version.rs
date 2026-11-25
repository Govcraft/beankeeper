use beankeeper_cli::db::{Db, SchemaVersion, current_schema_version, get_schema_version};
use beankeeper_cli::error::CliError;
use secrecy::SecretString;

#[test]
fn linked_schema_version_matches_an_encrypted_snapshot() -> Result<(), CliError> {
    const EXPECTED: SchemaVersion = current_schema_version();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("books.db");
    let passphrase = SecretString::from("schema-version-fixture");
    drop(Db::open(&path, Some(&passphrase))?);

    let snapshot = Db::open_snapshot(&path, Some(&passphrase))?;
    let stored = get_schema_version(snapshot.read())?;
    assert_eq!(i64::from(EXPECTED), stored);
    assert_eq!(EXPECTED.to_string(), stored.to_string());
    Ok(())
}

#[test]
fn ordinary_opens_refuse_an_older_ledger_without_writing() -> Result<(), CliError> {
    use beankeeper_cli::output::json::EnvelopeError;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.db");
    let expected = current_schema_version();
    let stored = SchemaVersion::try_from(i64::from(expected) - 1)?;
    let raw = rusqlite::Connection::open(&path)?;
    raw.execute_batch(
        "CREATE TABLE schema_version (version INTEGER);
         CREATE TABLE companies (slug TEXT);",
    )?;
    raw.execute(
        "INSERT INTO schema_version VALUES (?1)",
        [i64::from(stored)],
    )?;
    raw.execute("INSERT INTO companies VALUES ('legacy')", [])?;
    drop(raw);

    let before = std::fs::read(&path)?;
    for open in [Db::open_existing, Db::open, Db::open_snapshot] {
        let error = open(&path, None).expect_err("the path alone cannot upgrade a ledger");
        assert!(matches!(
            error,
            CliError::SchemaMismatch { stored: found, expected: required }
            if found == stored && required == expected
        ));
        assert_eq!(error.exit_code(), 4);
        let payload = EnvelopeError::from(&error);
        assert_eq!(payload.code.as_str(), "SCHEMA_MISMATCH");
        assert_eq!(
            payload.details,
            Some(serde_json::json!({
                "stored": stored,
                "expected": expected,
            }))
        );
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(directory.path().read_dir()?.count(), 1);
    }
    Ok(())
}

#[test]
fn standalone_upgrade_refuses_an_older_ledger_that_cannot_be_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("invalid-legacy.db");
    let raw = rusqlite::Connection::open(&path)?;
    raw.execute_batch(
        "CREATE TABLE schema_version (version INTEGER);
         INSERT INTO schema_version VALUES (1);",
    )?;
    drop(raw);
    let before = std::fs::read(&path)?;

    for args in [&["verify"][..], &["mcp", "serve"][..]] {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("bk"))
            .arg("--db")
            .arg(&path)
            .arg("--json")
            .args(args)
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BK_AGENT_TOKEN")
            .output()?;
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(output.stdout, Vec::<u8>::new());
        let error: serde_json::Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["error"]["code"], "DATABASE");
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(directory.path().read_dir()?.count(), 1);
    }
    Ok(())
}

#[test]
fn prepared_cli_dispatch_uses_the_connection_and_its_authorization() -> Result<(), CliError> {
    use beankeeper_cli::{cli::Cli, commands};
    use clap::Parser as _;

    let directory = tempfile::tempdir()?;
    let selected = directory.path().join("unused.db");
    let mut cli = Cli::parse_from(["bk", "company", "create", "prepared", "Prepared"]);
    cli.db.clone_from(&selected);
    let path = directory.path().join("prepared.db");
    commands::dispatch_with_db(&mut cli, Db::open(&path, None)?, None)?;
    let db = Db::open_existing(&path, None)?;
    assert!(beankeeper_cli::db::company_exists(
        db.read(),
        &"prepared".parse()?
    )?);
    assert!(!selected.exists());

    cli.agent_token = Some("untrusted token".into());
    assert!(matches!(
        commands::dispatch_with_db(&mut cli, db, None),
        Err(CliError::Unauthorized(_))
    ));

    let backup = directory.path().join("prepared.bk-backup");
    let mut cli = Cli::try_parse_from([
        std::ffi::OsStr::new("bk"),
        std::ffi::OsStr::new("backup"),
        std::ffi::OsStr::new("--output"),
        backup.as_os_str(),
    ])
    .map_err(|error| CliError::Usage(error.to_string()))?;
    cli.db.clone_from(&selected);
    commands::dispatch_with_db(&mut cli, Db::open_existing(&path, None)?, None)?;
    let snapshot = Db::open_snapshot(&backup, None)?;
    assert!(beankeeper_cli::db::company_exists(
        snapshot.read(),
        &"prepared".parse()?
    )?);
    assert!(!selected.exists());
    Ok(())
}
