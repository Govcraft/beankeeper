//! Audited, reversible replacement of the bookkeeping rows in a live ledger.

mod tables;

use std::{
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use beankeeper::types::Sha256Hex;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{OptionalExtension, params, params_from_iter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links, grant::RestoreAdmin,
};
use crate::{
    db::{
        Actor, Db, SnapshotOverwrite, VerifiedSnapshot,
        connection::ReadConn,
        current_schema_version,
        ids::{AuditRowId, ReceiptId},
        schema::HASH_CHAIN_SCHEMA_VERSION,
        snapshot::{files, same_file},
        verification::VerifiedLedger,
    },
    error::CliError,
    identity::Granted,
    operation::Operation,
};

/// A path supplied for a standalone backup on the machine serving the ledger.
///
/// File identity and compatibility are established only inside an Admin write
/// scope, since the path can be changed after parsing.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(try_from = "PathBuf")]
pub struct BackupPath(PathBuf);

impl TryFrom<PathBuf> for BackupPath {
    type Error = CliError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.file_name().is_none() {
            return Err(CliError::Validation("backup path must name a file".into()));
        }
        Ok(Self(path))
    }
}

impl AsRef<Path> for BackupPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A ledger-wide restore or an undo of a recorded restore.
pub enum RestoreRequest {
    /// Restore the books from a standalone backup.
    Backup(BackupPath),
    /// Restore the books that preceded a recorded restore.
    Undo(ReceiptId),
}

/// A restore destination whose existence was checked before opening it.
pub(crate) struct ExistingRestoreTarget<'a>(&'a Path);

impl<'a> TryFrom<(&'a Path, &RestoreRequest, Option<&Path>)> for ExistingRestoreTarget<'a> {
    type Error = CliError;

    fn try_from(
        (path, request, passphrase_file): (&'a Path, &RestoreRequest, Option<&Path>),
    ) -> Result<Self, Self::Error> {
        if path.try_exists()? {
            return Ok(Self(path));
        }
        let destination = shell_path(path);
        let error = match request {
            RestoreRequest::Backup(input) => {
                let input = shell_path(input.as_ref());
                let passphrase = passphrase_file.map_or_else(String::new, |file| {
                    format!(" --passphrase-file {}", shell_path(file))
                });
                CliError::Usage(format!(
                    "restore destination {destination} does not exist. Recover to a new ledger with `bk --db {destination} recover --input {input}{passphrase}`."
                ))
            }
            RestoreRequest::Undo(_) => CliError::NotFound(format!(
                "restore destination {destination} does not exist. Select the ledger that issued the undo receipt."
            )),
        };
        Err(error)
    }
}

impl AsRef<Path> for ExistingRestoreTarget<'_> {
    fn as_ref(&self) -> &Path {
        self.0
    }
}

fn shell_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_encoded_bytes();
    // A quoted leading hyphen can still be interpreted as a flag by the CLI.
    let prefix = if bytes.starts_with(b"-") { "./" } else { "" };
    if let Some(path) = path.to_str() {
        format!("'{prefix}{}'", path.replace('\'', "'\\''"))
    } else {
        let escaped: String = bytes
            .iter()
            .copied()
            .flat_map(std::ascii::escape_default)
            .map(char::from)
            .collect();
        format!("$'{prefix}{escaped}'")
    }
}

impl RestoreRequest {
    /// Operation used by the CLI and MCP authorization catalogs.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Backup(_) => Operation::Restore,
            Self::Undo(_) => Operation::RestoreUndo,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<RestoreReceipt, CliError> {
        Granted::<RestoreAdmin>::with_db(db, actor, |grant| {
            grant.mutate("sp_restore_books", |tx, actor| {
                let ledger = db.file_path().ok_or_else(|| {
                    CliError::Validation("restore requires a file-backed ledger".into())
                })?;
                let input = RestoreInput::resolve(self, tx.read(), &ledger)?;
                let undo = PendingUndo::new(db, &ledger)?;
                let backup = CompatibleBackup::open(db, &ledger, &input)?;
                tables::replace(tx, &backup)?;
                let (undo_of, summary) = match input {
                    RestoreInput::Backup { .. } => {
                        (None, "Restored the books from the chosen backup.")
                    }
                    RestoreInput::Undo { receipt, .. } => (
                        Some(receipt),
                        "Restored the books from before the previous restore.",
                    ),
                };
                let event = RestoreEvent {
                    backup_id: SnapshotDigest(backup.digest),
                    source_audit_id: backup.source_audit_id,
                    undo: undo.reference.clone(),
                    undo_of,
                };
                let after = serde_json::to_string(&event).map_err(invalid_event)?;
                let restore_id = append_with_links(
                    tx.connection(),
                    actor,
                    AuditChange {
                        company_slug: None,
                        entity: AuditEntity::Ledger,
                        entity_id: event.backup_id.0.as_str(),
                        action: AuditAction::LedgerRestored,
                        snapshots: (None, Some(&after)),
                    },
                    AuditLinks::None,
                )?;
                let verification =
                    VerifiedLedger::verify_at(tx.read(), current_schema_version(), |_| Ok(()))?;
                undo.keep();
                Ok(RestoreReceipt {
                    restore_id,
                    undo_id: restore_id,
                    summary,
                    event,
                    verification,
                })
            })
        })
    }
}

