//! Shared CLI and MCP entry points for reviewed vendor payments.

use chrono::NaiveDate;
use clap::{Args, Subcommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::disbursements as writes,
        companies::{Company, CompanySlug},
        disbursements::{self, Disbursement, Terms},
    },
    disbursement::{DestinationFingerprint, DisbursementId},
    error::CliError,
    mcp::input_values::Text,
    operation::Operation,
    output::json::envelope_ok,
    request::CalendarDateArg,
};

/// Review, submit, and reconcile vendor payment instructions.
#[derive(Debug, Args)]
pub struct DisbursementArgs {
    /// Company whose issued bills will be paid.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
    #[command(subcommand)]
    pub command: DisbursementCommand,
}

/// Commands for reviewed vendor payments.
#[derive(Debug, Clone, Subcommand)]
pub enum DisbursementCommand {
    /// Draft a payment instruction with a cash account and bill allocations, supplied as JSON.
    Draft {
        #[arg(long)]
        terms: String,
    },
    /// Retrieve the host's current beneficiary fingerprint for review.
    Preview { id: DisbursementId },
    /// Approve the frozen terms and reviewed host beneficiary.
    Approve {
        id: DisbursementId,
        #[arg(long)]
        destination: DestinationFingerprint,
    },
    /// Revoke approval before an attempt.
    Revoke { id: DisbursementId },
    /// Decline an instruction before dispatch.
    Reject {
        id: DisbursementId,
        #[arg(long)]
        reason: String,
    },
    /// Send the first approved attempt or look up a previous attempt.
    Dispatch { id: DisbursementId },
    /// Post confirmed evidence to an open accounting date.
    Reconcile {
        id: DisbursementId,
        #[arg(long)]
        date: CalendarDateArg,
    },
    /// Show the current instruction and recorded evidence.
    Show { id: DisbursementId },
    /// List up to 100 recent payment instructions.
    List,
}

impl DisbursementCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Draft { .. } => Operation::DisbursementDraft,
            Self::Preview { .. } => Operation::DisbursementPreview,
            Self::Approve { .. } => Operation::DisbursementApprove,
            Self::Revoke { .. } => Operation::DisbursementRevoke,
            Self::Reject { .. } => Operation::DisbursementReject,
            Self::Dispatch { .. } => Operation::DisbursementDispatch,
            Self::Reconcile { .. } => Operation::DisbursementReconcile,
            Self::Show { .. } => Operation::DisbursementShow,
            Self::List => Operation::DisbursementList,
        }
    }
}

pub(crate) enum Action {
    Draft(Terms),
    Preview(DisbursementId),
    Approve(DisbursementId, DestinationFingerprint),
    Revoke(DisbursementId),
    Reject(DisbursementId, String),
    Dispatch(DisbursementId),
    Reconcile(DisbursementId, NaiveDate),
    Show(DisbursementId),
    List,
}

/// Request shared by the CLI and MCP catalog.
pub struct DisbursementRequest {
    pub(crate) company: CompanySlug,
    pub(crate) action: Action,
}

impl TryFrom<(CompanySlug, DisbursementCommand)> for DisbursementRequest {
    type Error = CliError;

    fn try_from(
        (company, command): (CompanySlug, DisbursementCommand),
    ) -> Result<Self, Self::Error> {
        let action = match command {
            DisbursementCommand::Draft { terms } => Action::Draft(
                serde_json::from_str(&terms)
                    .map_err(|err| CliError::Validation(format!("invalid payment terms: {err}")))?,
            ),
            DisbursementCommand::Preview { id } => Action::Preview(id),
            DisbursementCommand::Approve { id, destination } => Action::Approve(id, destination),
            DisbursementCommand::Revoke { id } => Action::Revoke(id),
            DisbursementCommand::Reject { id, reason } => Action::Reject(id, reason),
            DisbursementCommand::Dispatch { id } => Action::Dispatch(id),
            DisbursementCommand::Reconcile { id, date } => Action::Reconcile(id, date.into()),
            DisbursementCommand::Show { id } => Action::Show(id),
            DisbursementCommand::List => Action::List,
        };
        Ok(Self { company, action })
    }
}

