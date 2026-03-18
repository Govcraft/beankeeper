use std::path::Path;

use rusqlite::Connection;
use secrecy::{ExposeSecret, SecretString};

use crate::error::CliError;

/// Database connection wrapper with `SQLCipher` support.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (or creates) a database at `path`, optionally encrypting with `passphrase`.
    ///
    /// Enables foreign keys, WAL journal mode, and ensures the schema is up to date.
    /// On Unix, sets file permissions to 0600 (owner-only read/write).
    ///
    /// # Errors
    ///
    /// Returns `CliError::Sqlite` on connection or pragma errors.
    pub fn open(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        let conn = Connection::open(path)?;

        if let Some(pp) = passphrase {
            conn.pragma_update(None, "key", pp.expose_secret())?;
        }

        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;

        super::schema::ensure_schema(&conn)?;

        // Best-effort permission tightening on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms).ok();
        }

        Ok(Self { conn })
    }

    /// Opens an in-memory database (useful for tests).
    ///
    /// Schema is initialised but no file permissions are set.
    ///
    /// # Errors
    ///
    /// Returns `CliError::Sqlite` on connection or schema errors.
    pub fn open_in_memory() -> Result<Self, CliError> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        super::schema::ensure_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Returns a reference to the underlying `rusqlite` connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_succeeds() {
        let db = Db::open_in_memory();
        assert!(db.is_ok());
    }

    #[test]
    fn open_file_creates_db() {
        let dir = tempfile::tempdir().ok();
        let Some(dir) = dir.as_ref() else {
            return;
        };
        let path = dir.path().join("test.db");
        let db = Db::open(&path, None);
        assert!(db.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn schema_version_is_set_after_open() {
        let db = Db::open_in_memory();
        assert!(db.is_ok());
        let Some(db) = db.ok() else {
            return;
        };
        let version = super::super::schema::get_schema_version(db.conn());
        assert!(version.is_ok());
        assert_eq!(version.ok(), Some(5));
    }
}
