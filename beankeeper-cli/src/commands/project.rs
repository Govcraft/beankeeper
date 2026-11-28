//! Shared command and MCP requests for project accounting.

use beankeeper::types::Currency;
use clap::{Args, Subcommand};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::corrections::bookkeeping::{Project as ProjectWrite, Submission},
        audit::projects::{
            self as writes, BillingChange, ProjectChange, ProjectInvoiceInput, ProjectMutation,
        },
        companies::{Company, CompanySlug},
        documents::DocumentId,
        ids::{EntryId, ReceiptId},
        projects::{
            self, AvailableSource, Project, ProjectBudgetInput, ProjectId, ProjectMonth,
            ProjectReport, ProjectTerms, SplitInput, TimeEntry, TimeEntryId, WorkInput,
        },
    },
    error::CliError,
    operation::Operation,
    output::json,
    request::CalendarDateArg,
};

/// Company-scoped project accounting.
#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Project accounting and billing operations.
#[derive(Debug, Clone, Subcommand)]
pub enum ProjectCommand {
    /// Define a project for a customer.
    Create {
        /// JSON object with code, name, customer ID and currency.
        #[arg(long, value_parser = parse_json::<ProjectTerms>)]
        terms: ProjectTerms,
    },
    /// Archive a project and retain its accounting history.
    Archive { id: ProjectId },
    /// List active projects with optional archived projects.
    List {
        #[arg(long)]
        include_archived: bool,
    },
    /// Show a project by ID.
    Show { id: ProjectId },
    /// Record dated work and an optional exact billable rate.
    Record {
        /// JSON object with project, date, minutes, description and optional `rate_minor`.
        #[arg(long, value_parser = parse_json::<WorkInput>)]
        work: WorkInput,
    },
    /// Approve a work record for billing.
    Approve {
        id: TimeEntryId,
        #[arg(long)]
        date: CalendarDateArg,
    },
    /// List recorded time and approvals for a project.
    Time { id: ProjectId },
    /// Attribute exact portions of a posted revenue or expense entry.
    Split {
        entry: EntryId,
        #[arg(long)]
        date: CalendarDateArg,
        /// JSON list of project, `amount_minor` and optional billable state.
        #[arg(long, value_parser = parse_splits)]
        splits: Splits,
    },
    /// Set the project expense target for a calendar month.
    Budget {
        /// JSON object with project, month and `amount_minor`.
        #[arg(long, value_parser = parse_json::<ProjectBudgetInput>)]
        budget: ProjectBudgetInput,
    },
    /// List approved, unclaimed work and expenses as of the final day.
    Unbilled {
        id: ProjectId,
        #[arg(long)]
        from: CalendarDateArg,
        #[arg(long)]
        through: CalendarDateArg,
    },
    /// Build an invoice draft from selected approved work and expenses.
    DraftInvoice {
        /// JSON object with project, number, accounts, dates and exact source IDs.
        #[arg(long, value_parser = parse_json::<ProjectInvoiceInput>)]
        invoice: ProjectInvoiceInput,
    },
    /// Cancel an unissued project invoice draft.
    CancelInvoice { id: DocumentId },
    /// Report monthly project profit, budget variance and unallocated amounts.
    Report {
        #[arg(long)]
        month: ProjectMonth,
        #[arg(long)]
        currency: Currency,
        /// Include journal entries and their dated distributions.
        #[arg(long)]
        detail: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Splits(pub(crate) Vec<SplitInput>);

fn parse_json<T: DeserializeOwned>(raw: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(raw)
}

fn parse_splits(raw: &str) -> Result<Splits, serde_json::Error> {
    parse_json(raw).map(Splits)
}

impl ProjectCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Create { .. } => Operation::ProjectCreate,
            Self::Archive { .. } => Operation::ProjectArchive,
            Self::List { .. } => Operation::ProjectList,
            Self::Show { .. } => Operation::ProjectShow,
            Self::Record { .. } => Operation::ProjectRecord,
            Self::Approve { .. } => Operation::ProjectApprove,
            Self::Time { .. } => Operation::ProjectTime,
            Self::Split { .. } => Operation::ProjectSplit,
            Self::Budget { .. } => Operation::ProjectBudget,
            Self::Unbilled { .. } => Operation::ProjectUnbilled,
            Self::DraftInvoice { .. } => Operation::ProjectDraftInvoice,
            Self::CancelInvoice { .. } => Operation::ProjectCancelInvoice,
            Self::Report { .. } => Operation::ProjectReport,
        }
    }
}

/// A project operation with its company scope.
pub struct ProjectRequest {
    company: CompanySlug,
    command: ProjectCommand,
}

