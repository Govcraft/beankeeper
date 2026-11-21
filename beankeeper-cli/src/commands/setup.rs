//! Readiness for the selected ledger's onboarding flow.

use clap::Subcommand;
use serde::Serialize;

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    Actor, Db,
    audit::setup::{self, End, Generation},
};
use crate::error::CliError;
use crate::identity;
use crate::operation::Operation;
use crate::output::json;

pub(crate) const DEFAULT_TTL: &str = "15m";

/// Ledger setup operations.
#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Read the state of a ledger before adding setup data.
    Status,
    /// Issue the agent credential for one new setup generation.
    Begin {
        /// Name for the setup agent.
        name: String,
        /// Base64 Ed25519 public key of the setup agent.
        #[arg(long)]
        public_key: String,
        /// Credential lifetime, at most one hour.
        #[arg(long, default_value = DEFAULT_TTL)]
        ttl: String,
    },
    /// Complete a setup generation and deactivate its credential.
    Finish {
        /// Setup generation returned by begin.
        generation: Generation,
    },
    /// Cancel a setup generation and deactivate its credential.
    Revoke {
        /// Setup generation returned by begin.
        generation: Generation,
    },
}

impl SetupCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Status => Operation::LedgerSetupStatus,
            Self::Begin { .. } => Operation::LedgerSetupBegin,
            Self::Finish { .. } => Operation::LedgerSetupFinish,
            Self::Revoke { .. } => Operation::LedgerSetupRevoke,
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

fn run_status(cli: &Cli) -> Result<(), CliError> {
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

pub(crate) fn begin_json(
    db: &Db,
    actor: &Actor,
    name: &str,
    public_key: &str,
    ttl: &str,
) -> Result<serde_json::Value, CliError> {
    let result = setup::begin(db, actor, name, public_key, identity::parse_ttl(ttl)?)?;
    Ok(serde_json::json!({
        "session": result.session,
        "credential": result.credential.response_json(),
    }))
}

pub(crate) fn end_json(
    db: &Db,
    actor: &Actor,
    generation: Generation,
    end: End,
) -> Result<serde_json::Value, CliError> {
    let session = setup::end(db, actor, generation, end)?;
    serde_json::to_value(session).map_err(|error| CliError::General(error.to_string()))
}

pub(crate) fn run(cli: &Cli, command: &SetupCommand) -> Result<(), CliError> {
    if matches!(command, SetupCommand::Status) {
        return run_status(cli);
    }
    let db = cli.open_db()?;
    let actor = cli.principal();
    let data = match command {
        SetupCommand::Begin {
            name,
            public_key,
            ttl,
        } => begin_json(&db, &actor, name, public_key, ttl)?,
        SetupCommand::Finish { generation } => end_json(&db, &actor, *generation, End::Finish)?,
        SetupCommand::Revoke { generation } => end_json(&db, &actor, *generation, End::Revoke)?,
        SetupCommand::Status => return run_status(cli),
    };
    if cli.is_json() {
        println!(
            "{}",
            json::envelope_ok(cli.meta(command.operation().name(), None), data)?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&data)
                .map_err(|error| CliError::General(error.to_string()))?
        );
    }
    Ok(())
}
