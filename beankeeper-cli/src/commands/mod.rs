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
use crate::output::json::Meta;

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
        Command::Account(args) => match &args.command {
            AccountCommand::Create { .. } => "account.create",
            AccountCommand::List { .. } => "account.list",
            AccountCommand::Show { .. } => "account.show",
            AccountCommand::Delete { .. } => "account.delete",
        },
        Command::Txn(args) => match args.command.as_ref() {
            TxnCommand::Post { .. } => "txn.post",
            TxnCommand::List { .. } => "txn.list",
            TxnCommand::Show { .. } => "txn.show",
            TxnCommand::Import { .. } => "txn.import",
            TxnCommand::Attach { .. } => "txn.attach",
            TxnCommand::Clear { .. } => "txn.clear",
            TxnCommand::Reconcile => "txn.reconcile",
        },
        Command::Report(args) => match &args.command {
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
pub fn dispatch(cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
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

        Command::Account(args) => {
            let company = require_company(cli)?;
            account::run(cli, &company, &args.command)
        }

        Command::Txn(args) => {
            // Reconcile scans all companies, so --company is not required.
            if matches!(args.command.as_ref(), crate::cli::TxnCommand::Reconcile) {
                txn::run(cli, "", args.command.as_ref(), meta)
            } else {
                let company = require_company(cli)?;
                txn::run(cli, &company, args.command.as_ref(), meta)
            }
        }

        Command::Report(args) => {
            let company = require_company(cli)?;
            report::run(cli, &company, &args.command)
        }
    }
}
