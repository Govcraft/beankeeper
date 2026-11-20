//! Pending obligations, explicit posting, and recurring transaction templates.

use beankeeper::types::Currency;
use serde_json::Value;

use crate::cli::{Cli, OutputFormat, ScheduleCommand, resolve_format};
use crate::db::{self, PostEntryParams, schedules};
use crate::error::CliError;
use crate::output::json;

/// Execute a company-scoped schedule command.
///
/// # Errors
/// Returns validation, persistence, or output errors.
pub fn run(cli: &Cli, company: &str, command: &ScheduleCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    db::companies::get_company(db.conn(), company)?;
    let actor = cli.principal();
    let data = match command {
        ScheduleCommand::Create { .. } => {
            let params = create_params(command)?;
            serde_json::to_value(schedules::create_schedule(
                db.conn(),
                &actor,
                company,
                &params,
            )?)
        }
        ScheduleCommand::List { all, due_through } => {
            let rows: Vec<_> = schedules::list_schedules(db.conn(), company)?
                .into_iter()
                .filter(|row| *all || row.status == "pending")
                .filter(|row| {
                    due_through.is_none_or(|cutoff| row.next_due.is_some_and(|date| date <= cutoff))
                })
                .collect();
            serde_json::to_value(rows)
        }
        ScheduleCommand::Run {
            through,
            dry_run,
            allow_large_run,
        } => {
            let through = through.unwrap_or_else(|| chrono::Local::now().date_naive());
            serde_json::to_value(schedules::run_due_with_options(
                db.conn(),
                &actor,
                company,
                through,
                schedules::RunOptions {
                    dry_run: *dry_run,
                    allow_large_run: *allow_large_run,
                },
            )?)
        }
        ScheduleCommand::Cancel { id } => {
            serde_json::to_value(schedules::cancel_schedule(db.conn(), &actor, company, id)?)
        }
    }
    .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
    render(cli, company, command, &data)
}

fn create_params(command: &ScheduleCommand) -> Result<schedules::CreateScheduleParams, CliError> {
    let ScheduleCommand::Create {
        description,
        start,
        frequency,
        end,
        debit,
        credit,
        currency,
        metadata,
        meta,
        tax,
    } = command
    else {
        return Err(CliError::Usage("expected schedule create".into()));
    };
    let parsed_currency = Currency::from_code(currency)
        .map_err(|e| CliError::Validation(format!("invalid currency: {e}")))?;
    let tax = super::txn::parse_tax_args(tax)?;
    let mut entries = Vec::new();
    for (args, direction) in [(debit, "debit"), (credit, "credit")] {
        for arg in args {
            let (account_code, amount, memo) = super::txn::parse_entry_arg(arg, parsed_currency)?;
            entries.push(PostEntryParams {
                tax_category: tax.get(&account_code).cloned(),
                account_code,
                direction: direction.into(),
                amount,
                memo,
            });
        }
    }
    let pairs = db::metadata::parse_meta_args(meta)?;
    let metadata = db::metadata::build_post_metadata(metadata.as_deref(), &pairs)?;
    Ok(schedules::CreateScheduleParams {
        description: description.clone(),
        currency: currency.clone(),
        start_date: *start,
        frequency: *frequency,
        end_date: *end,
        entries,
        metadata,
    })
}

fn render(
    cli: &Cli,
    company: &str,
    command: &ScheduleCommand,
    data: &Value,
) -> Result<(), CliError> {
    let format = resolve_format(None, cli);
    if cli.verbosity.quiet && format != OutputFormat::Json {
        return Ok(());
    }
    if !cli.verbosity.quiet && matches!(command, ScheduleCommand::Run { dry_run: true, .. }) {
        let count = data.as_array().map_or(0, Vec::len);
        eprintln!(
            "Preview: {count} due occurrences; no transactions posted or schedules advanced."
        );
    }
    if format == OutputFormat::Json {
        println!(
            "{}",
            json::envelope_ok(cli.meta(command.command_name(), Some(company)), data)?
        );
        return Ok(());
    }
    let rows = match data {
        Value::Array(rows) => rows.iter().collect::<Vec<_>>(),
        row => vec![row],
    };
    let occurrence = matches!(command, ScheduleCommand::Run { .. });
    let headers: &[&str] = if occurrence {
        &["schedule_id", "date", "transaction_id"]
    } else {
        &[
            "id",
            "description",
            "currency",
            "frequency",
            "next_due",
            "status",
        ]
    };
    match format {
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer
                .write_record(headers)
                .map_err(|e| CliError::General(e.to_string()))?;
            for row in rows {
                writer
                    .write_record(headers.iter().map(|key| cell(&row[*key])))
                    .map_err(|e| CliError::General(e.to_string()))?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => {
            let mut table = comfy_table::Table::new();
            table.set_header(headers.iter().copied());
            for row in rows {
                table.add_row(headers.iter().map(|key| cell(&row[*key])));
            }
            println!("{table}");
        }
        OutputFormat::Json => unreachable!("JSON handled above"),
    }
    Ok(())
}

fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}