impl DisbursementRequest {
    pub(crate) fn from_cli(
        cli: &Cli,
        company: &CompanySlug,
        command: &DisbursementCommand,
    ) -> Result<Self, CliError> {
        if matches!(resolve_format(None, cli), OutputFormat::Csv) {
            return Err(unsupported_output());
        }
        (company.clone(), command.clone()).try_into()
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self.action {
            Action::Draft(_) => Operation::DisbursementDraft,
            Action::Preview(_) => Operation::DisbursementPreview,
            Action::Approve(_, _) => Operation::DisbursementApprove,
            Action::Revoke(_) => Operation::DisbursementRevoke,
            Action::Reject(_, _) => Operation::DisbursementReject,
            Action::Dispatch(_) => Operation::DisbursementDispatch,
            Action::Reconcile(_, _) => Operation::DisbursementReconcile,
            Action::Show(_) => Operation::DisbursementShow,
            Action::List => Operation::DisbursementList,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<Response, CliError> {
        let Self { company, action } = self;
        let record = match action {
            Action::Draft(terms) => writes::draft(db, actor, &company, terms)?,
            Action::Preview(id) => {
                let destination = writes::preview(db, actor, &company, &id)?;
                return Ok(Response(Data::Preview { destination }));
            }
            Action::Approve(id, destination) => {
                writes::approve(db, actor, &company, &id, &destination)?
            }
            Action::Revoke(id) => writes::revoke(db, actor, &company, &id)?,
            Action::Reject(id, reason) => writes::reject(db, actor, &company, &id, &reason)?,
            Action::Dispatch(id) => writes::dispatch(db, actor, &company, &id)?,
            Action::Reconcile(id, date) => writes::reconcile(db, actor, &company, &id, date)?,
            Action::Show(id) => db.read_snapshot(|reader| {
                disbursements::read(&Company::bind(reader, company)?, &id)
            })?,
            Action::List => {
                return db
                    .read_snapshot(|reader| disbursements::list(&Company::bind(reader, company)?))
                    .map(|rows| Response(Data::List(rows)));
            }
        };
        Ok(Response(Data::Record(Box::new(record))))
    }
}

/// Payment evidence visible to the CLI and MCP.
#[derive(Serialize)]
#[serde(transparent)]
pub struct Response(Data);

#[derive(Serialize)]
#[serde(untagged)]
enum Data {
    Preview { destination: DestinationFingerprint },
    Record(Box<Disbursement>),
    List(Vec<Disbursement>),
}

impl Response {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => {
                println!(
                    "{}",
                    envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
                );
            }
            OutputFormat::Table => {
                let records: &[Disbursement] = match &self.0 {
                    Data::Preview { destination } => {
                        println!("{destination}");
                        return Ok(());
                    }
                    Data::Record(record) => std::slice::from_ref(record.as_ref()),
                    Data::List(records) => records,
                };
                for record in records {
                    println!(
                        "{}  {}  {} minor units {}  {}",
                        record.id,
                        record.vendor,
                        record.amount_minor,
                        record.currency,
                        record.status()
                    );
                }
            }
            OutputFormat::Csv => {
                return Err(unsupported_output());
            }
        }
        Ok(())
    }
}

fn unsupported_output() -> CliError {
    CliError::Usage("disbursements support table and JSON output".into())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisbursementDraftInput {
    pub(crate) company: Text<CompanySlug>,
    pub(crate) terms: Terms,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisbursementSelector {
    pub(crate) company: Text<CompanySlug>,
    pub(crate) id: DisbursementId,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisbursementRejectInput {
    pub(crate) company: Text<CompanySlug>,
    pub(crate) id: DisbursementId,
    pub(crate) reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisbursementReconcileInput {
    pub(crate) company: Text<CompanySlug>,
    pub(crate) id: DisbursementId,
    pub(crate) date: CalendarDateArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisbursementApproveInput {
    pub(crate) company: Text<CompanySlug>,
    pub(crate) id: DisbursementId,
    pub(crate) destination: DestinationFingerprint,
}
