use std::fs::{self, File};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

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

pub(crate) fn reject_sidecars(path: &Path) -> Result<(), CliError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        if PathBuf::from(name).try_exists()? {
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
    force: bool,
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
    let destination = absolute_destination(destination)?;
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
