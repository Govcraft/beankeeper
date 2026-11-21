//! Confirmation required before permanent ledger deletions.

use std::io::IsTerminal;

use crate::error::CliError;

/// Proof that a destructive action was confirmed.
pub struct Confirmed(());

impl Confirmed {
    /// Check that a noninteractive caller supplied explicit confirmation.
    ///
    /// # Errors
    /// Returns a usage error if confirmation was not supplied.
    pub fn forced(force: bool) -> Result<Self, CliError> {
        if force {
            Ok(Self(()))
        } else {
            Err(CliError::Usage(
                "use --force to confirm deletion when stdin is not a terminal".into(),
            ))
        }
    }
}

/// Confirm deletion with `--force` or an affirmative answer on a terminal.
///
/// # Errors
/// Returns a usage error without a terminal or `--force`, or an input error
/// if the terminal could not supply an answer.
pub fn confirm_deletion(force: bool, target: &str) -> Result<Option<Confirmed>, CliError> {
    let stdin = std::io::stdin();
    if force || !stdin.is_terminal() {
        return Confirmed::forced(force).map(Some);
    }
    eprint!("Delete {target}? [y/N] ");
    let mut answer = String::new();
    stdin.read_line(&mut answer)?;
    Ok(answer
        .trim()
        .eq_ignore_ascii_case("y")
        .then_some(Confirmed(())))
}
