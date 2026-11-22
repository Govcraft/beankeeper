//! Pending obligations, explicit posting, and recurring transaction templates.

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use super::posting_output::PostingLineJson;

use crate::cli::{Cli, OutputFormat, ScheduleCommand, ScheduleCreateArgs, resolve_format};
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
        ScheduleCommand::Create(args) => {
            let params = create_params(args)?;
            let schedule = schedules::create_schedule(db.conn(), &actor, company, &params)?;
            serde_json::to_value(ScheduleJson::from(&schedule))
        }
        ScheduleCommand::List { all, due_through } => {
            let rows: Vec<_> = schedules::list_schedules(db.conn(), company)?
                .into_iter()
                .filter(|row| *all || row.state.next_due().is_some())
                .filter(|row| {
                    due_through.is_none_or(|cutoff| {
                        row.state.next_due().is_some_and(|date| date <= cutoff)
                    })
                })
                .collect();
            serde_json::to_value(rows.iter().map(ScheduleJson::from).collect::<Vec<_>>())
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
            let schedule = schedules::cancel_schedule(db.conn(), &actor, company, id)?;
            serde_json::to_value(ScheduleJson::from(&schedule))
        }
    }
    .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
    render(cli, company, command, &data)
}

#[derive(Serialize)]
struct ScheduleJson<'a> {
    id: &'a schedules::ScheduleId,
    company_slug: &'a str,
    description: &'a str,
    currency: &'static str,
    start_date: NaiveDate,
    #[serde(serialize_with = "json::serialize_display")]
    frequency: beankeeper::core::Frequency,
    end_date: Option<NaiveDate>,
    #[serde(flatten)]
    state: schedules::ScheduleState,
    entries: Vec<PostingLineJson<'a>>,
    metadata: Option<&'a str>,
    occurrence_index: u32,
}

impl<'a> From<&'a schedules::Schedule> for ScheduleJson<'a> {
    fn from(schedule: &'a schedules::Schedule) -> Self {
        Self {
            id: &schedule.id,
            company_slug: &schedule.company_slug,
            description: &schedule.description,
            currency: schedule.currency.code(),
            start_date: schedule.start_date,
            frequency: schedule.frequency,
            end_date: schedule.end_date,
            state: schedule.state,
            entries: PostingLineJson::for_entries(&schedule.entries, Some(schedule.currency)),
            metadata: schedule.metadata.as_deref(),
            occurrence_index: schedule.occurrence_index,
        }
    }
}

fn create_params(args: &ScheduleCreateArgs) -> Result<schedules::CreateScheduleParams, CliError> {
    let ScheduleCreateArgs {
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
    } = args;
    let currency = currency.resolve()?;
    let tax = super::txn::parse_tax_args(tax)?;
    let mut entries = Vec::new();
    for (args, direction) in [
        (debit, beankeeper::types::DebitOrCredit::Debit),
        (credit, beankeeper::types::DebitOrCredit::Credit),
    ] {
        for arg in args {
            let (account_code, amount, memo) = super::txn::parse_entry_arg(arg, currency)?;
            entries.push(PostEntryParams {
                tax_category: tax.get(&account_code).cloned(),
                account_code,
                direction,
                amount,
                memo,
            });
        }
    }
    let pairs = db::metadata::parse_meta_args(meta)?;
    let metadata = db::metadata::build_post_metadata(metadata.as_deref(), &pairs)?;
    Ok(schedules::CreateScheduleParams {
        description: description.clone(),
        currency,
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
    let rows = match data {
        Value::Array(rows) => rows.as_slice(),
        row => std::slice::from_ref(row),
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
        OutputFormat::Json => {
            println!(
                "{}",
                json::envelope_ok(cli.meta(command.operation(), Some(company)), data)?
            );
        }
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
