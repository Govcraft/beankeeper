use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use secrecy::{ExposeSecret, SecretString};

use crate::error::CliError;

mod read;
pub use read::{ReadConn, ReadStatement};

/// Database connection wrapper with `SQLCipher` support.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open a current-version ledger read-only, without migrations or PRAGMA writes.
    ///
    /// # Errors
    /// Returns a database error for an invalid, unsupported, or unreadable snapshot.
    pub fn open_snapshot(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        Self::open_compatible_snapshot(path, passphrase, super::schema::CURRENT_VERSION)
    }

    pub(crate) fn open_compatible_snapshot(
        path: &Path,
        passphrase: Option<&SecretString>,
        minimum: i64,
    ) -> Result<Self, CliError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if let Some(pp) = passphrase {
            conn.pragma_update(None, "key", pp.expose_secret())?;
        }
        let version = super::schema::get_schema_version(&conn)?;
        let current = super::schema::CURRENT_VERSION;
        if !(minimum..=current).contains(&version) {
            let expected = if minimum == current {
                current.to_string()
            } else {
                format!("{minimum} through {current}")
            };
            return Err(CliError::Database(format!(
                "not a supported beankeeper snapshot: schema version {version}; expected {expected}"
            )));
        }
        Ok(Self { conn })
    }

    /// Opens (or creates) a database at `path`, optionally encrypting with `passphrase`.
    ///
    /// Enables foreign keys, WAL journal mode, and ensures the schema is up to date.
    /// On Unix, sets file permissions to 0600 (owner-only read/write).
    ///
    /// # Errors
    ///
    /// Returns `CliError::Sqlite` on connection or pragma errors.
    pub fn open(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        Self::open_with_flags(path, passphrase, OpenFlags::default())
    }

    /// Opens an existing database without creating a new file.
    ///
    /// Applies the same encryption, connection settings, and schema migrations
    /// as [`Self::open`]. Commands other than `init` must use this method.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::NotFound`] with initialization instructions when the
    /// path does not exist, or a database error if it cannot be opened.
    pub fn open_existing(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        if !path.try_exists()? {
            return Err(CliError::NotFound(format!(
                "database '{}' not found; run `bk init --path '{}'` to create it",
                path.display(),
                path.display()
            )));
        }
        // Omit SQLITE_OPEN_CREATE so a file removed after the existence check
        // cannot be silently recreated.
        Self::open_with_flags(
            path,
            passphrase,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    fn open_with_flags(
        path: &Path,
        passphrase: Option<&SecretString>,
        flags: OpenFlags,
    ) -> Result<Self, CliError> {
        let conn = Connection::open_with_flags(path, flags)?;

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

    /// Borrows this ledger for query-only access without exposing its SQLite connection.
    #[must_use]
    pub fn read(&self) -> ReadConn<'_> {
        ReadConn::new(&self.conn)
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
        assert_eq!(version.ok(), Some(super::super::schema::CURRENT_VERSION));
    }
}
