pub mod account;
pub mod company;
pub mod export;
pub mod init;
pub mod report;
pub mod txn;
pub mod verify;

use crate::cli::{Cli, Command, require_company};
use crate::error::CliError;

/// Dispatch a parsed CLI command.
///
/// # Errors
///
/// Returns [`CliError`] for any command-level failure.
pub fn dispatch(cli: &Cli) -> Result<(), CliError> {
    match &cli.command {
        Command::Init {
            encrypt,
            path,
            force,
        } => init::run(cli, *encrypt, path.as_deref(), *force),

        Command::Verify => verify::run(cli),

        Command::Export { format, output } => export::run(cli, *format, output.as_deref()),

        Command::Company(sub) => company::run(cli, sub),

        Command::Account(sub) => {
            let company = require_company(cli)?;
            account::run(cli, &company, sub)
        }

        Command::Txn(sub) => {
            let company = require_company(cli)?;
            txn::run(cli, &company, sub)
        }

        Command::Report(sub) => {
            let company = require_company(cli)?;
            report::run(cli, &company, sub)
        }
    }
}