impl From<(&CompanySlug, &ProjectArgs)> for ProjectRequest {
    fn from((company, args): (&CompanySlug, &ProjectArgs)) -> Self {
        Self::new(company.clone(), args.command.clone())
    }
}

impl ProjectRequest {
    pub(crate) const fn new(company: CompanySlug, command: ProjectCommand) -> Self {
        Self { company, command }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        &self.company
    }

    pub(crate) const fn operation(&self) -> Operation {
        self.command.operation()
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<ProjectResponse, CliError> {
        use ProjectCommand as Cmd;
        use ProjectMutation as Edit;

        let company = &self.company;
        let result = match self.command {
            Cmd::Create { terms } => {
                return ProjectWrite::Create { terms }
                    .submit(db, actor, company)
                    .map(Response::Bookkeeping)
                    .map(ProjectResponse);
            }
            Cmd::Archive { id } => Edit::Archive(id),
            Cmd::Record { work } => Edit::Record(work),
            Cmd::Approve { id, date } => Edit::Approve(id, date.into()),
            Cmd::Split {
                entry,
                date,
                splits,
            } => {
                return ProjectWrite::Split {
                    entry,
                    date: date.into(),
                    splits: splits.0,
                }
                .submit(db, actor, company)
                .map(Response::Bookkeeping)
                .map(ProjectResponse);
            }
            Cmd::Budget { budget } => Edit::Budget(budget),
            Cmd::DraftInvoice { invoice } => {
                return writes::draft_invoice(db, actor, company, invoice)
                    .map(Box::new)
                    .map(Response::Invoice)
                    .map(ProjectResponse);
            }
            Cmd::CancelInvoice { id } => {
                let receipt_id = writes::cancel_invoice(db, actor, company, &id)?;
                return Ok(ProjectResponse(Response::Receipt { receipt_id }));
            }
            command => {
                let scope = Company::bind(db.read(), self.company)?;
                return Self::read(&scope, command).map(ProjectResponse);
            }
        };
        writes::mutate(db, actor, company, result)
            .map(Response::Change)
            .map(ProjectResponse)
    }

    fn read(company: &Company<'_>, command: ProjectCommand) -> Result<Response, CliError> {
        use ProjectCommand as Cmd;
        match command {
            Cmd::List { include_archived } => {
                projects::list_projects(company, include_archived).map(Response::List)
            }
            Cmd::Show { id } => projects::find_project(company, &id).map(Response::Project),
            Cmd::Time { id } => projects::list_time(company, &id).map(Response::Time),
            Cmd::Unbilled { id, from, through } => {
                projects::unbilled(company, &id, from.into(), through.into())
                    .map(Response::Unbilled)
            }
            Cmd::Report {
                month,
                currency,
                detail,
            } => projects::monthly_report(company, month, currency, detail).map(Response::Report),
            _ => Err(CliError::Database(
                "expected a project read operation".into(),
            )),
        }
    }
}

/// The same structured response for CLI JSON and MCP.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ProjectResponse(Response);

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Change(ProjectChange),
    Bookkeeping(Submission<ProjectChange>),
    Invoice(Box<BillingChange>),
    Receipt { receipt_id: ReceiptId },
    List(Vec<Project>),
    Project(Project),
    Time(Vec<TimeEntry>),
    Unbilled(Vec<AvailableSource>),
    Report(ProjectReport),
}

impl ProjectResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        match (&self.0, format) {
            (_, OutputFormat::Json) => println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
            ),
            (Response::Report(report), _) => super::rule::render_rows(
                format,
                &[
                    "code",
                    "revenue_minor",
                    "expense_minor",
                    "profit_minor",
                    "budget_remaining_minor",
                    "unbilled_time_minor",
                    "unbilled_expense_minor",
                ],
                report
                    .projects
                    .iter()
                    .map(|row| {
                        [
                            row.project.terms.code.to_string(),
                            row.recognized.revenue_minor.to_string(),
                            row.recognized.expense_minor.to_string(),
                            row.profit_minor.to_string(),
                            row.budget_remaining_minor
                                .map_or_else(String::new, |value| value.to_string()),
                            row.unbilled_time_minor.to_string(),
                            row.unbilled_expense_minor.to_string(),
                        ]
                    })
                    .chain(std::iter::once([
                        "Unallocated".into(),
                        report.unallocated.revenue_minor.to_string(),
                        report.unallocated.expense_minor.to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ])),
            )?,
            _ => println!(
                "{}",
                serde_json::to_string_pretty(self).map_err(|error| CliError::General(format!(
                    "project output could not be rendered: {error}"
                )))?
            ),
        }
        Ok(())
    }
}
