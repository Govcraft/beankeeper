//! Verified local snapshots for backup and host-managed schema upgrades.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use secrecy::SecretString;

use super::connection::{Db, ReadConn, open_existing_keyed, open_keyed, require_existing_file};
use super::policy::LedgerPolicy;
use super::schema::{self, SchemaVersion};
use super::verification::VerifiedLedger;
use crate::error::CliError;

pub(crate) mod files;

/// Evidence of a completed standalone upgrade and its retained backup.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LedgerUpgrade {
    from: SchemaVersion,
    to: SchemaVersion,
    #[serde(serialize_with = "serialize_backup_path")]
    backup: PathBuf,
}

fn serialize_backup_path<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&path.display())
}

impl LedgerUpgrade {
    /// Schema version preserved in the verified backup.
    #[must_use]
    pub const fn from(&self) -> SchemaVersion {
        self.from
    }

    /// Schema version reached by the upgrade.
    #[must_use]
    pub const fn to(&self) -> SchemaVersion {
        self.to
    }

    /// Local path to the retained pre-upgrade backup.
    #[must_use]
    pub fn backup(&self) -> &Path {
        &self.backup
    }
}

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

    pub(super) fn verify_migration_source(
        &self,
        tx: &Transaction<'_>,
    ) -> Result<SchemaVersion, CliError> {
        self.require_published_copy()?;
        let version = self.schema_version();
        let source = &self.source;
        let same_source = tx
            .path()
            .and_then(|path| fs::metadata(path).ok())
            .is_some_and(|metadata| same_file(&metadata, &source.metadata));
        if !same_source
            || data_version(&source.connection)? != source.data_version
            || schema::stored_schema(ReadConn::physical(tx))? != version
        {
            return Err(CliError::Database(
                "the ledger changed; take a new verified snapshot before migrating".into(),
            ));
        }
        Ok(version)
    }
}

impl Db {
    /// Migrate the ledger bound to a published, verified snapshot and open it for use.
    ///
    /// Create a fresh snapshot if another SQLite connection has written to the
    /// ledger since the copy began. Hosts must coordinate any out-of-band file
    /// moves or replacements while taking a snapshot and opening the ledger.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::Db;
    /// use std::path::Path;
    /// let _ = Db::open_after_snapshot(Path::new("books.db"), None);
    /// ```
    ///
    /// # Errors
    /// Returns an error if the snapshot or ledger has changed, opening the ledger
    /// fails, or a migration fails. The published snapshot remains available.
    pub fn open_after_snapshot(
        snapshot: VerifiedSnapshot,
        passphrase: Option<&SecretString>,
    ) -> Result<Self, CliError> {
        let path = snapshot.source.path.clone();
        let connection = open_existing_keyed(&path, passphrase)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let transaction = Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)?;
        schema::migrate_verified(transaction, snapshot)?;
        Self::finish_file_open(connection, &path, passphrase)
    }

    pub(crate) fn open_standalone(
        path: &Path,
        passphrase: Option<&SecretString>,
    ) -> Result<(Self, Option<LedgerUpgrade>), CliError> {
        match Self::open_existing(path, passphrase) {
            Err(CliError::SchemaMismatch { .. }) => {
                let source = path.canonicalize()?;
                let parent = source
                    .parent()
                    .ok_or_else(|| CliError::Usage("ledger must be in a directory".into()))?;
                let backup = tempfile::Builder::new()
                    .prefix(".bk-pre-migration-")
                    .tempdir_in(parent)?;
                let snapshot = VerifiedSnapshot::create(
                    &source,
                    passphrase,
                    &backup.path().join("books.bk-backup"),
                    SnapshotOverwrite::Refuse,
                )?;
                let from = snapshot.schema_version();
                if from == schema::current_schema_version() {
                    // Another process finished the upgrade before this snapshot began.
                    return Self::open_existing(&source, passphrase).map(|db| (db, None));
                }
                let backup_path = snapshot.path().to_owned();
                files::sync_directory(parent)?;
                // Retain the published copy even if migration fails or is interrupted.
                let _ = backup.keep();
                let db = Self::open_after_snapshot(snapshot, passphrase)?;
                let upgrade = LedgerUpgrade {
                    from,
                    to: db.schema_version(),
                    backup: backup_path,
                };
                Ok((db, Some(upgrade)))
            }
            result => result.map(|db| (db, None)),
        }
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
