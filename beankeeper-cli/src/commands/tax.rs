//! CLI and MCP entry points for configured VAT and its reviewable register.

use beankeeper::types::{AccountCode, Currency};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        companies::{Company, CompanySlug},
        tax_codes::{
            self, RateRevision, RateTerms, TaxCodeName, TaxError, TaxRate, TaxTreatment,
            VatRegister,
        },
    },
    error::CliError,
    operation::Operation,
    output::json,
    request::CalendarDateArg,
};

/// Owner VAT settings and period evidence.
#[derive(Debug, Subcommand)]
pub enum TaxCommand {
    /// Configure one effective-dated UK invoice VAT code revision.
    Configure(ConfigureArgs),
    /// List both current and historical configured revisions.
    List,
    /// Read VAT evidence and account reconciliation for a GBP period.
    Report {
        #[arg(long)]
        from: CalendarDateArg,
        #[arg(long)]
        to: CalendarDateArg,
        #[arg(long, default_value_t = Currency::GBP)]
        currency: Currency,
    },
}

/// Required owner configuration for a rate revision.
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Stable owner-facing code.
    pub code: TaxCodeName,
    /// Date the revision first applies.
    #[arg(long)]
    pub effective_from: CalendarDateArg,
    /// Owner-selected treatment.
    #[arg(long, value_enum)]
    pub treatment: TaxTreatment,
    /// Exact percentage using up to four decimal places.
    #[arg(long)]
    pub rate: TaxRate,
    /// Liability used for output VAT.
    #[arg(long)]
    pub output_account: Option<AccountCode>,
    /// Asset used for recoverable input VAT.
    #[arg(long)]
    pub input_account: Option<AccountCode>,
}

impl TaxCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Configure(_) => Operation::TaxConfigure,
            Self::List => Operation::TaxList,
            Self::Report { .. } => Operation::TaxReport,
        }
    }
}

/// Typed operations shared by the terminal and MCP.
pub enum TaxRequest {
    Configure(CompanySlug, TaxCodeName, RateTerms),
    List(CompanySlug),
    Report(CompanySlug, NaiveDate, NaiveDate, Currency),
}

impl TaxRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Configure(..) => Operation::TaxConfigure,
            Self::List(_) => Operation::TaxList,
            Self::Report(..) => Operation::TaxReport,
        }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        match self {
            Self::Configure(company, ..) | Self::List(company) | Self::Report(company, ..) => {
                company
            }
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<TaxResponse, CliError> {
        let response = match self {
            Self::Configure(company, name, terms) => Response::Revision(Box::new(
                crate::db::audit::tax_codes::configure(db, actor, &company, name, terms)?,
            )),
            Self::List(company) => {
                let scope = Company::bind(db.read(), company)?;
                Response::Revisions(tax_codes::list_rates(&scope)?)
            }
            Self::Report(company, from, to, currency) => {
                if currency != Currency::GBP {
                    return Err(TaxError::Currency.into());
                }
                let scope = Company::bind(db.read(), company)?;
                Response::Register(Box::new(tax_codes::read_vat_register(&scope, from, to)?))
            }
        };
        Ok(TaxResponse(response))
    }
}

pub(crate) fn ledger_request(
    cli: &Cli,
    command: &TaxCommand,
) -> Result<crate::request::LedgerRequest, CliError> {
    let company = crate::cli::require_company(cli)?.clone();
    Ok(crate::request::LedgerRequest::Tax(
        (company, command).into(),
    ))
}

impl From<(CompanySlug, &TaxCommand)> for TaxRequest {
    fn from((company, command): (CompanySlug, &TaxCommand)) -> Self {
        match command {
            TaxCommand::Configure(args) => Self::Configure(
                company,
                args.code.clone(),
                RateTerms {
                    effective_from: args.effective_from.into(),
                    treatment: args.treatment,
                    rate: args.rate,
                    output_account: args.output_account.clone(),
                    input_account: args.input_account.clone(),
                },
            ),
            TaxCommand::List => Self::List(company),
            TaxCommand::Report { from, to, currency } => {
                Self::Report(company, (*from).into(), (*to).into(), *currency)
            }
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Revision(Box<RateRevision>),
    Revisions(Vec<RateRevision>),
    Register(Box<VatRegister>),
}

/// A typed VAT result shared by CLI JSON and MCP.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct TaxResponse(Response);

impl TaxResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if format == OutputFormat::Json {
            println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
            );
            return Ok(());
        }
        match &self.0 {
            Response::Revision(revision) => {
                render_revisions(format, std::slice::from_ref(revision.as_ref()))
            }
            Response::Revisions(revisions) => render_revisions(format, revisions),
            Response::Register(register) => super::rule::render_rows(
                format,
                &[
                    "kind",
                    "type",
                    "date",
                    "document",
                    "posting",
                    "code",
                    "revision",
                    "account",
                    "net_minor",
                    "vat_minor",
                    "gross_minor",
                    "expected_minor",
                    "posted_minor",
                ],
                register.export_rows(),
            ),
        }
    }
}

fn render_revisions(format: OutputFormat, rows: &[RateRevision]) -> Result<(), CliError> {
    super::rule::render_rows(
        format,
        &[
            "code",
            "code_id",
            "revision",
            "effective_from",
            "treatment",
            "rate_percent",
            "output_account",
            "input_account",
        ],
        rows.iter().map(|row| {
            [
                row.name.to_string(),
                row.code.to_string(),
                row.id.to_string(),
                row.terms.effective_from.to_string(),
                row.terms.treatment.as_str().into(),
                row.terms.rate.to_string(),
                row.terms
                    .output_account
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                row.terms
                    .input_account
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ]
        }),
    )
}
