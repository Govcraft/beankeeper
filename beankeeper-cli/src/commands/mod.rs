pub mod account;
pub mod company;
pub mod export;
pub mod import_ofx;
pub mod init;
pub mod report;
pub mod txn;
pub mod verify;

use crate::cli::{
    AccountCommand, Cli, Command, CompanyCommand, ReportCommand, TxnCommand, require_company,
};
use crate::error::CliError;

/// Map a parsed `Command` to its dot-notation command name.
#[must_use]
pub fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Init { .. } => "init",
        Command::Verify => "verify",
        Command::Export { .. } => "export",
        Command::Company(sub) => match sub {
            CompanyCommand::Create { .. } => "company.create",
            CompanyCommand::List => "company.list",
            CompanyCommand::Show { .. } => "company.show",
            CompanyCommand::Delete { .. } => "company.delete",
        },
        Command::Account(sub) => match sub {
            AccountCommand::Create { .. } => "account.create",
            AccountCommand::List { .. } => "account.list",
            AccountCommand::Show { .. } => "account.show",
            AccountCommand::Delete { .. } => "account.delete",
        },
        Command::Txn(sub) => match sub.as_ref() {
            TxnCommand::Post { .. } => "txn.post",
            TxnCommand::List { .. } => "txn.list",
            TxnCommand::Show { .. } => "txn.show",
            TxnCommand::Import { .. } => "txn.import",
            TxnCommand::Attach { .. } => "txn.attach",
            TxnCommand::Reconcile => "txn.reconcile",
        },
        Command::Report(sub) => match sub {
            ReportCommand::TrialBalance { .. } => "report.trial-balance",
            ReportCommand::Balance { .. } => "report.balance",
            ReportCommand::IncomeStatement { .. } => "report.income-statement",
            ReportCommand::BalanceSheet { .. } => "report.balance-sheet",
            ReportCommand::TaxSummary { .. } => "report.tax-summary",
        },
    }
}

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
            demo,
        } => init::run(cli, *encrypt, path.as_deref(), *force, *demo),

        Command::Verify => verify::run(cli),

        Command::Export { format, output } => export::run(cli, *format, output.as_deref()),

        Command::Company(sub) => company::run(cli, sub),

        Command::Account(sub) => {
            let company = require_company(cli)?;
            account::run(cli, &company, sub)
        }

        Command::Txn(sub) => {
            // Reconcile scans all companies, so --company is not required.
            if matches!(sub.as_ref(), crate::cli::TxnCommand::Reconcile) {
                txn::run(cli, "", sub)
            } else {
                let company = require_company(cli)?;
                txn::run(cli, &company, sub)
            }
        }

        Command::Report(sub) => {
            let company = require_company(cli)?;
            report::run(cli, &company, sub)
        }
    }
}
