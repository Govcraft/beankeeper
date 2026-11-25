//! Agent-facing FX posting and revaluation commands.

use beankeeper::types::{AccountCode, DebitOrCredit, ExchangeRate};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{Actor, Db, amounts::PositiveMinorUnits, companies::CompanySlug, fx},
    error::CliError,
    identity::{Granted, Post},
    operation::Operation,
    output::json::envelope_ok,
    request::CalendarDateArg,
};

use super::reference::TransactionReference;

/// Foreign-currency ledger operations.
#[derive(Debug, Args)]
pub struct FxArgs {
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
    #[command(subcommand)]
    pub command: FxCommand,
}

/// Typed CLI arguments for recording native movements and revaluing balances.
#[derive(Debug, Subcommand)]
pub enum FxCommand {
    /// Record a native movement and its balanced functional-currency value.
    Post {
        #[arg(long)]
        account: AccountCode,
        #[arg(long)]
        offset_account: AccountCode,
        #[arg(long)]
        direction: DebitOrCredit,
        #[arg(long)]
        native_minor: PositiveMinorUnits,
        #[arg(long)]
        rate: ExchangeRate,
        #[arg(long)]
        date: CalendarDateArg,
        #[arg(long)]
        description: String,
        #[arg(long)]
        reference: TransactionReference,
    },
    /// Post the change in carrying value. Omit the account to revalue all active foreign accounts.
    Revalue {
        #[arg(long)]
        account: Option<AccountCode>,
        #[arg(long)]
        fx_gain_loss_account: AccountCode,
        #[arg(long)]
        as_of: CalendarDateArg,
        /// Functional major units per native major unit. Defaults to the latest recorded rate for each pair.
        #[arg(long)]
        rate: Option<ExchangeRate>,
    },
}

impl FxCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Post { .. } => Operation::FxPost,
            Self::Revalue { .. } => Operation::FxRevalue,
        }
    }
}

/// A validated FX request shared by the CLI and MCP.
pub struct FxRequest {
    company: CompanySlug,
    change: Change,
}

enum Change {
    Post(fx::Movement),
    Revalue(fx::Revalue),
}

impl From<(CompanySlug, fx::Movement)> for FxRequest {
    fn from((company, movement): (CompanySlug, fx::Movement)) -> Self {
        Self {
            company,
            change: Change::Post(movement),
        }
    }
}

impl From<(CompanySlug, fx::Revalue)> for FxRequest {
    fn from((company, revalue): (CompanySlug, fx::Revalue)) -> Self {
        Self {
            company,
            change: Change::Revalue(revalue),
        }
    }
}

impl From<(&CompanySlug, &FxCommand)> for FxRequest {
    fn from((company, command): (&CompanySlug, &FxCommand)) -> Self {
        match command {
            FxCommand::Post {
                account,
                offset_account,
                direction,
                native_minor,
                rate,
                date,
                description,
                reference,
            } => (
                company.clone(),
                fx::Movement {
                    account: account.clone(),
                    offset: offset_account.clone(),
                    direction: *direction,
                    native_minor: *native_minor,
                    rate: *rate,
                    date: (*date).into(),
                    description: description.clone(),
                    reference: reference.clone(),
                },
            )
                .into(),
            FxCommand::Revalue {
                account,
                fx_gain_loss_account,
                as_of,
                rate,
            } => (
                company.clone(),
                fx::Revalue {
                    account: account.clone(),
                    offset: fx_gain_loss_account.clone(),
                    date: (*as_of).into(),
                    rate: *rate,
                },
            )
                .into(),
        }
    }
}

impl FxRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self.change {
            Change::Post(_) => Operation::FxPost,
            Change::Revalue(_) => Operation::FxRevalue,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<FxResponse, CliError> {
        let operation = self.operation();
        let postings = Granted::<Post>::with_db(db, actor, |grant| match self.change {
            Change::Post(input) => fx::post(grant, &self.company, &input),
            Change::Revalue(input) => fx::revalue(grant, &self.company, &input),
        })?;
        Ok(FxResponse {
            operation,
            company: self.company,
            postings,
        })
    }
}

/// Carrying-value changes and their undo receipts, when a posting was needed.
#[derive(Serialize)]
pub struct FxResponse {
    #[serde(skip)]
    operation: Operation,
    #[serde(skip)]
    company: CompanySlug,
    postings: Vec<fx::Posted>,
}

impl FxResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => println!(
                "{}",
                envelope_ok(cli.meta(self.operation, Some(self.company.as_str())), self)?
            ),
            OutputFormat::Table | OutputFormat::Csv => {
                if self.postings.is_empty() {
                    println!("No active foreign-currency accounts.");
                }
                for posting in &self.postings {
                    println!("{posting}");
                }
            }
        }
        Ok(())
    }
}
