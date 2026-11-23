//! CLI and MCP operations for company posting cutoffs.

use crate::cli::{Cli, OutputFormat, PeriodCommand, resolve_format};
use crate::db::{Actor, Db, audit, companies::CompanySlug, periods};
use crate::error::CliError;
use crate::identity::{Admin, Granted};
use crate::operation::Operation;
use crate::output::json;

/// Canonical command name used for authorization and JSON envelopes.
#[must_use]
pub fn command_name(command: &PeriodCommand) -> &'static str {
    operation(command).name()
}

pub(crate) const fn operation(command: &PeriodCommand) -> Operation {
    match command {
        PeriodCommand::Close { .. } => Operation::PeriodClose,
        PeriodCommand::Reopen { .. } => Operation::PeriodReopen,
        PeriodCommand::List => Operation::PeriodList,
    }
}

/// A company-scoped operation on the posting cutoff.
pub enum PeriodRequest {
    /// Reject postings through an inclusive, canonical cutoff date.
    Close(CompanySlug, periods::PeriodDate),
    /// Unlock the posting period and record a nonblank explanation.
    Reopen(CompanySlug, periods::ReopenReason),
    /// Read the cutoff and its last change.
    List(CompanySlug),
}

impl TryFrom<(CompanySlug, &PeriodCommand)> for PeriodRequest {
    type Error = CliError;

    fn try_from((company, command): (CompanySlug, &PeriodCommand)) -> Result<Self, Self::Error> {
        Ok(match command {
            PeriodCommand::Close { through } => Self::Close(company, through.parse()?),
            PeriodCommand::Reopen { reason } => Self::Reopen(company, reason.parse()?),
            PeriodCommand::List => Self::List(company),
        })
    }
}

impl PeriodRequest {
    /// Operation used for authorization and output metadata.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Close(..) => Operation::PeriodClose,
            Self::Reopen(..) => Operation::PeriodReopen,
            Self::List(_) => Operation::PeriodList,
        }
    }

    /// Company whose posting period the operation addresses.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        match self {
            Self::Close(company, _) | Self::Reopen(company, _) | Self::List(company) => company,
        }
    }

    /// Execute a period request.
    ///
    /// # Errors
    /// Returns authorization or storage errors, or rejects a cutoff change that conflicts with the ledger.
    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<PeriodResponse, CliError> {
        let (company, change) = match self {
            Self::Close(company, through) => (company, periods::PeriodChange::Close(through)),
            Self::Reopen(company, reason) => (company, periods::PeriodChange::Reopen(reason)),
            Self::List(company) => {
                let scoped = crate::db::companies::Company::bind(db.read(), company)?;
                return periods::list(&scoped).map(PeriodResponse);
            }
        };
        let lock = Granted::<Admin>::with_db(db, actor, |grant| {
            audit::change_period(grant, &company, change)
        })?;
        Ok(PeriodResponse(vec![lock]))
    }
}

/// Persisted posting cutoff rows.
#[derive(serde::Serialize)]
#[serde(transparent)]
pub struct PeriodResponse(Vec<periods::PeriodLock>);

impl PeriodResponse {
    /// The state and last change of any matching posting cutoff.
    #[must_use]
    pub fn rows(&self) -> &[periods::PeriodLock] {
        &self.0
    }

    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &str,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if cli.verbosity.quiet && format != OutputFormat::Json {
            return Ok(());
        }
        match format {
            OutputFormat::Json => println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company)), self)?
            ),
            OutputFormat::Table => {
                let mut table = comfy_table::Table::new();
                table.set_header(["Company", "Closed through", "Changed at", "Actor", "Reason"]);
                for row in self.rows() {
                    let through = row.closed_through().map(|date| date.to_string());
                    table.add_row([
                        row.company_slug.as_str(),
                        through.as_deref().unwrap_or("open"),
                        &row.changed_at,
                        &row.actor,
                        row.reason().unwrap_or(""),
                    ]);
                }
                println!("{table}");
            }
            OutputFormat::Csv => {
                let mut writer = csv::WriterBuilder::new()
                    .has_headers(false)
                    .from_writer(std::io::stdout());
                writer
                    .write_record([
                        "company_slug",
                        "closed_through",
                        "changed_at",
                        "actor",
                        "reason",
                    ])
                    .map_err(|error| CliError::General(error.to_string()))?;
                for row in self.rows() {
                    writer
                        .serialize(row)
                        .map_err(|error| CliError::General(error.to_string()))?;
                }
                writer.flush()?;
            }
        }
        Ok(())
    }
}
