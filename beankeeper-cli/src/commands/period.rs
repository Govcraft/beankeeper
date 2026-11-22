//! CLI and MCP operations for company posting cutoffs.

use crate::cli::{Cli, OutputFormat, PeriodCommand, resolve_format};
use crate::db::{Actor, Db, audit, periods};
use crate::error::CliError;
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

/// Execute an operation without producing output; shared with MCP.
///
/// # Errors
/// Returns validation, authorization, or storage errors.
pub fn execute(
    db: &Db,
    company: &str,
    command: &PeriodCommand,
    actor: &Actor,
) -> Result<Vec<periods::PeriodLock>, CliError> {
    match command {
        PeriodCommand::Close { through } => Ok(vec![audit::change_period(
            db.conn(),
            actor,
            company,
            periods::PeriodChange::Close(through.parse()?),
        )?]),
        PeriodCommand::Reopen { reason } => Ok(vec![audit::change_period(
            db.conn(),
            actor,
            company,
            periods::PeriodChange::Reopen(reason.parse()?),
        )?]),
        PeriodCommand::List => periods::list(db.conn(), company),
    }
}

/// Execute and render a period command in JSON, table, or CSV form.
///
/// # Errors
/// Returns validation, authorization, storage, or output errors.
pub fn run(cli: &Cli, company: &str, command: &PeriodCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let rows = execute(&db, company, command, &cli.principal())?;
    let format = resolve_format(None, cli);
    if cli.verbosity.quiet && format != OutputFormat::Json {
        return Ok(());
    }
    match format {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(cli.meta(operation(command), Some(company)), &rows,)?
        ),
        OutputFormat::Table => {
            let mut table = comfy_table::Table::new();
            table.set_header(["Company", "Closed through", "Changed at", "Actor", "Reason"]);
            for row in &rows {
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
            for row in &rows {
                writer
                    .serialize(row)
                    .map_err(|error| CliError::General(error.to_string()))?;
            }
            writer.flush()?;
        }
    }
    Ok(())
}
