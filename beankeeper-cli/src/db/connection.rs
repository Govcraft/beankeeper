use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use secrecy::{ExposeSecret, SecretString};

use super::audit::savepoint::{AuditedTx, with_audited_tx, with_rolled_back_audited_tx};
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
        let version = super::schema::get_schema_version(ReadConn::new(&conn))?;
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

    /// Connection for internal storage and corruption fixtures.
    #[cfg(test)]
    pub(in crate::db) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(super) fn with_audited_tx<T>(
        &self,
        name: &'static str,
        run: impl FnOnce(&AuditedTx<'_>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        with_audited_tx(&self.conn, name, run)
    }

    /// Give a sequence of authorized mutations one reserved SQLite writer.
    pub(crate) fn in_write_batch<T>(
        &self,
        run: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        self.with_audited_tx("sp_write_batch", |tx| {
            super::audit::reserve_ledger_writer(tx)?;
            run()
        })
    }

    /// Exercise actual mutation checks and discard all of the writes.
    pub(crate) fn preview_write_batch<T>(
        &self,
        run: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        with_rolled_back_audited_tx(&self.conn, "sp_write_preview", |tx| {
            super::audit::reserve_ledger_writer(tx)?;
            run()
        })
    }

    /// Write a consistent copy, including committed WAL contents.
    pub(crate) fn snapshot_into(&self, path: &Path) -> Result<(), CliError> {
        let path = path
            .to_str()
            .ok_or_else(|| CliError::Usage("snapshot destination must be a UTF-8 path".into()))?;
        // SQLCipher inherits the main database key for the attached copy.
        self.conn.execute("VACUUM INTO ?1", [path])?;
        Ok(())
    }

    pub(crate) fn has_cipher_salt(&self) -> bool {
        self.conn
            .query_row("PRAGMA cipher_salt", [], |row| row.get::<_, String>(0))
            .is_ok_and(|salt| !salt.is_empty())
    }

    /// Borrows this ledger for query-only access without exposing its SQLite connection.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::Db;
    /// fn raw_connection(db: &Db) {
    ///     let _ = db.conn();
    /// }
    /// ```
    #[must_use]
    pub fn read(&self) -> ReadConn<'_> {
        ReadConn::new(&self.conn)
    }

    /// Run queries against one SQLite snapshot.
    ///
    /// Values can be returned from the callback. Its reader and prepared
    /// statements borrow the snapshot for the duration of the callback.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::{Db, connection::ReadConn};
    /// use beankeeper_cli::error::CliError;
    ///
    /// fn escape(db: &Db) -> Result<ReadConn<'_>, CliError> {
    ///     db.read_snapshot(|reader| Ok(reader))
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::{Db, connection::ReadStatement};
    /// use beankeeper_cli::error::CliError;
    ///
    /// fn escape(db: &Db) -> Result<ReadStatement<'_>, CliError> {
    ///     db.read_snapshot(|reader| Ok(reader.prepare("SELECT 1")?))
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns a callback error or a database error from the savepoint.
    pub fn read_snapshot<T>(
        &self,
        run: impl for<'snapshot> FnOnce(ReadConn<'snapshot>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        super::audit::atomically(&self.conn, || run(self.read()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Admin, Granted};

    #[test]
    fn read_snapshot_retains_one_view_and_releases_it_on_error() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("books.db");
        let writer = Db::open(&path, None)?;
        let db = Db::open_snapshot(&path, None)?;
        let count = |reader: ReadConn<'_>| -> Result<i64, CliError> {
            Ok(reader.query_row("SELECT COUNT(*) FROM companies", [], |row| row.get(0))?)
        };

        let result: Result<(), CliError> = db.read_snapshot(|reader| {
            assert_eq!(count(reader)?, 0);
            Granted::<Admin>::with_db(
                &writer,
                &super::super::Actor::new("fixture".parse()?),
                |grant| {
                    super::super::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
                },
            )?;
            assert_eq!(count(reader)?, 0);
            Err(CliError::Validation("stop snapshot".into()))
        });
        assert!(matches!(result, Err(CliError::Validation(message)) if message == "stop snapshot"));
        assert!(db.conn.is_autocommit());
        assert_eq!(count(db.read())?, 1);
        Ok(())
    }

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
        let version = super::super::schema::get_schema_version(db.read());
        assert!(version.is_ok());
        assert_eq!(version.ok(), Some(super::super::schema::CURRENT_VERSION));
    }
}
