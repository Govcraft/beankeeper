use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use super::SnapshotOverwrite;
use crate::error::CliError;

pub(crate) fn staging_directory(destination: &Path) -> Result<tempfile::TempDir, CliError> {
    Ok(tempfile::Builder::new()
        .prefix(".bk-snapshot-")
        .tempdir_in(parent(destination))?)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sidecar_paths(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    const SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];
    SUFFIXES.into_iter().map(|suffix| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

pub(crate) fn reject_sidecars(path: &Path) -> Result<(), CliError> {
    for sidecar in sidecar_paths(path) {
        if sidecar.try_exists()? {
            return Err(CliError::Validation(format!(
                "'{}' has SQLite sidecar files; close all connections and checkpoint it first",
                path.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_destination(
    source: &Path,
    destination: &Path,
    overwrite: SnapshotOverwrite,
) -> Result<(), CliError> {
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
            match overwrite {
                SnapshotOverwrite::Refuse => {
                    return Err(CliError::Validation(format!(
                        "destination '{}' already exists; use --force to replace it",
                        destination.display()
                    )));
                }
                SnapshotOverwrite::Replace => {}
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
    let destination = absolute_destination(destination)?;
    if sidecar_paths(&source).any(|sidecar| destination == sidecar) {
        return Err(CliError::Validation(
            "destination must not be a SQLite sidecar of the source".into(),
        ));
    }
    Ok(())
}

pub(super) fn absolute_destination(path: &Path) -> Result<PathBuf, CliError> {
    let name = path
        .file_name()
        .ok_or_else(|| CliError::Usage("destination must name a file".into()))?;
    Ok(parent(path).canonicalize()?.join(name))
}

pub(crate) fn publish(
    staged: NamedTempFile,
    source: &Path,
    destination: &Path,
    overwrite: SnapshotOverwrite,
) -> Result<(), CliError> {
    // Recheck after the potentially long snapshot/verification operation.
    validate_destination(source, destination, overwrite)?;
    staged.as_file().sync_all()?;
    match overwrite {
        SnapshotOverwrite::Refuse => staged.persist_noclobber(destination),
        SnapshotOverwrite::Replace => staged.persist(destination),
    }
    .map_err(|error| CliError::Io(error.error))?;
    sync_directory(parent(destination))
}

pub(super) fn sync_directory(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