/// The committed restore receipt and the ID to pass to the undo operation.
#[derive(Debug, Serialize)]
pub struct RestoreReceipt {
    restore_id: ReceiptId,
    undo_id: ReceiptId,
    summary: &'static str,
    #[serde(flatten)]
    event: RestoreEvent,
    verification: VerifiedLedger,
}

impl RestoreReceipt {
    /// The audit receipt that committed this change.
    #[must_use]
    pub const fn restore_id(&self) -> ReceiptId {
        self.restore_id
    }

    /// Plain-language summary of the change.
    #[must_use]
    pub const fn summary(&self) -> &'static str {
        self.summary
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RestoreEvent {
    // The SHA-256 is also the content address of the chosen backup.
    backup_id: SnapshotDigest,
    source_audit_id: Option<AuditRowId>,
    undo: UndoReference,
    undo_of: Option<ReceiptId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UndoReference {
    snapshot_id: UndoSnapshotId,
    sha256: SnapshotDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct SnapshotDigest(Sha256Hex);

impl TryFrom<String> for SnapshotDigest {
    type Error = beankeeper::types::Sha256HexError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Sha256Hex::new(value).map(Self)
    }
}

impl From<SnapshotDigest> for String {
    fn from(digest: SnapshotDigest) -> Self {
        digest.0.to_string()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct UndoSnapshotId(MagicTypeId);

impl UndoSnapshotId {
    const PREFIX: &str = "bkundo";

    fn generate() -> Self {
        Self(Self::PREFIX.create_type_id::<V7>())
    }

    fn path(&self, ledger: &Path) -> PathBuf {
        ledger.with_file_name(format!(".{self}.bk-backup"))
    }
}

impl fmt::Display for UndoSnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for UndoSnapshotId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value).map_err(invalid_event)?;
        if id.prefix_str().as_deref() != Ok(Self::PREFIX) {
            return Err(invalid_event("invalid undo snapshot ID prefix"));
        }
        Ok(Self(id))
    }
}

impl TryFrom<String> for UndoSnapshotId {
    type Error = CliError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<UndoSnapshotId> for String {
    fn from(id: UndoSnapshotId) -> Self {
        id.to_string()
    }
}

enum RestoreInput {
    Backup {
        path: BackupPath,
    },
    Undo {
        receipt: ReceiptId,
        path: PathBuf,
        digest: SnapshotDigest,
    },
}

impl RestoreInput {
    fn resolve(
        request: RestoreRequest,
        reader: ReadConn<'_>,
        ledger: &Path,
    ) -> Result<Self, CliError> {
        let receipt = match request {
            RestoreRequest::Undo(receipt) => receipt,
            RestoreRequest::Backup(path) => return Ok(Self::Backup { path }),
        };
        let event: String = reader
            .query_row(
                "SELECT after FROM audit_log WHERE id = ?1 AND action = ?2",
                params![receipt, AuditAction::LedgerRestored.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CliError::NotFound(format!("restore receipt #{receipt} not found")))?;
        let event: RestoreEvent = serde_json::from_str(&event).map_err(invalid_event)?;
        // Only known non-bookkeeping entities can be ignored when checking
        // for later events in the physical chain.
        let ignored: Vec<_> = AuditEntity::ALL
            .iter()
            .copied()
            .filter(|entity| !entity.affects_books())
            .map(AuditEntity::as_str)
            .collect();
        let placeholders = vec!["?"; ignored.len()].join(", ");
        let latest: ReceiptId = reader.query_row(
            &format!("SELECT MAX(id) FROM audit_log WHERE entity NOT IN ({placeholders})"),
            params_from_iter(ignored),
            |row| row.get(0),
        )?;
        if latest != receipt {
            return Err(CliError::Validation(format!(
                "restore receipt #{receipt} is superseded by a later bookkeeping change"
            )));
        }
        Ok(Self::Undo {
            receipt,
            path: event.undo.snapshot_id.path(ledger),
            digest: event.undo.sha256,
        })
    }

    fn verify_digest(&self, actual: &Sha256Hex) -> Result<(), CliError> {
        match self {
            Self::Backup { .. } => Ok(()),
            Self::Undo { digest, .. } if &digest.0 == actual => Ok(()),
            Self::Undo { .. } => Err(CliError::Validation(
                "undo snapshot does not match its recorded SHA-256".into(),
            )),
        }
    }
}

impl AsRef<Path> for RestoreInput {
    fn as_ref(&self) -> &Path {
        match self {
            Self::Backup { path } => path.as_ref(),
            Self::Undo { path, .. } => path,
        }
    }
}

fn invalid_event(error: impl fmt::Display) -> CliError {
    CliError::Database(format!("invalid restore audit event: {error}"))
}

/// A private current-schema backup paired with its verification evidence and
/// the physical audit head it shares with the authorized live ledger.
struct CompatibleBackup {
    db: Db,
    digest: Sha256Hex,
    source_audit_id: Option<AuditRowId>,
    _verification: VerifiedLedger,
    // Dropped after the connection closes, also deleting any temporary sidecars.
    _staged: tempfile::NamedTempFile,
    _directory: tempfile::TempDir,
}

impl CompatibleBackup {
    fn open(live: &Db, ledger: &Path, input: &RestoreInput) -> Result<Self, CliError> {
        let path = input.as_ref();
        let source = path.canonicalize().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                CliError::NotFound(format!("snapshot '{}' not found", path.display()))
            } else {
                error.into()
            }
        })?;
        let metadata = fs::metadata(&source)?;
        if !metadata.is_file() || same_file(&metadata, &fs::metadata(ledger)?) {
            return Err(CliError::Validation(
                "restore requires a standalone backup different from the live ledger".into(),
            ));
        }
        files::reject_sidecars(&source)?;
        let directory = files::staging_directory(ledger)?;
        let mut staged = tempfile::NamedTempFile::new_in(directory.path())?;
        let digest = copy_and_hash(&mut File::open(&source)?, staged.as_file_mut())?;
        files::reject_sidecars(&source)?;
        input.verify_digest(&digest)?;

