//! Readiness for the selected ledger's onboarding flow.

use clap::Subcommand;
use serde::Serialize;

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::Db;
use crate::error::CliError;
use crate::operation::Operation;
use crate::output::json;

/// Ledger setup operations.
#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Read the state of a ledger before adding setup data.
    Status,
}

impl SetupCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Status => Operation::LedgerSetupStatus,
        }
    }
}

/// Persisted setup facts for the selected ledger.
#[derive(Debug, Serialize)]
pub struct SetupStatus {
    /// Number of personal or business books.
    pub companies: u64,
    /// Number of chart accounts across those books.
    pub accounts: u64,
    /// Number of posted journal transactions.
    pub transactions: u64,
    /// Whether the owner initialized the key needed to issue agent credentials.
    pub owner_key_initialized: bool,
}

/// Read setup state from an already-open ledger in one consistent query.
///
/// # Errors
/// Returns database errors if the selected ledger cannot be read.
pub fn status(db: &Db) -> Result<SetupStatus, CliError> {
    Ok(db.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM companies),
                (SELECT COUNT(*) FROM accounts),
                (SELECT COUNT(*) FROM transactions),
                EXISTS(SELECT 1 FROM identity_root WHERE singleton = 1)",
        [],
        |row| {
            let count = |index| {
                let value: i64 = row.get(index)?;
                u64::try_from(value)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
            };
            Ok(SetupStatus {
                companies: count(0)?,
                accounts: count(1)?,
                transactions: count(2)?,
                owner_key_initialized: row.get(3)?,
            })
        },
    )?)
}

pub(crate) fn run_status(cli: &Cli) -> Result<(), CliError> {
    let db = Db::open_snapshot(&cli.db, cli.resolve_passphrase()?)?;
    let _snapshot = db.conn().unchecked_transaction()?;
    cli.verify_agent(&db)?;
    let state = status(&db)?;
    match resolve_format(None, cli) {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(cli.meta(Operation::LedgerSetupStatus.name(), None), state)?
        ),
        OutputFormat::Csv => {
            println!("companies,accounts,transactions,owner_key_initialized");
            println!(
                "{},{},{},{}",
                state.companies, state.accounts, state.transactions, state.owner_key_initialized
            );
        }
        OutputFormat::Table => {
            println!("Companies: {}", state.companies);
            println!("Accounts: {}", state.accounts);
            println!("Posted transactions: {}", state.transactions);
            println!(
                "Owner key initialized: {}",
                if state.owner_key_initialized {
                    "yes"
                } else {
                    "no"
                }
            );
        }
    }
    Ok(())
}
