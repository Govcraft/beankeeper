//! Tax-oriented book summaries built from existing entry tags and metadata.

mod schedule;
mod vendors;

use beankeeper::types::{Currency, Money};
use clap::Args;
use rusqlite::params;
use serde::Serialize;

use super::report::ReportOutput;
use crate::cli::{Cli, OutputFormat, ReportCommand};
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output::json::serialize_display;

pub use super::report_dates::DateRange as Period;
pub use schedule::{ScheduleRow, ScheduleSummary, render_csv as render_schedule_c_csv};
pub use vendors::{VendorRow, VendorSummary, VendorThreshold, render_csv as render_vendors_csv};

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

/// A read-only tax book summary with explicit dates and a single currency.
#[derive(Serialize)]
pub struct TaxReport<T> {
    #[serde(flatten)]
    period: Period,
    #[serde(serialize_with = "serialize_display")]
    currency: Currency,
    #[serde(flatten)]
    summary: T,
}

impl<T> TaxReport<T> {
    /// Inclusive dates covered by the summary.
    #[must_use]
    pub const fn period(&self) -> Period {
        self.period
    }

    /// Currency of every monetary amount in the summary.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Report-specific rows and totals.
    #[must_use]
    pub const fn summary(&self) -> &T {
        &self.summary
    }
}

/// Compute the Schedule C book summary for an already-open ledger.
///
/// # Errors
/// Returns [`CliError`] for an unknown company, database failures, or report
/// amounts exceeding the supported minor-unit range.
pub fn build_schedule_c(
    db: &Db,
    company: &str,
    period: Period,
    currency: Currency,
) -> Result<TaxReport<ScheduleSummary>, CliError> {
    crate::db::audit::atomically(db.conn(), || {
        crate::db::companies::get_company(db.conn(), company)?;
        let entries = load_entries(db, company, period, currency)?;
        Ok(TaxReport {
            period,
            currency,
            summary: schedule::build(&entries, currency)?,
        })
    })
}

/// Compute the 1099 vendor book summary for an already-open ledger.
///
/// The threshold fixes the report currency. Only vendors that meet the
/// threshold appear in the result.
///
/// # Errors
/// Returns [`CliError`] for an unknown company, conflicting vendor metadata,
/// database failures, or report amounts exceeding the supported range.
pub fn build_vendors(
    db: &Db,
    company: &str,
    period: Period,
    threshold: VendorThreshold,
) -> Result<TaxReport<VendorSummary>, CliError> {
    crate::db::audit::atomically(db.conn(), || {
        crate::db::companies::get_company(db.conn(), company)?;
        let currency = threshold.money().currency();
        let entries = load_entries(db, company, period, currency)?;
        Ok(TaxReport {
            period,
            currency,
            summary: vendors::build(&entries, threshold)?,
        })
    })
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
    let period = Period::resolve(
        args.from.as_deref(),
        args.to.as_deref(),
        chrono::Local::now().date_naive(),
    )?;
    match sub {
        ReportCommand::ScheduleC(_) => {
            let report = build_schedule_c(db, company, period, currency)?;
            match output.format {
                OutputFormat::Json => {
                    let meta = cli
                        .meta("report.schedule-c", Some(company))
                        .with_currency(currency);
                    output.write_json(meta, &report, None)?;
                }
                OutputFormat::Csv => writeln!(output.writer, "{}", schedule::render_csv(&report)?)?,
                OutputFormat::Table => {
                    writeln!(output.writer, "{}", schedule::render_table(&report))?;
                }
            }
        }
        ReportCommand::Vendor1099(args) => {
            let threshold = super::txn::parse_amount_to_minor(&args.threshold, currency)?;
            let threshold =
                VendorThreshold::new(Money::from_minor(i128::from(threshold), currency))?;
            let report = build_vendors(db, company, period, threshold)?;
            match output.format {
                OutputFormat::Json => {
                    let meta = cli
                        .meta("report.1099", Some(company))
                        .with_currency(currency);
                    output.write_json(meta, &report, None)?;
                }
                OutputFormat::Csv => writeln!(output.writer, "{}", vendors::render_csv(&report)?)?,
                OutputFormat::Table => {
                    writeln!(output.writer, "{}", vendors::render_table(&report))?;
                }
            }
        }
        _ => return Err(CliError::General("internal error: not a tax report".into())),
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] tax book summary generated");
    }
    Ok(())
}

fn load_entries(
    db: &Db,
    company: &str,
    period: Period,
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
        params![
            company,
            currency.code(),
            period.from().to_string(),
            period.to().to_string()
        ],
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

fn overflow() -> CliError {
    CliError::Validation("tax summary exceeds the supported minor-unit range".into())
}

fn add(total: &mut i64, amount: i64) -> Result<(), CliError> {
    *total = total.checked_add(amount).ok_or_else(overflow)?;
    Ok(())
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
