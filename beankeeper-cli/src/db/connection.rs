use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use rusqlite::{Connection, OpenFlags};
use secrecy::{ExposeSecret, SecretString};

use super::audit::{restore::ExistingRestoreTarget, savepoint::AuditedTx};
use super::schema::{self, SchemaVersion, current_schema_version, stored_schema};
use crate::error::CliError;
use crate::ledger_target::LedgerTarget;

mod read;
mod write;
pub use read::{ReadConn, ReadStatement};
use write::WriteAuthorization;
pub(super) use write::WriteGrant;
pub use write::{LedgerWriteAuthorizer, LedgerWriteKind, WriteDenied};

/// Database connection wrapper with `SQLCipher` support.
pub struct Db {
    conn: Connection,
    id: ConnectionId,
    passphrase: Option<SecretString>,
    schema_version: SchemaVersion,
    writes: WriteAuthorization,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Db").finish_non_exhaustive()
    }
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
            writes: WriteAuthorization::default(),
        }
    }

    /// Apply host write policy to this opened connection, including embedded CLI and MCP calls.
    ///
    /// MCP binds the target to the registered name or to `startup`. For embedded
    /// CLI or direct library use, supply the target that identifies this ledger to the host.
    #[must_use]
    pub fn with_write_authorizer(
        mut self,
        target: LedgerTarget,
        authorizer: impl LedgerWriteAuthorizer,
    ) -> Self {
        self.writes = WriteAuthorization::new(target, authorizer);
        self
    }

    pub(crate) fn at_target(mut self, target: LedgerTarget) -> Self {
        self.writes.set_target(target);
        self
    }

    pub(crate) const fn ledger_target(&self) -> &LedgerTarget {
        self.writes.target()
    }

    pub(crate) const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    pub(crate) fn snapshot_key(&self) -> Option<&SecretString> {
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
    /// # Errors
    /// Returns [`CliError::SchemaMismatch`] for a supported older schema, or a
    /// database error for an invalid, unsupported, or unreadable snapshot.
    pub fn open_snapshot(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        Self::open_compatible_snapshot(path, passphrase, current_schema_version())
    }

    pub(crate) fn open_compatible_snapshot(
        path: &Path,
        passphrase: Option<&SecretString>,
        minimum: SchemaVersion,
    ) -> Result<Self, CliError> {
        let conn = open_snapshot_file(path, passphrase)?;
        let version = stored_schema(ReadConn::physical(&conn))?.require_at_least(minimum)?;
        Ok(Self::from_keyed_connection(conn, None, version))
    }

    pub(crate) fn open_supported_snapshot(
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        let conn = open_snapshot_file(path, passphrase)?;
        let version = stored_schema(ReadConn::physical(&conn))?;
        Ok(Self::from_keyed_connection(conn, None, version))
    }

    /// Open a current ledger or initialize an empty database, optionally encrypted.
    ///
    /// Enables foreign keys and WAL mode. On Unix, sets file permissions to 0600.
    /// # Errors
    /// Returns [`CliError::SchemaMismatch`] for a supported older ledger. Use
    /// [`Self::open_after_snapshot`] to migrate it. Other failures include
    /// unsupported schemas, non-ledger databases and connection errors.
    pub fn open(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        Self::open_or_initialize(
            open_keyed(path, passphrase, OpenFlags::default())?,
            path,
            passphrase,
        )
    }

    /// Open a current ledger or initialize an existing empty database.
    ///
    /// This never creates a missing file or migrates a ledger.
    ///
    /// # Errors
    /// Returns [`CliError::SchemaMismatch`] for a supported older ledger,
    /// [`CliError::NotFound`] for a missing file, or a database error.
    pub fn open_existing(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        require_existing_file(path)?;
        Self::open_or_initialize(open_existing_keyed(path, passphrase)?, path, passphrase)
    }

    fn open_or_initialize(
        conn: Connection,
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        if let Some(version) = schema::read_schema(ReadConn::physical(&conn))? {
            version.require_at_least(current_schema_version())?;
        } else {
            conn.pragma_update(None, "foreign_keys", "ON")?;
            schema::initialize(&conn)?;
        }
        Self::finish_file_open(conn, path, passphrase)
    }

    // Restore targets must already be at the running schema.
    pub(crate) fn open_for_restore(
        target: &ExistingRestoreTarget<'_>,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        let conn = open_existing_keyed(target.as_ref(), passphrase)?;
        stored_schema(ReadConn::physical(&conn))?.require_at_least(current_schema_version())?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self::from_keyed_connection(
            conn,
            passphrase,
            current_schema_version(),
        ))
    }

    pub(super) fn finish_file_open(
        conn: Connection,
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
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
        initialize_sqlcipher();
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        schema::initialize(&conn)?;

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
        self.with_audited_tx_for(LedgerWriteKind::default(), name, run)
    }

    pub(super) fn with_audited_tx_for<T>(
        &self,
        kind: LedgerWriteKind,
        name: &'static str,
        run: impl FnOnce(&AuditedTx<'_>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        self.admit_write(kind, |grant| AuditedTx::run(grant, name, run))
    }

    /// Give a sequence of authorized mutations one reserved `SQLite` writer.
    pub(crate) fn in_write_batch<T>(
        &self,
        run: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        self.in_write_batch_for(LedgerWriteKind::default(), run)
    }

    pub(crate) fn in_write_batch_for<T>(
        &self,
        kind: LedgerWriteKind,
        run: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        self.with_audited_tx_for(kind, "sp_write_batch", |tx| {
            super::audit::reserve_ledger_writer(tx)?;
            run()
        })
    }

    /// Exercise actual mutation checks and discard all of the writes.
    pub(crate) fn preview_write_batch<T>(
        &self,
        run: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        self.admit_write(LedgerWriteKind::default(), |grant| {
            AuditedTx::preview(grant, "sp_write_preview", |tx| {
                super::audit::reserve_ledger_writer(tx)?;
                run()
            })
        })
    }

    pub(crate) fn has_cipher_salt(&self) -> bool {
        self.conn
            .query_row("PRAGMA cipher_salt", [], |row| row.get::<_, String>(0))
            .is_ok_and(|salt| !salt.is_empty())
    }

    /// Borrows this ledger for query-only access without exposing its `SQLite` connection.
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

    /// Run queries against one `SQLite` snapshot.
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

/// Finish `SQLite` and `SQLCipher` global setup before any ledger connection opens.
///
/// `sqlite3_initialize` marks `SQLite` initialized and releases its mutex before
/// it runs `SQLCipher`'s extra initialization. A second thread that opens a
/// connection in that window skips initialization and its `PRAGMA key` fails
/// with "An error occurred with PRAGMA key or rekey" because `SQLCipher` is not
/// ready yet. The thread that first initializes `SQLite` completes both steps
/// before its open returns, so performing that first open under a [`Once`]
/// makes every other opener in this crate wait until setup has finished.
///
/// Embedding hosts that open `SQLite` through other code must not race that
/// code against their first beankeeper ledger open.
fn initialize_sqlcipher() {
    static SQLCIPHER: Once = Once::new();
    // A failure here leaves SQLite uninitialized; the caller's own open then
    // initializes it and reports the error.
    SQLCIPHER.call_once(|| drop(Connection::open_in_memory()));
}

pub(super) fn open_keyed(
    path: &Path,
    passphrase: Option<&SecretString>,
    flags: OpenFlags,
) -> Result<Connection, CliError> {
    initialize_sqlcipher();
    let conn = Connection::open_with_flags(path, flags)?;
    if let Some(pp) = passphrase {
        conn.pragma_update(None, "key", pp.expose_secret())?;
    }
    Ok(conn)
}

pub(super) fn open_existing_keyed(
    path: &Path,
    passphrase: Option<&SecretString>,
) -> Result<Connection, CliError> {
    // A file removed after an existence check must never be silently recreated.
    open_keyed(
        path,
        passphrase,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
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
fn tighten_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).ok();
}

#[cfg(not(unix))]
fn tighten_permissions(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::identity::{Admin, Granted};

    #[test]
    fn a_batch_shares_one_admission_and_errors_end_the_scope() -> Result<(), CliError> {
        let checks = Arc::new(AtomicUsize::new(0));
        let count = checks.clone();
        let db = Db::open_in_memory()?.with_write_authorizer(
            LedgerTarget::default(),
            move |_: &LedgerTarget, _| {
                if count.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(())
                } else {
                    Err("Writes are disabled.".into())
                }
            },
        );
        let actor = super::super::Actor::new("owner".parse()?);
        let slug = "acme".parse()?;
        let batch: Result<(), CliError> = db.in_write_batch(|| {
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                super::super::create_company(grant, &slug, &"Acme".parse()?, None)
            })?;
            Err(CliError::Validation("roll back the batch".into()))
        });
        assert!(matches!(batch, Err(CliError::Validation(_))));
        assert_eq!(checks.load(Ordering::Relaxed), 1);
        assert!(!super::super::company_exists(db.read(), &slug)?);
        assert!(matches!(
            db.in_write_batch(|| Ok(())),
            Err(CliError::WriteDenied { .. })
        ));
        assert_eq!(checks.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[test]
    fn restore_admission_cannot_authorize_an_ordinary_write() -> Result<(), CliError> {
        let db = Db::open_in_memory()?.with_write_authorizer(
            LedgerTarget::default(),
            |_: &LedgerTarget, kind| {
                if kind == LedgerWriteKind::Restore {
                    Ok(())
                } else {
                    Err("Ordinary writes are disabled.".into())
                }
            },
        );
        let actor = super::super::Actor::new("owner".parse()?);
        db.in_write_batch_for(LedgerWriteKind::Restore, || {
            let write = Granted::<Admin>::with_db(&db, &actor, |_| {
                panic!("restore admission must not reach an ordinary mutation callback")
            });
            assert!(matches!(write, Err::<(), _>(CliError::WriteDenied { .. })));
            Ok(())
        })
    }

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
        let version = stored_schema(db.read());
        assert!(version.is_ok());
        assert_eq!(version.ok(), Some(super::super::schema::CURRENT_VERSION));
    }
}
