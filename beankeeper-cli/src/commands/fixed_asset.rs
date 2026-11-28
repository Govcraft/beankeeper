//! Shared CLI and MCP operations for the fixed asset register.

use beankeeper::types::{AccountCode, AccountName, Currency};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        amounts::PositiveMinorUnits,
        audit::grant::PostingGrant,
        companies::{Company, CompanySlug},
        fixed_assets::{
            self, Acquisition, AssetAccounts, Disposal, FixedAssetId, Register, Terms,
            UsefulLifeMonths,
        },
    },
    error::CliError,
    identity::{Granted, Post},
    operation::Operation,
    output::json::envelope_ok,
    request::CalendarDateArg,
};

use super::reference::TransactionReference;

pub(crate) mod wire;

#[derive(Debug, Args)]
pub struct FixedAssetArgs {
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
    /// Fixed asset operation.
    #[command(subcommand)]
    pub command: FixedAssetCommand,
}

#[derive(Debug, Subcommand)]
pub enum FixedAssetCommand {
    /// Register an existing acquisition or post its acquisition journal.
    Register(Box<RegisterArgs>),
    /// Show a fixed asset and its dated movements.
    Show {
        id: FixedAssetId,
        #[arg(long)]
        as_of: CalendarDateArg,
    },
    /// List registered assets and carrying values as of a date.
    List {
        #[arg(long)]
        as_of: CalendarDateArg,
    },
    /// Preview unposted monthly depreciation charges.
    Preview {
        id: FixedAssetId,
        #[arg(long)]
        through: CalendarDateArg,
    },
    /// Post due charges through an inclusive cutoff.
    Depreciate {
        id: FixedAssetId,
        #[arg(long)]
        through: CalendarDateArg,
    },
    /// Dispose of an asset and post any gain or loss.
    Dispose(DisposeArgs),
}

#[derive(Debug, Clone, Args)]
pub struct RegisterArgs {
    #[arg(long)]
    name: AccountName,
    #[arg(long)]
    currency: Currency,
    #[arg(long)]
    acquired_on: CalendarDateArg,
    #[arg(long)]
    in_service: CalendarDateArg,
    #[arg(long)]
    cost_minor: PositiveMinorUnits,
    #[arg(long, default_value_t)]
    salvage_minor: i64,
    #[arg(long, default_value_t)]
    opening_depreciation_minor: i64,
    #[arg(long)]
    life_months: UsefulLifeMonths,
    #[arg(long)]
    cost_account: AccountCode,
    #[arg(long)]
    accumulated_account: AccountCode,
    #[arg(long)]
    expense_account: AccountCode,
    #[arg(
        long,
        required_unless_present = "offset_account",
        conflicts_with = "offset_account"
    )]
    acquisition_transaction: Option<crate::db::ids::TransactionId>,
    #[arg(long, requires = "reference")]
    offset_account: Option<AccountCode>,
    #[arg(long, requires = "offset_account")]
    reference: Option<TransactionReference>,
}

#[derive(Debug, Clone, Args)]
pub struct DisposeArgs {
    id: FixedAssetId,
    #[arg(long)]
    date: CalendarDateArg,
    #[arg(long, default_value_t)]
    proceeds_minor: i64,
    #[arg(long)]
    proceeds_account: Option<AccountCode>,
    #[arg(long)]
    gain_account: AccountCode,
    #[arg(long)]
    loss_account: AccountCode,
}

impl FixedAssetCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Register(_) => Operation::FixedAssetRegister,
            Self::Show { .. } => Operation::FixedAssetShow,
            Self::List { .. } => Operation::FixedAssetList,
            Self::Preview { .. } => Operation::FixedAssetPreview,
            Self::Depreciate { .. } => Operation::FixedAssetDepreciate,
            Self::Dispose(_) => Operation::FixedAssetDispose,
        }
    }
}

/// One validated fixed asset request, independent of transport.
pub struct FixedAssetRequest {
    pub(crate) company: CompanySlug,
    pub(crate) action: Action,
}

pub(crate) enum Action {
    Register(Box<Register>),
    Show(FixedAssetId, NaiveDate),
    List(NaiveDate),
    Preview(FixedAssetId, NaiveDate),
    Depreciate(FixedAssetId, NaiveDate),
    Dispose(Disposal),
}

impl RegisterArgs {
    fn into_input(self) -> Result<Register, CliError> {
        let acquisition = match (
            self.acquisition_transaction,
            self.offset_account,
            self.reference,
        ) {
            (Some(id), None, None) => Acquisition::Existing(id),
            (None, Some(offset), Some(reference)) => Acquisition::Post { offset, reference },
            _ => {
                return Err(CliError::Validation(
                    "select an existing acquisition or supply an offset and reference".into(),
                ));
            }
        };
        Ok(Register {
            acquisition,
            terms: Terms {
                name: self.name,
                currency: self.currency,
                acquired_on: self.acquired_on.into(),
                in_service: self.in_service.into(),
                cost: self.cost_minor,
                salvage_minor: self.salvage_minor,
                opening_depreciation_minor: self.opening_depreciation_minor,
                life_months: self.life_months,
                accounts: AssetAccounts {
                    cost: self.cost_account,
                    accumulated: self.accumulated_account,
                    expense: self.expense_account,
                },
            },
        })
    }
}

