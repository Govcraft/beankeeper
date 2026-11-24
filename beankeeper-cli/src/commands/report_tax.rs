//! Tax-oriented book summaries built from existing entry tags and metadata.

mod schedule;
mod vendors;

use beankeeper::types::{Currency, DebitOrCredit};
use clap::Args;
use rusqlite::params;
use serde::Serialize;

use super::report::ReportOutput;
use super::report::ReportPresentation;
use super::txn::MajorAmount;
use crate::cli::OutputFormat;
use crate::db::{
    companies::Company,
    connection::{Db, ReadConn},
};
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
    pub from: Option<crate::cli::CalendarDateArg>,
    /// Last date (inclusive). Defaults to today.
    #[arg(long)]
    pub to: Option<crate::cli::CalendarDateArg>,
    /// Currency; required for companies with transactions in multiple currencies.
    #[arg(long)]
    pub currency: Option<Currency>,
    /// Report format (also accepts the global --format or --json options).
    #[arg(long, value_enum)]
    pub output_format: Option<OutputFormat>,
}

/// Vendor payment summary options. No IRS form is generated or filed.
#[derive(Debug, Clone, Args)]
pub struct VendorArgs {
    #[command(flatten)]
    pub period: TaxArgs,
    /// Minimum total in major currency units. Defaults to 600. Use 0 to list every payee.
    #[arg(long, allow_hyphen_values = true)]
    pub threshold: Option<VendorMinimum>,
}

/// Nonnegative vendor-report minimum before a reporting currency is chosen.
#[derive(Debug, Clone)]
pub struct VendorMinimum(MajorAmount);

impl TryFrom<MajorAmount> for VendorMinimum {
    type Error = CliError;

    fn try_from(amount: MajorAmount) -> Result<Self, Self::Error> {
        if amount.sign().is_lt() {
            return Err(CliError::Validation(
                "--threshold must be nonnegative".into(),
            ));
        }
        Ok(Self(amount))
    }
}

impl std::str::FromStr for VendorMinimum {
    type Err = CliError;

    fn from_str(amount: &str) -> Result<Self, Self::Err> {
        amount.parse::<MajorAmount>()?.try_into()
    }
}

impl Default for VendorMinimum {
    fn default() -> Self {
        Self(MajorAmount::from(600u32))
    }
}

impl std::fmt::Display for VendorMinimum {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

struct TaxEntry {
    transaction_id: i64,
    metadata: Option<String>,
    account_code: String,
    account_name: String,
    account_type: beankeeper::types::AccountType,
    tax_category: Option<String>,
    direction: DebitOrCredit,
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
    db.read_snapshot(|reader| {
        let company = Company::bind(reader, company.parse()?)?;
        build_schedule_c_on(reader, &company, period, currency)
    })
}

pub(super) fn build_schedule_c_on(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    period: Period,
    currency: Currency,
) -> Result<TaxReport<ScheduleSummary>, CliError> {
    let entries = load_entries(reader, company.slug().as_str(), period, currency)?;
    Ok(TaxReport {
        period,
        currency,
        summary: schedule::build(&entries, currency)?,
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
    db.read_snapshot(|reader| {
        let company = Company::bind(reader, company.parse()?)?;
        build_vendors_on(reader, &company, period, threshold)
    })
}

pub(super) fn build_vendors_on(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    period: Period,
    threshold: VendorThreshold,
) -> Result<TaxReport<VendorSummary>, CliError> {
    let currency = threshold.money().currency();
    let entries = load_entries(reader, company.slug().as_str(), period, currency)?;
    Ok(TaxReport {
        period,
        currency,
        summary: vendors::build(&entries, threshold)?,
    })
}

pub(super) fn render_schedule(
    context: &ReportPresentation<'_>,
    report: &TaxReport<ScheduleSummary>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    render_tax(
        context,
        report,
        output,
        schedule::render_table,
        schedule::render_csv,
    )
}

pub(super) fn render_vendors(
    context: &ReportPresentation<'_>,
    report: &TaxReport<VendorSummary>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    render_tax(
        context,
        report,
        output,
        vendors::render_table,
        vendors::render_csv,
    )
}

fn render_tax<T: Serialize>(
    context: &ReportPresentation<'_>,
    report: &TaxReport<T>,
    output: &mut ReportOutput<'_>,
    table: impl FnOnce(&TaxReport<T>) -> String,
    csv: impl FnOnce(&TaxReport<T>) -> Result<String, CliError>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Json => output.write_json(
            context.meta().with_currency(report.currency()),
            report,
            None,
        )?,
        OutputFormat::Csv => writeln!(output.writer, "{}", csv(report)?)?,
        OutputFormat::Table => writeln!(output.writer, "{}", table(report))?,
    }
    if !context.quiet {
        eprintln!("[ok] tax book summary generated");
    }
    Ok(())
}

fn load_entries(
    reader: ReadConn<'_>,
    company: &str,
    period: Period,
    currency: Currency,
) -> Result<Vec<TaxEntry>, CliError> {
    let mut stmt = reader.prepare(
        "SELECT t.id, t.metadata, a.code, a.name, a.type, e.tax_category, e.direction, e.amount
         FROM posted_transactions t JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug
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
