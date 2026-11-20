//! Tax-oriented book summaries built from existing entry tags and metadata.

mod schedule;
mod vendors;

use beankeeper::types::{Amount, Currency};
use clap::Args;
use rusqlite::params;
use serde::Serialize;

use super::report::ReportOutput;
use crate::cli::{Cli, OutputFormat, ReportCommand};
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output::json::{Envelope, EnvelopeError};

/// Shared period, currency, and export options for tax-oriented reports.
#[derive(Debug, Clone, Args)]
pub struct TaxArgs {
    /// First date (inclusive). Defaults to January 1 of the end date's year.
    #[arg(long)]
    pub from: Option<String>,
    /// Last date (inclusive). Defaults to today.
    #[arg(long)]
    pub to: Option<String>,
    /// Currency; required for companies with transactions in multiple currencies.
    #[arg(long)]
    pub currency: Option<String>,
    /// Report format (also accepts the global --format or --json options).
    #[arg(long, value_enum)]
    pub output_format: Option<OutputFormat>,
}

/// Vendor payment summary options. No IRS form is generated or filed.
#[derive(Debug, Clone, Args)]
pub struct VendorArgs {
    #[command(flatten)]
    pub period: TaxArgs,
    /// Minimum total in major currency units. Use 0 to list every payee.
    #[arg(long, default_value = "600", allow_hyphen_values = true)]
    pub threshold: String,
}

struct TaxEntry {
    transaction_id: i64,
    metadata: Option<String>,
    account_code: String,
    account_name: String,
    account_type: String,
    tax_category: Option<String>,
    direction: String,
    amount: i64,
}

#[derive(Serialize)]
struct TaxReport<T> {
    from: String,
    to: String,
    currency: String,
    #[serde(flatten)]
    summary: T,
}

pub(crate) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let args = match sub {
        ReportCommand::ScheduleC(args) => args,
        ReportCommand::Vendor1099(args) => &args.period,
        _ => return Err(CliError::General("internal error: not a tax report".into())),
    };
    let currency = super::report_currency::resolve_report_currency(
        db.conn(),
        company,
        args.currency.as_deref(),
    )?;
    let entries = load_entries(db, company, args, currency)?;
    let rendered = match sub {
        ReportCommand::ScheduleC(_) => {
            let report = wrap(args, currency, schedule::build(&entries)?)?;
            match output.format {
                OutputFormat::Json => {
                    render_json(cli, company, "report.schedule-c", currency, &report)?
                }
                OutputFormat::Csv => schedule::render_csv(&report)?,
                OutputFormat::Table => schedule::render_table(&report, currency),
            }
        }
        ReportCommand::Vendor1099(args) => {
            let threshold = super::txn::parse_amount_to_minor(&args.threshold, currency)?;
            if threshold < 0 {
                return Err(CliError::Validation(
                    "--threshold must be nonnegative".into(),
                ));
            }
            let report = wrap(&args.period, currency, vendors::build(&entries, threshold)?)?;
            match output.format {
                OutputFormat::Json => render_json(cli, company, "report.1099", currency, &report)?,
                OutputFormat::Csv => vendors::render_csv(&report)?,
                OutputFormat::Table => vendors::render_table(&report, currency),
            }
        }
        _ => return Err(CliError::General("internal error: not a tax report".into())),
    };
    writeln!(output.writer, "{rendered}")?;
    if !cli.verbosity.quiet {
        eprintln!("[ok] tax book summary generated");
    }
    Ok(())
}

fn load_entries(
    db: &Db,
    company: &str,
    args: &TaxArgs,
    currency: Currency,
) -> Result<Vec<TaxEntry>, CliError> {
    let mut stmt = db.conn().prepare(
        "SELECT t.id, t.metadata, a.code, a.name, a.type, e.tax_category, e.direction, e.amount
         FROM transactions t JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug
         JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND t.date >= ?3 AND t.date <= ?4
         ORDER BY t.id, e.id",
    )?;
    let rows = stmt.query_map(
        params![company, currency.code(), args.from, args.to],
        |row| {
            Ok(TaxEntry {
                transaction_id: row.get(0)?,
                metadata: row.get(1)?,
                account_code: row.get(2)?,
                account_name: row.get(3)?,
                account_type: row.get(4)?,
                tax_category: row.get(5)?,
                direction: row.get(6)?,
                amount: row.get(7)?,
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

fn wrap<T>(args: &TaxArgs, currency: Currency, summary: T) -> Result<TaxReport<T>, CliError> {
    Ok(TaxReport {
        from: args
            .from
            .clone()
            .ok_or_else(|| CliError::General("tax report start date was not resolved".into()))?,
        to: args
            .to
            .clone()
            .ok_or_else(|| CliError::General("tax report end date was not resolved".into()))?,
        currency: currency.code().into(),
        summary,
    })
}

fn render_json<T: Serialize>(
    cli: &Cli,
    company: &str,
    command: &str,
    currency: Currency,
    report: &T,
) -> Result<String, CliError> {
    serde_json::to_string_pretty(&Envelope {
        ok: true,
        meta: cli.meta(command, Some(company)).with_currency(currency),
        data: Some(report),
        error: None::<EnvelopeError>,
    })
    .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

fn overflow() -> CliError {
    CliError::Validation("tax summary exceeds the supported minor-unit range".into())
}

fn add(total: &mut i64, amount: i64) -> Result<(), CliError> {
    *total = total.checked_add(amount).ok_or_else(overflow)?;
    Ok(())
}

fn decimal(amount: i64, currency: Currency) -> String {
    Amount::new(i128::from(amount)).format_decimal(currency.minor_units())
}

fn finish_csv(writer: csv::Writer<Vec<u8>>) -> Result<String, CliError> {
    let bytes = writer
        .into_inner()
        .map_err(|error| CliError::General(format!("CSV flush failed: {error}")))?;
    String::from_utf8(bytes)
        .map_err(|error| CliError::General(format!("invalid CSV UTF-8: {error}")))
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {error}"))
}
