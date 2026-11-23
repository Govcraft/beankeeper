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
