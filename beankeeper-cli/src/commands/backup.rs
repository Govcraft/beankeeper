//! Full database snapshots, staged and verified before publication.

use crate::operation::Operation;
use std::fs::{self, File};
use std::path::Path;

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::cli::Cli;
use crate::db::snapshot::files::{
    publish, reject_sidecars, staging_directory, validate_destination,
};
use crate::db::{Db, VerifiedSnapshot, verification::VerifiedLedger};
use crate::error::CliError;

#[derive(Serialize)]
struct SnapshotInfo<'a> {
    path: &'a Path,
    bytes: u64,
    schema_version: i64,
    encrypted: bool,
}

/// Create a consistent snapshot of the entire existing ledger.
///
/// # Errors
/// Returns an error for an unsafe destination, failed snapshot or invalid ledger.
pub fn backup(cli: &Cli, output: &Path, force: bool) -> Result<(), CliError> {
    let snapshot = VerifiedSnapshot::create_authorized(
        cli.db_path()?,
        cli.resolve_passphrase()?,
        output,
        force.into(),
        |reader, version, policy| cli.authorize_backup(reader, version, policy),
    )?;
    report(
        cli,
        Operation::Backup,
        SnapshotInfo {
            path: output,
            bytes: snapshot.bytes(),
            schema_version: snapshot.schema_version().into(),
            encrypted: snapshot.encrypted(),
        },
    )
}

/// Restore a full snapshot with its original passphrase and verify it in-process.
///
/// # Errors
/// Returns an error without replacing the destination if validation fails.
pub fn restore(cli: &Cli, input: &Path, force: bool) -> Result<(), CliError> {
    let destination = cli.explicit_db()?.ok_or_else(|| {
        CliError::Usage(
            "restore requires an explicit --db destination or named ledger target".into(),
        )
    })?;
    validate_destination(input, destination, force)?;
    if !input.try_exists()? {
        return Err(CliError::NotFound(format!(
            "snapshot '{}' not found",
            input.display()
        )));
    }
    reject_sidecars(input)?;
    let directory = staging_directory(destination)?;
    let mut staged = NamedTempFile::new_in(directory.path())?;
    // Copy into the private staging file, retaining its owner-only permissions.
    std::io::copy(&mut File::open(input)?, staged.as_file_mut())?;
    let snapshot = Db::open_snapshot(staged.path(), cli.resolve_passphrase()?)?;
    cli.verify_agent(&snapshot)?;
    let verification = VerifiedLedger::read(&snapshot)?;
    let encrypted = snapshot.has_cipher_salt();
    drop(snapshot);
    if destination.try_exists()? {
        // Inspect a private copy: even read-only SQLite connections can create
        // WAL sidecars on the original destination and prevent publication.
        // The destination has its own trust boundary, independent of the snapshot.
        let mut destination_copy = NamedTempFile::new_in(directory.path())?;
        std::io::copy(
            &mut File::open(destination)?,
            destination_copy.as_file_mut(),
        )?;
        let existing = Db::open_snapshot(destination_copy.path(), cli.resolve_passphrase()?)
            .or_else(|error| Db::open_snapshot(destination_copy.path(), None).map_err(|_| error))?;
        if crate::db::policy::get(existing.read())?.enforce {
            return Err(CliError::Unauthorized("cannot overwrite an enforced ledger; disable enforcement with ledger/admin before restore".into()));
        }
    }
    publish(staged, input, destination, force)?;
    report(
        cli,
        Operation::Restore,
        SnapshotInfo {
            path: destination,
            bytes: fs::metadata(destination)?.len(),
            schema_version: verification.schema_version(),
            encrypted,
        },
    )
}

fn report(cli: &Cli, command: Operation, info: SnapshotInfo<'_>) -> Result<(), CliError> {
    let path = info.path;
    if cli.is_json() {
        println!(
            "{}",
            crate::output::json::envelope_ok(cli.meta(command, None), info)?
        );
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] Verified {}: {}", command.name(), path.display());
    }
    Ok(())
}
