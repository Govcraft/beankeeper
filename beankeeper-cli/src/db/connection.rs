use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use secrecy::{ExposeSecret, SecretString};

use super::audit::{
    restore::ExistingRestoreTarget,
    savepoint::{AuditedTx, with_rolled_back_audited_tx},
};
use super::schema::{self, AuditHistory, SchemaVersion, current_schema_version};
use crate::error::CliError;

mod read;
pub use read::{ReadConn, ReadStatement};

/// Database connection wrapper with `SQLCipher` support.
pub struct Db {
    conn: Connection,
    id: ConnectionId,
    passphrase: Option<SecretString>,
    schema_version: SchemaVersion,
    audit_history: AuditHistory,
}

/// Process-local identity of one open connection, including in-memory ledgers.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConnectionId(Arc<()>);

impl ConnectionId {
    pub(super) fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Db {
    pub(super) fn from_keyed_connection(
        conn: Connection,
        passphrase: Option<&SecretString>,
        schema_version: SchemaVersion,
    ) -> Self {
        Self {
            conn,
            id: ConnectionId::default(),
            passphrase: passphrase.cloned(),
            schema_version,
            audit_history: AuditHistory::from(schema_version),
        }
    }

    pub(super) const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    pub(super) fn snapshot_key(&self) -> Option<&SecretString> {
        self.passphrase.as_ref()
    }

    pub(crate) fn id(&self) -> &ConnectionId {
        &self.id
    }

    pub(crate) fn file_path(&self) -> Option<PathBuf> {
        self.conn
            .path()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    }

    pub(crate) fn close(self) -> Result<(), CliError> {
        self.conn.close().map_err(|(_, error)| error.into())
    }

    /// Open a current-version ledger read-only, without migrations or PRAGMA writes.
    ///
    /// Use [`super::current_schema_version`] to obtain the expected version.
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
        let conn = open_snapshot_file(path, passphrase)?;
        let stored_version = schema::get_schema_version(ReadConn::physical(&conn))?;
        let version = SchemaVersion::try_from(stored_version)
            .ok()
            .filter(|version| i64::from(*version) >= minimum);
        let Some(version) = version else {
            let current = schema::CURRENT_VERSION;
            let expected = if minimum == current {
                current.to_string()
            } else {
                format!("{minimum} through {current}")
            };
            return Err(CliError::Database(format!(
                "not a supported beankeeper snapshot: schema version {stored_version}; expected {expected}"
            )));
        };
        Ok(Self::from_keyed_connection(conn, None, version))
    }

    pub(super) fn open_supported_snapshot(
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        let conn = open_snapshot_file(path, passphrase)?;
        let version =
            SchemaVersion::try_from(schema::get_schema_version(ReadConn::physical(&conn))?)?;
        Ok(Self::from_keyed_connection(conn, None, version))
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
        require_existing_file(path)?;
        // Omit SQLITE_OPEN_CREATE so a file removed after the existence check
        // cannot be silently recreated.
        Self::open_with_flags(
            path,
            passphrase,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    // Restore targets must already be at the running schema. In particular, an
    // unauthorized or failed restore must never migrate the live ledger.
    pub(crate) fn open_for_restore(
        target: &ExistingRestoreTarget<'_>,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        // Omit SQLITE_OPEN_CREATE in case the file disappeared after the check.
        let conn = open_keyed(
            target.as_ref(),
            passphrase,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let reader = ReadConn::physical(&conn);
        let version = super::schema::get_schema_version(reader)?;
        if version != super::schema::CURRENT_VERSION {
            return Err(CliError::Database(format!(
                "restore target schema version {version}; expected {}",
                super::schema::CURRENT_VERSION
            )));
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self::from_keyed_connection(
            conn,
            passphrase,
            current_schema_version(),
        ))
    }

    fn open_with_flags(
        path: &Path,
        passphrase: Option<&SecretString>,
        flags: OpenFlags,
    ) -> Result<Self, CliError> {
        let conn = open_keyed(path, passphrase, flags)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        super::schema::ensure_schema(&conn)?;
        tighten_permissions(path);
        Ok(Self::from_keyed_connection(
            conn,
            passphrase,
            current_schema_version(),
        ))
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

        Ok(Self::from_keyed_connection(
            conn,
            None,
            current_schema_version(),
        ))
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
        AuditedTx::run(self.read(), name, run)
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
        with_rolled_back_audited_tx(self.read(), "sp_write_preview", |tx| {
            super::audit::reserve_ledger_writer(tx)?;
            run()
        })
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
        ReadConn::for_db(self)
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

pub(super) fn require_existing_file(path: &Path) -> Result<(), CliError> {
    if !path.try_exists()? {
        return Err(CliError::NotFound(format!(
            "database '{}' not found; run `bk init --path '{}'` to create it",
            path.display(),
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn open_keyed(
    path: &Path,
    passphrase: Option<&SecretString>,
    flags: OpenFlags,
) -> Result<Connection, CliError> {
    let conn = Connection::open_with_flags(path, flags)?;
    if let Some(pp) = passphrase {
        conn.pragma_update(None, "key", pp.expose_secret())?;
    }
    Ok(conn)
}

fn open_snapshot_file(
    path: &Path,
    passphrase: Option<&SecretString>,
) -> Result<Connection, CliError> {
    open_keyed(
        path,
        passphrase,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

#[cfg(unix)]
pub(super) fn tighten_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).ok();
}

#[cfg(not(unix))]
pub(super) fn tighten_permissions(_: &Path) {}

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
