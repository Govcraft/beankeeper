use std::io::Read;
use std::path::Path;
use std::process;

use secrecy::SecretString;

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
    if let Ok(cmd) = std::env::var("BEANKEEPER_PASSPHRASE_CMD") {
        if !cmd.is_empty() {
            let secret = run_passphrase_command(&cmd)?;
            return Ok(Some(secret));
        }
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

    let mut buf = String::new();
    std::fs::File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut buf))
        .map_err(|e| CliError::General(format!("failed to read from fd {fd}: {e}")))?;

    let trimmed = buf.trim_end_matches('\n').to_owned();
    if trimmed.is_empty() {
        return Err(CliError::Usage(format!(
            "passphrase from fd {fd} is empty"
        )));
    }
    Ok(SecretString::from(trimmed))
}

#[cfg(not(unix))]
fn read_from_fd(fd: i32) -> Result<SecretString, CliError> {
    Err(CliError::Usage(format!(
        "--passphrase-fd is only supported on Unix (got fd {fd})"
    )))
}

/// Read a passphrase from a file, trimming the trailing newline.
fn read_from_file(path: &Path) -> Result<SecretString, CliError> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| CliError::General(format!("failed to read passphrase file {}: {e}", path.display())))?;

    let trimmed = contents.trim_end_matches('\n').to_owned();
    if trimmed.is_empty() {
        return Err(CliError::Usage(format!(
            "passphrase file {} is empty",
            path.display()
        )));
    }
    Ok(SecretString::from(trimmed))
}

/// Execute a shell command and capture its stdout as the passphrase.
fn run_passphrase_command(cmd: &str) -> Result<SecretString, CliError> {
    let output = process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::inherit())
        .output()
        .map_err(|e| {
            CliError::General(format!("failed to execute passphrase command: {e}"))
        })?;

    if !output.status.success() {
        return Err(CliError::General(format!(
            "passphrase command exited with status {}",
            output.status
        )));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| CliError::General(format!("passphrase command output is not UTF-8: {e}")))?;

    let trimmed = stdout.trim_end_matches('\n').to_owned();
    if trimmed.is_empty() {
        return Err(CliError::Usage(
            "passphrase command produced empty output".into(),
        ));
    }
    Ok(SecretString::from(trimmed))
}

/// Prompt for a passphrase on the TTY.
fn prompt_passphrase(prompt: &str) -> Result<SecretString, CliError> {
    let pass = rpassword::prompt_password(prompt)
        .map_err(CliError::Io)?;

    if pass.is_empty() {
        return Err(CliError::Usage("passphrase cannot be empty".into()));
    }
    Ok(SecretString::from(pass))
}
