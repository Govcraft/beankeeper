//! Full database snapshots, staged and verified before publication.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::cli::Cli;
use crate::db::{Db, schema};
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
    validate_destination(&cli.db, output, force)?;
    let db = cli.open_db()?;
    let directory = staging_directory(output)?;
    let staged = NamedTempFile::new_in(directory.path())?;
    let path = staged
        .path()
        .to_str()
        .ok_or_else(|| CliError::Usage("snapshot destination must be a UTF-8 path".into()))?;
    // SQLCipher inherits the main database key for VACUUM's attached database.
    // SQLite reads a consistent transaction, including committed WAL contents.
    db.conn().execute("VACUUM INTO ?1", [path])?;
    let snapshot = Db::open_snapshot(staged.path(), cli.resolve_passphrase()?)?;
    let version = super::verify::verify(&snapshot)?;
    let encrypted = is_encrypted(&snapshot);
    drop(snapshot);
    publish(staged, &cli.db, output, force)?;
    report(cli, "backup", output, version, encrypted)
}

/// Restore a full snapshot with its original passphrase and verify it in-process.
///
/// # Errors
/// Returns an error without replacing the destination if validation fails.
pub fn restore(cli: &Cli, input: &Path, force: bool) -> Result<(), CliError> {
    validate_destination(input, &cli.db, force)?;
    if !input.try_exists()? {
        return Err(CliError::NotFound(format!(
            "snapshot '{}' not found",
            input.display()
        )));
    }
    reject_sidecars(input)?;
    let directory = staging_directory(&cli.db)?;
    let mut staged = NamedTempFile::new_in(directory.path())?;
    // Copy into the private staging file, retaining its owner-only permissions.
    std::io::copy(&mut File::open(input)?, staged.as_file_mut())?;
    let snapshot = Db::open_snapshot(staged.path(), cli.resolve_passphrase()?)?;
    cli.verify_agent(&snapshot)?;
    let version = super::verify::verify(&snapshot)?;
    let encrypted = is_encrypted(&snapshot);
    drop(snapshot);
    publish(staged, input, &cli.db, force)?;
    report(cli, "restore", &cli.db, version, encrypted)
}

fn is_encrypted(db: &Db) -> bool {
    db.conn()
        .query_row("PRAGMA cipher_salt", [], |row| row.get::<_, String>(0))
        .is_ok_and(|salt| !salt.is_empty())
}

fn staging_directory(destination: &Path) -> Result<tempfile::TempDir, CliError> {
    Ok(tempfile::Builder::new()
        .prefix(".bk-snapshot-")
        .tempdir_in(parent(destination))?)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn reject_sidecars(path: &Path) -> Result<(), CliError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        if PathBuf::from(name).try_exists()? {
            return Err(CliError::Validation(format!(
                "'{}' has SQLite sidecar files; close all connections and checkpoint it before replacement or restore",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_destination(source: &Path, destination: &Path, force: bool) -> Result<(), CliError> {
    reject_source_sidecar(source, destination)?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(CliError::Validation(
                    "destination must be a regular file, not a symlink or directory".into(),
                ));
            }
            if let Ok(source_metadata) = fs::metadata(source) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if source_metadata.dev() == metadata.dev()
                        && source_metadata.ino() == metadata.ino()
                    {
                        return Err(CliError::Validation(
                            "source and destination must be different files".into(),
                        ));
                    }
                }
                #[cfg(not(unix))]
                if source_metadata.is_file()
                    && source.canonicalize()? == destination.canonicalize()?
                {
                    return Err(CliError::Validation(
                        "source and destination must be different files".into(),
                    ));
                }
            }
            if !force {
                return Err(CliError::Validation(format!(
                    "destination '{}' already exists; use --force to replace it",
                    destination.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    reject_sidecars(destination)
}

fn reject_source_sidecar(source: &Path, destination: &Path) -> Result<(), CliError> {
    let Ok(source) = source.canonicalize() else {
        return Ok(());
    };
    let destination = parent(destination).canonicalize()?.join(
        destination
            .file_name()
            .ok_or_else(|| CliError::Usage("destination must name a file".into()))?,
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = source.as_os_str().to_os_string();
        name.push(suffix);
        if destination.as_os_str() == name {
            return Err(CliError::Validation(
                "destination must not be a SQLite sidecar of the source".into(),
            ));
        }
    }
    Ok(())
}

fn publish(
    staged: NamedTempFile,
    source: &Path,
    destination: &Path,
    force: bool,
) -> Result<(), CliError> {
    // Recheck after the potentially long snapshot/verification operation.
    validate_destination(source, destination, force)?;
    staged.as_file().sync_all()?;
    if force {
        staged
            .persist(destination)
            .map_err(|error| CliError::Io(error.error))?;
    } else {
        staged
            .persist_noclobber(destination)
            .map_err(|error| CliError::Io(error.error))?;
    }
    #[cfg(unix)]
    File::open(parent(destination))?.sync_all()?;
    Ok(())
}

fn report(
    cli: &Cli,
    command: &str,
    path: &Path,
    version: i64,
    encrypted: bool,
) -> Result<(), CliError> {
    let info = SnapshotInfo {
        path,
        bytes: fs::metadata(path)?.len(),
        schema_version: version,
        encrypted,
    };
    debug_assert_eq!(version, schema::CURRENT_VERSION);
    if cli.is_json() {
        println!(
            "{}",
            crate::output::json::envelope_ok(cli.meta(command, None), info)?
        );
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] Verified {command}: {}", path.display());
    }
    Ok(())
}