impl TryFrom<(&CompanySlug, &FixedAssetCommand)> for FixedAssetRequest {
    type Error = CliError;
    fn try_from(
        (company, command): (&CompanySlug, &FixedAssetCommand),
    ) -> Result<Self, Self::Error> {
        let action = match command {
            FixedAssetCommand::Register(args) => {
                Action::Register(Box::new(args.as_ref().clone().into_input()?))
            }
            FixedAssetCommand::Show { id, as_of } => Action::Show(id.clone(), (*as_of).into()),
            FixedAssetCommand::List { as_of } => Action::List((*as_of).into()),
            FixedAssetCommand::Preview { id, through } => {
                Action::Preview(id.clone(), (*through).into())
            }
            FixedAssetCommand::Depreciate { id, through } => {
                Action::Depreciate(id.clone(), (*through).into())
            }
            FixedAssetCommand::Dispose(input) => Action::Dispose(Disposal {
                asset: input.id.clone(),
                date: input.date.into(),
                proceeds_minor: input.proceeds_minor,
                proceeds_account: input.proceeds_account.clone(),
                gain_account: input.gain_account.clone(),
                loss_account: input.loss_account.clone(),
            }),
        };
        Ok(Self {
            company: company.clone(),
            action,
        })
    }
}

impl FixedAssetRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self.action {
            Action::Register(_) => Operation::FixedAssetRegister,
            Action::Show(_, _) => Operation::FixedAssetShow,
            Action::List(_) => Operation::FixedAssetList,
            Action::Preview(_, _) => Operation::FixedAssetPreview,
            Action::Depreciate(_, _) => Operation::FixedAssetDepreciate,
            Action::Dispose(_) => Operation::FixedAssetDispose,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<FixedAssetResponse, CliError> {
        let operation = self.operation();
        let company = self.company;
        let book = || company.clone();
        let asset = match self.action {
            Action::Register(input) => direct(db, actor, |grant| {
                fixed_assets::register(grant, book(), *input)
            })?
            .into(),
            Action::Show(id, date) => db
                .read_snapshot(|reader| {
                    fixed_assets::show(&Company::bind(reader, book())?, &id, date)
                })?
                .into(),
            Action::List(date) => db
                .read_snapshot(|reader| fixed_assets::list(&Company::bind(reader, book())?, date))
                .map(AssetData::Register)?,
            Action::Preview(id, date) => db
                .read_snapshot(|reader| {
                    fixed_assets::preview(&Company::bind(reader, book())?, &id, date)
                })
                .map(AssetData::Charges)?,
            Action::Depreciate(id, date) => direct(db, actor, |grant| {
                fixed_assets::depreciate(grant, book(), &id, date)
            })
            .map(AssetData::Charges)?,
            Action::Dispose(input) => direct(db, actor, |grant| {
                fixed_assets::dispose(grant, book(), input)
            })?
            .into(),
        };
        Ok(FixedAssetResponse {
            operation,
            company,
            asset,
        })
    }
}

fn direct<T>(
    db: &Db,
    actor: &Actor,
    action: impl for<'scope> FnOnce(Granted<'scope, Post>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => action(grant),
        PostingGrant::Proposal(_) => Err(CliError::Forbidden(
            "fixed asset changes require direct posting authority".into(),
        )),
    })
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum AssetData {
    Asset(Box<fixed_assets::AssetView>),
    Register(fixed_assets::RegisterView),
    Charges(Vec<fixed_assets::Depreciation>),
}

impl From<fixed_assets::AssetView> for AssetData {
    fn from(asset: fixed_assets::AssetView) -> Self {
        Self::Asset(Box::new(asset))
    }
}

/// Result of a fixed asset operation.
#[derive(Serialize)]
pub struct FixedAssetResponse {
    #[serde(skip)]
    operation: Operation,
    #[serde(skip)]
    company: CompanySlug,
    asset: AssetData,
}

impl FixedAssetResponse {
    pub(crate) const fn data(&self) -> &AssetData {
        &self.asset
    }

    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let meta = cli.meta(self.operation, Some(self.company.as_str()));
        match resolve_format(None, cli) {
            OutputFormat::Json => println!("{}", envelope_ok(meta, &self.asset)?),
            OutputFormat::Table => match &self.asset {
                AssetData::Asset(asset) => println!("{asset}"),
                AssetData::Register(register) => print!("{register}"),
                AssetData::Charges(charges) => {
                    for charge in charges {
                        println!("{charge}");
                    }
                }
            },
            OutputFormat::Csv => self.csv()?,
        }
        Ok(())
    }

    fn csv(&self) -> Result<(), CliError> {
        let mut writer = csv::Writer::from_writer(std::io::stdout().lock());
        match &self.asset {
            AssetData::Asset(asset) => writer
                .serialize(asset.csv_row())
                .map_err(std::io::Error::other)?,
            AssetData::Register(register) => {
                for asset in register.rows() {
                    writer
                        .serialize(asset.csv_row())
                        .map_err(std::io::Error::other)?;
                }
            }
            AssetData::Charges(charges) => {
                for charge in charges {
                    writer
                        .serialize(charge.csv_row())
                        .map_err(std::io::Error::other)?;
                }
            }
        }
        writer.flush()?;
        Ok(())
    }
}
