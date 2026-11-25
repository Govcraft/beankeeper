//! Verified local snapshots for backup and host-managed schema upgrades.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use secrecy::SecretString;

use super::connection::{Db, ReadConn, open_keyed, require_existing_file, tighten_permissions};
use super::policy::LedgerPolicy;
use super::schema::{self, SchemaVersion};
use super::verification::VerifiedLedger;
use crate::error::CliError;

pub(crate) mod files;

/// Whether publication may replace an existing backup file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnapshotOverwrite {
    /// Publish only if the destination does not exist.
    #[default]
    Refuse,
    /// Replace an existing regular backup file.
    Replace,
}

impl From<bool> for SnapshotOverwrite {
    fn from(force: bool) -> Self {
        if force { Self::Replace } else { Self::Refuse }
    }
}

/// Proof that a readable, verified snapshot of a specific ledger was published.
///
/// A host can consume this proof with [`Db::open_after_snapshot`]. The proof
/// stays in-process and is bound to the source connection used to take the copy.
///
/// ```compile_fail
/// use beankeeper_cli::db::VerifiedSnapshot;
/// let forged: VerifiedSnapshot = Default::default();
/// ```
pub struct VerifiedSnapshot {
    source: SnapshotSource,
    path: PathBuf,
    metadata: Metadata,
    encrypted: bool,
    verification: VerifiedLedger,
}

struct SnapshotSource {
    path: PathBuf,
    connection: Connection,
    metadata: Metadata,
    data_version: i64,
}

impl SnapshotSource {
    fn open(path: &Path, passphrase: Option<&SecretString>) -> Result<Self, CliError> {
        require_existing_file(path)?;
        let path = path.canonicalize()?;
        let metadata = fs::metadata(&path)?;
        let connection = open_keyed(
            &path,
            passphrase,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let data_version = data_version(&connection)?;
        Ok(Self {
            path,
            connection,
            metadata,
            data_version,
        })
    }

    fn copy_into(&self, path: &Path) -> Result<(), CliError> {
        let path = path
            .to_str()
            .ok_or_else(|| CliError::Usage("snapshot destination must be a UTF-8 path".into()))?;
        // SQLCipher inherits the key, and VACUUM reads committed WAL pages.
        self.connection.execute("VACUUM INTO ?1", [path])?;
        Ok(())
    }
}

impl VerifiedSnapshot {
    /// Copy an existing ledger read-only, verify it with its original passphrase,
    /// and publish it atomically. Existing ledgers need no schema upgrade first.
    ///
    /// This storage API is for trusted hosts that already control the files.
    /// # Errors
    /// Returns an error if the source, destination, or copied ledger fails validation.
    pub fn create(
        source: &Path,
        passphrase: Option<&SecretString>,
        destination: &Path,
        overwrite: SnapshotOverwrite,
    ) -> Result<Self, CliError> {
        Self::create_authorized(source, passphrase, destination, overwrite, |_, _, _| Ok(()))
    }

    pub(crate) fn create_authorized(
        source: &Path,
        passphrase: Option<&SecretString>,
        destination: &Path,
        overwrite: SnapshotOverwrite,
        authorize: impl FnOnce(
            ReadConn<'_>,
            SchemaVersion,
            Option<&LedgerPolicy>,
        ) -> Result<(), CliError>,
    ) -> Result<Self, CliError> {
        let source = SnapshotSource::open(source, passphrase)?;
        files::validate_destination(&source.path, destination, overwrite)?;
        let path = files::absolute_destination(destination)?;
        let directory = files::staging_directory(&path)?;
        let staged = tempfile::NamedTempFile::new_in(directory.path())?;
        source.copy_into(staged.path())?;
        let snapshot = Db::open_supported_snapshot(staged.path(), passphrase)?;
        let version = snapshot.schema_version();
        let (verification, encrypted) = snapshot.read_snapshot(|reader| {
            let verification = VerifiedLedger::verify_at(reader, version, |policy| {
                authorize(reader, version, policy)
            })?;
            Ok((verification, snapshot.has_cipher_salt()))
        })?;
        snapshot.close()?;
        let metadata = staged.as_file().metadata()?;
        files::publish(staged, &source.path, &path, overwrite)?;
        Ok(Self {
            source,
            path,
            metadata,
            encrypted,
            verification,
        })
    }

    /// Absolute path of the published copy.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Schema version captured in the copy.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.verification.schema_version()
    }

    /// Size of the published copy in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.metadata.len()
    }

    /// Whether the copy is encrypted.
    #[must_use]
    pub const fn encrypted(&self) -> bool {
        self.encrypted
    }

    /// Ledger verification evidence from before publication.
    #[must_use]
    pub const fn verification(&self) -> &VerifiedLedger {
        &self.verification
    }

    fn require_published_copy(&self) -> Result<(), CliError> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_file()
            || !same_file(&metadata, &self.metadata)
            || metadata.len() != self.metadata.len()
            || metadata.modified()? != self.metadata.modified()?
        {
            return Err(CliError::Database(
                "the verified snapshot changed; take a new snapshot before migrating".into(),
            ));
        }
        Ok(())
    }
}

impl Db {
    /// Migrate the ledger bound to a published, verified snapshot and open it for use.
    ///
    /// Create a fresh snapshot if another SQLite connection has written to the
    /// ledger since the copy began. Hosts must coordinate any out-of-band file
    /// moves or replacements while taking a snapshot and opening the ledger.
    /// # Errors
    /// Returns an error if the snapshot or ledger has changed, opening the ledger
    /// fails, or a migration fails. The published snapshot remains available.
    pub fn open_after_snapshot(
        snapshot: VerifiedSnapshot,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        snapshot.require_published_copy()?;
        let version = snapshot.schema_version();
        let source = snapshot.source;
        let connection = open_keyed(
            &source.path,
            passphrase,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let transaction = Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)?;
        if !same_file(&fs::metadata(&source.path)?, &source.metadata)
            || data_version(&source.connection)? != source.data_version
            || schema::get_schema_version(ReadConn::physical(&transaction))? != i64::from(version)
        {
            return Err(CliError::Database(
                "the ledger changed; take a new verified snapshot before migrating".into(),
            ));
        }
        schema::migrate_verified(&transaction, version)?;
        transaction.commit()?;

        source.connection.close().map_err(|(_, error)| error)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        tighten_permissions(&source.path);
        Ok(Self::from_keyed_connection(
            connection,
            passphrase,
            schema::current_schema_version(),
        ))
    }
}

fn data_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.pragma_query_value(None, "data_version", |row| row.get(0))
}

#[cfg(unix)]
pub(super) fn same_file(first: &Metadata, second: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
}

#[cfg(not(unix))]
pub(super) fn same_file(first: &Metadata, second: &Metadata) -> bool {
    first
        .created()
        .is_ok_and(|created| second.created().is_ok_and(|other| other == created))
}
