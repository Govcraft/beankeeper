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
use crate::db::{
    Db, SchemaVersion, SnapshotOverwrite, VerifiedSnapshot,
    audit::restore::{ExistingRestoreTarget, RestoreRequest},
    verification::VerifiedLedger,
};
use crate::error::CliError;

#[derive(Serialize)]
struct SnapshotInfo<'a> {
    path: &'a Path,
    bytes: u64,
    schema_version: SchemaVersion,
    encrypted: bool,
}

/// Create a consistent snapshot of the entire existing ledger.
///
/// # Errors
/// Returns an error for an unsafe destination, failed snapshot or invalid ledger.
pub fn backup(cli: &Cli, output: &Path, overwrite: SnapshotOverwrite) -> Result<(), CliError> {
    let path = cli.command_db_path()?;
    let passphrase = if let Some(db) = cli.prepared_ledger() {
        db.snapshot_key()
    } else {
        cli.resolve_passphrase()?
    };
    let snapshot = VerifiedSnapshot::create_authorized(
        &path,
        passphrase,
        output,
        overwrite,
        |reader, version, policy| cli.authorize_backup(reader, version, policy),
    )?;
    report(
        cli,
        Operation::Backup,
        SnapshotInfo {
            path: output,
            bytes: snapshot.bytes(),
            schema_version: snapshot.schema_version(),
            encrypted: snapshot.encrypted(),
        },
    )
}

/// Recover a full snapshot to a new file with its original passphrase.
///
/// # Errors
/// Returns an error without replacing the destination if validation fails.
pub fn recover(cli: &Cli, input: &Path) -> Result<(), CliError> {
    let overwrite = SnapshotOverwrite::Refuse;
    let destination = cli.explicit_db()?.ok_or_else(|| {
        CliError::Usage(
            "recover requires an explicit --db destination or named ledger target".into(),
        )
    })?;
    if destination.try_exists()? {
        return Err(CliError::Validation(
            "recovery requires a new destination; use restore for an existing ledger".into(),
        ));
    }
    validate_destination(input, destination, overwrite)?;
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
    let passphrase = cli.resolve_passphrase()?;
    let snapshot = Db::open_snapshot(staged.path(), passphrase)?;
    cli.verify_agent(&snapshot)?;
    let verification = VerifiedLedger::read(&snapshot)?;
    let encrypted = snapshot.has_cipher_salt();
    drop(snapshot);
    crate::db::audit::disbursements::guard_recovered_file(
        staged.path(),
        passphrase,
        verification.schema_version(),
    )?;
    publish(staged, input, destination, overwrite)?;
    report(
        cli,
        Operation::Recover,
        SnapshotInfo {
            path: destination,
            bytes: fs::metadata(destination)?.len(),
            schema_version: verification.schema_version(),
            encrypted,
        },
    )
}

/// Run the same audited restore request served by MCP.
///
/// # Errors
/// Returns errors for an unselected ledger, failed authorization or a failed restore.
pub fn restore(cli: &Cli, request: RestoreRequest) -> Result<(), CliError> {
    let opened;
    let db = if let Some(db) = cli.prepared_ledger() {
        db
    } else {
        let destination = cli.explicit_db()?.ok_or_else(|| {
            CliError::Usage(
                "restore requires an explicit --db destination or named ledger target".into(),
            )
        })?;
        let target = ExistingRestoreTarget::try_from((
            destination,
            &request,
            cli.passphrase.passphrase_file.as_deref(),
        ))?;
        opened = Db::open_for_restore(&target, cli.resolve_passphrase()?)?;
        &opened
    };
    let request = crate::request::LedgerRequest::Restore(request);
    request
        .execute_with_actor(db, |_| {
            cli.verify_agent(db)?;
            Ok(cli.principal())
        })?
        .0
        .render(cli, None)
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