        let key = live.snapshot_key();
        let read_only = Db::open_supported_snapshot(staged.path(), key)?;
        let version = read_only.schema_version();
        let (db, verification, source_audit_id) = if version == current_schema_version() {
            let verification = read_only
                .read_snapshot(|reader| VerifiedLedger::verify_at(reader, version, |_| Ok(())))?;
            let anchor = matching_audit_head(live.read(), &verification)?;
            (read_only, verification, anchor)
        } else {
            read_only.close()?;
            let proof = VerifiedSnapshot::create(
                staged.path(),
                key,
                &directory.path().join("before-upgrade.db"),
                SnapshotOverwrite::Refuse,
            )?;
            let has_original_chain = i64::from(version) >= HASH_CHAIN_SCHEMA_VERSION;
            let original_anchor = matching_audit_head(live.read(), proof.verification())?;
            let migrated = Db::open_after_snapshot(proof, key)?;
            let verification = VerifiedLedger::read(&migrated)?;
            let anchor = if has_original_chain {
                original_anchor
            } else {
                matching_audit_head(live.read(), &verification)?
            };
            (migrated, verification, anchor)
        };
        Ok(Self {
            db,
            digest,
            source_audit_id,
            _verification: verification,
            _staged: staged,
            _directory: directory,
        })
    }
}

fn matching_audit_head(
    live: ReadConn<'_>,
    verified: &VerifiedLedger,
) -> Result<Option<AuditRowId>, CliError> {
    let Some((id, hash)) = verified.audit_head() else {
        return Ok(None);
    };
    let matches: bool = live.query_row(
        "SELECT EXISTS(SELECT 1 FROM audit_log WHERE id = ?1 AND row_hash = ?2)",
        params![id, hash],
        |row| row.get(0),
    )?;
    if !matches {
        return Err(CliError::Validation(
            "backup audit history does not belong to this ledger".into(),
        ));
    }
    Ok(Some(id))
}

struct PendingUndo {
    snapshot: VerifiedSnapshot,
    reference: UndoReference,
    committed: bool,
}

impl PendingUndo {
    fn new(db: &Db, ledger: &Path) -> Result<Self, CliError> {
        let id = UndoSnapshotId::generate();
        let path = id.path(ledger);
        let snapshot =
            VerifiedSnapshot::create(ledger, db.snapshot_key(), &path, SnapshotOverwrite::Refuse)?;
        let sha256 = copy_and_hash(&mut File::open(snapshot.path())?, &mut io::sink())?;
        let pending = Self {
            snapshot,
            reference: UndoReference {
                snapshot_id: id,
                sha256: SnapshotDigest(sha256),
            },
            committed: false,
        };
        let current: Option<(AuditRowId, String)> = db
            .read()
            .query_row(
                "SELECT id, row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if pending.snapshot.verification().audit_head()
            != current.as_ref().map(|(id, hash)| (*id, hash.as_str()))
        {
            return Err(CliError::Database(
                "live ledger changed while taking the undo snapshot".into(),
            ));
        }
        Ok(pending)
    }

    fn keep(mut self) {
        self.committed = true;
    }
}

impl Drop for PendingUndo {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(self.snapshot.path());
        }
    }
}

fn copy_and_hash(input: &mut impl Read, output: &mut impl Write) -> Result<Sha256Hex, CliError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(Sha256Hex::from_digest(hash.finalize().into()));
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
    }
}
