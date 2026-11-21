use std::io::{self, Read};
use std::path::Path;
use std::process;

use secrecy::{SecretString, zeroize::Zeroizing};

use crate::error::CliError;

/// Resolve a passphrase using the configured sources.
///
/// Resolution order:
/// 1. `--passphrase-fd <fd>` (Unix only) - read from file descriptor
/// 2. `--passphrase-file <path>` - read file contents, trim trailing newline
/// 3. `BEANKEEPER_PASSPHRASE_CMD` env var - run command, capture stdout
/// 4. TTY prompt via `rpassword`
///
/// Returns `None` if no passphrase source is configured (unencrypted database).
///
/// # Errors
///
/// Returns [`CliError`] if a configured source fails to provide a passphrase.
pub fn resolve_passphrase(
    passphrase_fd: Option<i32>,
    passphrase_file: Option<&Path>,
    prompt: bool,
) -> Result<Option<SecretString>, CliError> {
    // 1. --passphrase-fd
    if let Some(fd) = passphrase_fd {
        let secret = read_from_fd(fd)?;
        return Ok(Some(secret));
    }

    // 2. --passphrase-file
    if let Some(path) = passphrase_file {
        let secret = read_from_file(path)?;
        return Ok(Some(secret));
    }

    // 3. BEANKEEPER_PASSPHRASE_CMD
    if let Ok(cmd) = std::env::var("BEANKEEPER_PASSPHRASE_CMD")
        && !cmd.is_empty()
    {
        let secret = run_passphrase_command(&cmd)?;
        return Ok(Some(secret));
    }

    // 4. TTY prompt
    if prompt {
        let secret = prompt_passphrase("Passphrase: ")?;
        return Ok(Some(secret));
    }

    Ok(None)
}

/// Prompt for a passphrase twice (with confirmation) for initial encryption setup.
///
/// # Errors
///
/// Returns [`CliError::Validation`] if the two entries do not match.
/// Returns [`CliError::Io`] if the TTY read fails.
pub fn prompt_new_passphrase() -> Result<SecretString, CliError> {
    use secrecy::ExposeSecret;

    let first = prompt_passphrase("Enter passphrase: ")?;
    let confirm = prompt_passphrase("Confirm passphrase: ")?;

    if first.expose_secret() != confirm.expose_secret() {
        return Err(CliError::Validation("passphrases do not match".into()));
    }

    Ok(first)
}

/// Read a passphrase from a file descriptor (Unix only).
///
/// Uses `/proc/self/fd/<N>` to open the descriptor without `unsafe` code.
/// Falls back to `/dev/fd/<N>` if `/proc` is unavailable.
#[cfg(unix)]
fn read_from_fd(fd: i32) -> Result<SecretString, CliError> {
    let proc_path = format!("/proc/self/fd/{fd}");
    let dev_path = format!("/dev/fd/{fd}");

    let path = if Path::new(&proc_path).exists() {
        proc_path
    } else if Path::new(&dev_path).exists() {
        dev_path
    } else {
        return Err(CliError::General(format!(
            "cannot access file descriptor {fd}: neither /proc/self/fd/{fd} nor /dev/fd/{fd} exists"
        )));
    };

    let contents = std::fs::File::open(&path)
        .and_then(read_secret)
        .map_err(|e| CliError::General(format!("failed to read from fd {fd}: {e}")))?;

    nonempty_secret(contents.trim_end_matches('\n'), || {
        format!("passphrase from fd {fd} is empty")
    })
}

#[cfg(not(unix))]
fn read_from_fd(fd: i32) -> Result<SecretString, CliError> {
    Err(CliError::Usage(format!(
        "--passphrase-fd is only supported on Unix (got fd {fd})"
    )))
}

/// Read a passphrase from a file, trimming the trailing newline.
fn read_from_file(path: &Path) -> Result<SecretString, CliError> {
    let contents = std::fs::File::open(path)
        .and_then(read_secret)
        .map_err(|e| {
            CliError::General(format!(
                "failed to read passphrase file {}: {e}",
                path.display()
            ))
        })?;

    nonempty_secret(contents.trim_end_matches('\n'), || {
        format!("passphrase file {} is empty", path.display())
    })
}

/// Execute a shell command and capture its stdout as the passphrase.
fn run_passphrase_command(cmd: &str) -> Result<SecretString, CliError> {
    let mut child = process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::inherit())
        .spawn()
        .map_err(|e| CliError::General(format!("failed to execute passphrase command: {e}")))?;

    let contents = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stdout pipe unavailable"))
        .and_then(read_secret);
    let status = child
        .wait()
        .map_err(|e| CliError::General(format!("failed to wait for passphrase command: {e}")))?;
    if !status.success() {
        return Err(CliError::General(format!(
            "passphrase command exited with status {status}"
        )));
    }

    let contents = contents
        .map_err(|e| CliError::General(format!("failed to read passphrase command output: {e}")))?;

    nonempty_secret(contents.trim_end_matches('\n'), || {
        "passphrase command produced empty output".into()
    })
}

/// Prompt for a passphrase on the TTY.
fn prompt_passphrase(prompt: &str) -> Result<SecretString, CliError> {
    // rpassword zeroizes its input buffer and transfers ownership of the result.
    let pass = Zeroizing::new(rpassword::prompt_password(prompt).map_err(CliError::Io)?);
    nonempty_secret(&pass, || "passphrase cannot be empty".into())
}

fn read_secret(mut reader: impl Read) -> io::Result<Zeroizing<String>> {
    let mut contents = Zeroizing::new(String::new());
    reader.read_to_string(&mut contents)?;
    Ok(contents)
}

fn nonempty_secret(pass: &str, empty: impl FnOnce() -> String) -> Result<SecretString, CliError> {
    if pass.is_empty() {
        return Err(CliError::Usage(empty()));
    }
    Ok(SecretString::from(pass))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn passphrase_file_trims_only_trailing_newlines() -> Result<(), CliError> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(b" \nsecret \n\n")?;
        assert_eq!(read_from_file(file.path())?.expose_secret(), " \nsecret ");
        Ok(())
    }

    #[test]
    fn empty_and_non_utf8_passphrase_files_are_rejected() -> Result<(), CliError> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"\n\n")?;
        assert!(matches!(
            read_from_file(file.path()),
            Err(CliError::Usage(_))
        ));
        std::fs::write(file.path(), [0xff])?;
        assert!(matches!(
            read_from_file(file.path()),
            Err(CliError::General(_))
        ));
        Ok(())
    }
}
