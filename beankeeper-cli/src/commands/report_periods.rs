//! Multi-column reports (issue #22).
//!
//! `--period month|quarter|year` splits `--from`..`--to` into columns for
//! calendar periods and adds a Total column. Comparison requests use the
//! shared typed comparison implementation.
//!
//! Each account shows its net amount on its normal side (debit-normal for
//! assets and expenses, credit-normal for liabilities, equity and revenue).
//! The per-column amounts come from [`db::compute_trial_balance`], called once
//! per column window. Rendering lives in [`crate::output::report_periods`].
//!
//! Single-column and comparison modes use separate typed requests.

use std::collections::BTreeMap;

use beankeeper::types::{AccountType, Currency};
use chrono::{Datelike, Months, NaiveDate};
use clap::ValueEnum;
use serde::Serialize;

use super::report::{ReportOutput, ReportPresentation};
use super::report_dates::DateRange;
use crate::request::CalendarDateArg;

use crate::cli::{Command, OutputFormat, ReportCommand};
use crate::db;
use crate::db::companies::Company;
use crate::error::CliError;
use crate::operation::Operation;
use crate::output::{self, chart};

/// Maximum number of period columns a single `--period` report may produce.
pub const MAX_PERIODS: usize = 120;

/// Calendar granularity for `--period`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PeriodArg {
    /// One column per calendar month (labelled `2026-01`).
    #[default]
    Month,
    /// One column per calendar quarter (labelled `2026-Q1`).
    Quarter,
    /// One column per calendar year (labelled `2026`).
    Year,
}

impl PeriodArg {
    /// Lowercase name used in JSON output and titles.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }
}

/// An inclusive comparison range whose start cannot follow its end.
///
/// Private fields prevent callers from bypassing [`Self::new`].
///
/// ```compile_fail
/// use beankeeper_cli::commands::report_periods::ComparisonRange;
/// use chrono::NaiveDate;
///
/// let range = ComparisonRange {
///     from: NaiveDate::default(),
///     to: NaiveDate::default(),
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonRange {
    from: NaiveDate,
    to: NaiveDate,
}

impl ComparisonRange {
    /// Construct an inclusive range, rejecting reversed boundaries.
    ///
    /// # Errors
    /// Returns a validation error when `from` is after `to`.
    pub fn new(from: NaiveDate, to: NaiveDate) -> Result<Self, CliError> {
        if from > to {
            return Err(CliError::Validation(format!(
                "comparison start ({from}) must be on or before its end ({to})"
            )));
        }
        Ok(Self { from, to })
    }
}

/// Earlier range used by `--compare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareArg {
    /// The range of equal length immediately before `--from`. Ranges made of
    /// whole calendar months shift back by that many months (Q2 -> Q1).
    PriorPeriod,
    /// The same range one year earlier.
    PriorYear,
    /// An explicit inclusive comparison window.
    Custom(ComparisonRange),
}

impl CompareArg {
    /// Kebab-case name used in JSON output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PriorPeriod => "prior-period",
            Self::PriorYear => "prior-year",
            Self::Custom(..) => "custom",
        }
    }
}

impl std::str::FromStr for CompareArg {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "previous" | "prior-period" => Ok(Self::PriorPeriod),
            "year-ago" | "prior-year" => Ok(Self::PriorYear),
            _ => {
                let (from, to) = value.split_once("..").ok_or_else(|| {
                    CliError::Usage(
                        "--compare expects previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD".into(),
                    )
                })?;
                Ok(Self::Custom(ComparisonRange::new(
                    from.parse::<CalendarDateArg>()?.into(),
                    to.parse::<CalendarDateArg>()?.into(),
                )?))
            }
        }
    }
}

impl std::fmt::Display for CompareArg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PriorPeriod | Self::PriorYear => formatter.write_str(self.as_str()),
            Self::Custom(bounds) => write!(formatter, "{}..{}", bounds.from, bounds.to),
        }
    }
}

/// Which report is being rendered in multi-column form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiReportKind {
    /// Revenue and expense accounts with a Net Income row.
    IncomeStatement,
    /// All accounts (optionally filtered by type) with debit/credit totals.
    TrialBalance,
}

impl MultiReportKind {
    /// Kebab-case report name (used in JSON and command ids).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncomeStatement => "income-statement",
            Self::TrialBalance => "trial-balance",
        }
    }

    /// Human-readable report title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::IncomeStatement => "Income Statement",
            Self::TrialBalance => "Trial Balance",
        }
    }
}

/// Column layout of a multi-column report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One column per period, followed by a Total column.
    Periods(PeriodArg),
    /// Current and prior columns, followed by Change and Change %.
    Compare(CompareArg),
}

/// A single amount column and the date window it covers (inclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub label: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// One account row: net amount on its normal side for each column.
#[derive(Debug, Clone)]
pub struct AccountLine {
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    pub amounts: Vec<i64>,
}

/// A summary row (e.g. Net Income, Total Debits) with one value per column.
#[derive(Debug, Clone)]
pub struct SummaryLine {
    /// Machine-readable key (JSON field / CSV name).
    pub key: &'static str,
    /// Human-readable label.
    pub label: &'static str,
    pub amounts: Vec<i64>,
}

/// A fully computed multi-column report, ready for rendering.
#[derive(Debug, Clone)]
pub struct MultiColumnReport {
    pub kind: MultiReportKind,
    pub period: PeriodArg,
    pub currency: Currency,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub columns: Vec<Column>,
    pub accounts: Vec<AccountLine>,
    pub summary: Vec<SummaryLine>,
}

/// Command id for the multi-column modes, or `None` for any other command.
#[must_use]
pub fn command_id(cmd: &Command) -> Option<&'static str> {
    let Command::Report(args) = cmd else {
        return None;
    };
    let (kind, layout) = requested_layout(&args.command)?;
    Some(mode_command_id(kind, layout))
}

/// Command id for a report kind rendered with a multi-column layout.
#[must_use]
pub fn mode_command_id(kind: MultiReportKind, layout: Layout) -> &'static str {
    mode_operation(kind, layout).name()
}

fn mode_operation(kind: MultiReportKind, layout: Layout) -> Operation {
    match (kind, layout) {
        (MultiReportKind::IncomeStatement, Layout::Periods(_)) => {
            Operation::ReportIncomeStatementPeriods
        }
        (MultiReportKind::IncomeStatement, Layout::Compare(_)) => {
            Operation::ReportIncomeStatementCompare
        }
        (MultiReportKind::TrialBalance, Layout::Periods(_)) => Operation::ReportTrialBalancePeriods,
        (MultiReportKind::TrialBalance, Layout::Compare(_)) => Operation::ReportTrialBalanceCompare,
    }
}

/// The multi-column layout requested by a report subcommand, if any.
fn requested_layout(sub: &ReportCommand) -> Option<(MultiReportKind, Layout)> {
    let (kind, period, compare) = match sub {
        ReportCommand::IncomeStatement {
            period, compare, ..
        } => (MultiReportKind::IncomeStatement, period, compare),
        ReportCommand::TrialBalance {
            period, compare, ..
        } => (MultiReportKind::TrialBalance, period, compare),
        _ => return None,
    };
    match (period, compare) {
        (Some(p), _) => Some((kind, Layout::Periods(*p))),
        (None, Some(c)) => Some((kind, Layout::Compare(*c))),
        (None, None) => None,
    }
}

pub(super) fn compute(
    scope: &Company<'_>,
    kind: MultiReportKind,
    period: PeriodArg,
    dates: DateRange,
    currency: Currency,
    account_type: Option<AccountType>,
) -> Result<MultiColumnReport, CliError> {
    let (from, to) = (dates.from(), dates.to());
    let columns = period_columns(from, to, period)?;
    let (accounts, summary) = build_lines(scope, kind, account_type, currency, &columns)?;

    let report = MultiColumnReport {
        kind,
        period,
        currency,
        from,
        to,
        columns,
        accounts,
        summary,
    };

    output::report_periods::validate_totals(&report)?;
    Ok(report)
}

pub(super) fn render(
    context: &ReportPresentation<'_>,
    report: &MultiColumnReport,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            writeln!(
                output.writer,
                "{}",
                output::report_periods::render_table(report, output.use_color)
            )?;
        }
        OutputFormat::Json => {
            let chart = context
                .chart
                .project(|all| chart::periods(context.company, report, all))?;
            output.write_json(
                context.meta(),
                output::report_periods::report_json(report),
                chart,
            )?;
        }
        OutputFormat::Csv => {
            write!(
                output.writer,
                "{}",
                output::report_periods::render_csv(report)?
            )?;
        }
    }

    if !context.quiet {
        eprintln!(
            "[ok] {} generated ({} columns)",
            report.kind.title().to_lowercase(),
            report.columns.len()
        );
    }
    Ok(())
}

fn date_overflow() -> CliError {
    CliError::Validation("date is out of the supported range".to_string())
}

/// First day of the month `months` after the month containing `date`.
fn first_of_month_after(date: NaiveDate, months: u32) -> Option<NaiveDate> {
    date.with_day(1)?.checked_add_months(Months::new(months))
}

/// Last day of the calendar period (month/quarter/year) containing `date`.
fn period_end(date: NaiveDate, period: PeriodArg) -> Option<NaiveDate> {
    let month = date.month();
    let end_month = match period {
        PeriodArg::Month => month,
        PeriodArg::Quarter => month.div_ceil(3) * 3,
        PeriodArg::Year => 12,
    };
    first_of_month_after(date, end_month - month + 1)?.pred_opt()
}

/// Label for the calendar period containing `date`.
fn period_label(date: NaiveDate, period: PeriodArg) -> String {
    match period {
        PeriodArg::Month => date.format("%Y-%m").to_string(),
        PeriodArg::Quarter => format!("{}-Q{}", date.year(), date.month().div_ceil(3)),
        PeriodArg::Year => date.year().to_string(),
    }
}

/// Split `from..=to` into calendar periods, clipping the first and last.
///
/// # Errors
///
/// Returns `CliError::Validation` if the range yields more than
/// [`MAX_PERIODS`] columns or a date overflows.
pub fn period_columns(
    from: NaiveDate,
    to: NaiveDate,
    period: PeriodArg,
) -> Result<Vec<Column>, CliError> {
    let mut columns = Vec::new();
    let mut start = from;
    loop {
        let end = period_end(start, period).ok_or_else(date_overflow)?.min(to);
        columns.push(Column {
            label: period_label(start, period),
            from: start,
            to: end,
        });
        if columns.len() > MAX_PERIODS {
            return Err(CliError::Validation(format!(
                "--period {} over {from}..{to} yields more than {MAX_PERIODS} columns; \
                 narrow the range or use a coarser period",
                period.as_str()
            )));
        }
        if end >= to {
            break;
        }
        start = end.succ_opt().ok_or_else(date_overflow)?;
    }
    Ok(columns)
}

/// Whether `from..=to` spans whole calendar months exactly.
fn is_whole_months(from: NaiveDate, to: NaiveDate) -> bool {
    from.day() == 1 && to.succ_opt().is_some_and(|next| next.day() == 1)
}

/// Number of calendar months touched by `from..=to`.
fn month_span(from: NaiveDate, to: NaiveDate) -> u32 {
    let months = (to.year() - from.year()) * 12 + i32::try_from(to.month()).unwrap_or(0)
        - i32::try_from(from.month()).unwrap_or(0)
        + 1;
    u32::try_from(months).unwrap_or(0)
}

/// The comparison range for `--compare`.
///
/// `prior-year` shifts both ends back twelve months (Feb 29 clamps to Feb 28).
/// `prior-period` takes the range of equal length ending the day before
/// `from`; when the range is made of whole calendar months it shifts back by
/// that many months instead, so `2026-04-01..2026-06-30` compares with
/// `2026-01-01..2026-03-31`.
///
/// # Errors
///
/// Returns `CliError::Validation` if a date overflows.
pub fn prior_range(
    from: NaiveDate,
    to: NaiveDate,
    compare: CompareArg,
) -> Result<(NaiveDate, NaiveDate), CliError> {
    match compare {
        CompareArg::Custom(range) => Ok((range.from, range.to)),
        CompareArg::PriorYear => {
            let year = Months::new(12);
            Ok((
                from.checked_sub_months(year).ok_or_else(date_overflow)?,
                to.checked_sub_months(year).ok_or_else(date_overflow)?,
            ))
        }
        CompareArg::PriorPeriod => {
            let prior_to = from.pred_opt().ok_or_else(date_overflow)?;
            let prior_from = if is_whole_months(from, to) {
                from.checked_sub_months(Months::new(month_span(from, to)))
            } else {
                prior_to.checked_sub_signed(to.signed_duration_since(from))
            }
            .ok_or_else(date_overflow)?;
            Ok((prior_from, prior_to))
        }
    }
}

/// A compact label for a date range: `2026`, `2026-Q1` or `2026-01` when the
/// range is exactly that calendar period, otherwise `from..to`.
#[must_use]
pub fn range_label(from: NaiveDate, to: NaiveDate) -> String {
    if is_whole_months(from, to) && from.year() == to.year() {
        let (fm, tm) = (from.month(), to.month());
        if fm == 1 && tm == 12 {
            return from.year().to_string();
        }
        if fm % 3 == 1 && tm == fm + 2 {
            return format!("{}-Q{}", from.year(), fm.div_ceil(3));
        }
        if fm == tm {
            return from.format("%Y-%m").to_string();
        }
    }
    format!("{from}..{to}")
}

/// Fetch the balance rows for one column window.
pub(super) fn fetch_window_rows(
    company: &Company<'_>,
    kind: MultiReportKind,
    type_filter: Option<AccountType>,
    currency: Currency,
    (from, to): (NaiveDate, NaiveDate),
) -> Result<Vec<db::BalanceRow>, CliError> {
    let rows = match kind {
        MultiReportKind::IncomeStatement => {
            let mut rows = db::compute_trial_balance(
                company,
                Some(AccountType::Revenue),
                Some(from),
                Some(to),
                currency,
            )?;
            rows.extend(db::compute_trial_balance(
                company,
                Some(AccountType::Expense),
                Some(from),
                Some(to),
                currency,
            )?);
            rows
        }
        MultiReportKind::TrialBalance => {
            db::compute_trial_balance(company, type_filter, Some(from), Some(to), currency)?
        }
    };
    Ok(rows)
}

fn report_overflow() -> CliError {
    CliError::Validation("report amount exceeds the supported minor-unit range".into())
}

#[derive(Default)]
struct ColumnTotals {
    debit: i64,
    credit: i64,
    revenue: i64,
    expense: i64,
}

impl ColumnTotals {
    fn add(&mut self, row: &db::BalanceRow, net: i64) -> Result<(), CliError> {
        self.debit = self
            .debit
            .checked_add(row.debit_total)
            .ok_or_else(report_overflow)?;
        self.credit = self
            .credit
            .checked_add(row.credit_total)
            .ok_or_else(report_overflow)?;
        match row.account_type {
            AccountType::Revenue => {
                self.revenue = self.revenue.checked_add(net).ok_or_else(report_overflow)?;
            }
            AccountType::Expense => {
                self.expense = self.expense.checked_add(net).ok_or_else(report_overflow)?;
            }
            AccountType::Asset | AccountType::Liability | AccountType::Equity => {}
        }
        Ok(())
    }
}

/// Net amount of a balance row on its account's normal side.
///
/// # Errors
/// Returns validation when the net does not fit in a report amount.
pub fn normal_side_net(row: &db::BalanceRow) -> Result<i64, CliError> {
    let (positive, negative) = if row.account_type.is_debit_normal() {
        (row.debit_total, row.credit_total)
    } else {
        (row.credit_total, row.debit_total)
    };
    positive.checked_sub(negative).ok_or_else(report_overflow)
}

fn summary_lines(
    kind: MultiReportKind,
    columns: &[ColumnTotals],
) -> Result<Vec<SummaryLine>, CliError> {
    let lines = match kind {
        MultiReportKind::IncomeStatement => {
            let net_income = columns
                .iter()
                .map(|c| c.revenue.checked_sub(c.expense).ok_or_else(report_overflow))
                .collect::<Result<_, _>>()?;
            vec![
                SummaryLine {
                    key: "total_revenue",
                    label: "Total Revenue",
                    amounts: columns.iter().map(|c| c.revenue).collect(),
                },
                SummaryLine {
                    key: "total_expenses",
                    label: "Total Expenses",
                    amounts: columns.iter().map(|c| c.expense).collect(),
                },
                SummaryLine {
                    key: "net_income",
                    label: "Net Income",
                    amounts: net_income,
                },
            ]
        }
        MultiReportKind::TrialBalance => vec![
            SummaryLine {
                key: "total_debits",
                label: "Total Debits",
                amounts: columns.iter().map(|c| c.debit).collect(),
            },
            SummaryLine {
                key: "total_credits",
                label: "Total Credits",
                amounts: columns.iter().map(|c| c.credit).collect(),
            },
        ],
    };
    Ok(lines)
}

/// Fetch balances for every window and pivot them by account identity.
fn build_lines(
    company: &Company<'_>,
    kind: MultiReportKind,
    type_filter: Option<AccountType>,
    currency: Currency,
    columns: &[Column],
) -> Result<(Vec<AccountLine>, Vec<SummaryLine>), CliError> {
    let mut accounts = BTreeMap::<String, AccountLine>::new();
    let mut totals = Vec::with_capacity(columns.len());
    for col in columns {
        let rows = fetch_window_rows(company, kind, type_filter, currency, (col.from, col.to))?;
        let mut subtotals = ColumnTotals::default();
        for row in rows {
            let net = normal_side_net(&row)?;
            subtotals.add(&row, net)?;
            let line = accounts
                .entry(row.code.clone())
                .or_insert_with(|| AccountLine {
                    code: row.code,
                    name: row.name,
                    account_type: row.account_type,
                    amounts: Vec::with_capacity(columns.len()),
                });
            let completed = totals.len();
            if line.amounts.len() > completed {
                return Err(CliError::Database(format!(
                    "duplicate account in report period: {}",
                    line.code
                )));
            }
            line.amounts.resize(completed, 0);
            line.amounts.push(net);
        }
        totals.push(subtotals);
    }

    let mut accounts: Vec<AccountLine> = accounts
        .into_values()
        .map(|mut row| {
            row.amounts.resize(columns.len(), 0);
            row
        })
        .collect();
    if kind == MultiReportKind::IncomeStatement {
        // Codes are already ordered; keep revenues before expenses.
        accounts.sort_by_key(|line| line.account_type != AccountType::Revenue);
    }
    Ok((accounts, summary_lines(kind, &totals)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
    }

    fn labels(cols: &[Column]) -> Vec<String> {
        cols.iter().map(|c| c.label.clone()).collect()
    }

    #[test]
    fn monthly_columns_are_clipped() -> Result<(), CliError> {
        let cols = period_columns(d("2026-01-15"), d("2026-03-10"), PeriodArg::Month)?;
        assert_eq!(labels(&cols), ["2026-01", "2026-02", "2026-03"]);
        assert_eq!(cols[0].from, d("2026-01-15"));
        assert_eq!(cols[0].to, d("2026-01-31"));
        assert_eq!(cols[1].to, d("2026-02-28"));
        assert_eq!(cols[2].from, d("2026-03-01"));
        assert_eq!(cols[2].to, d("2026-03-10"));
        Ok(())
    }

    #[test]
    fn quarterly_and_yearly_columns() -> Result<(), CliError> {
        let q = period_columns(d("2025-11-01"), d("2026-05-31"), PeriodArg::Quarter)?;
        assert_eq!(labels(&q), ["2025-Q4", "2026-Q1", "2026-Q2"]);
        assert_eq!(q[1].from, d("2026-01-01"));
        assert_eq!(q[1].to, d("2026-03-31"));
        assert_eq!(q[2].to, d("2026-05-31"));

        let y = period_columns(d("2024-07-01"), d("2026-02-01"), PeriodArg::Year)?;
        assert_eq!(labels(&y), ["2024", "2025", "2026"]);
        assert_eq!(y[0].to, d("2024-12-31"));
        Ok(())
    }

    #[test]
    fn too_many_periods_is_rejected() {
        let r = period_columns(d("2000-01-01"), d("2026-12-31"), PeriodArg::Month);
        assert!(matches!(r, Err(CliError::Validation(_))));
    }

    #[test]
    fn prior_ranges() -> Result<(), CliError> {
        assert_eq!(
            prior_range(d("2026-04-01"), d("2026-06-30"), CompareArg::PriorPeriod)?,
            (d("2026-01-01"), d("2026-03-31"))
        );
        assert_eq!(
            prior_range(d("2026-03-01"), d("2026-03-31"), CompareArg::PriorPeriod)?,
            (d("2026-02-01"), d("2026-02-28"))
        );
        assert_eq!(
            prior_range(d("2026-01-11"), d("2026-01-20"), CompareArg::PriorPeriod)?,
            (d("2026-01-01"), d("2026-01-10"))
        );
        assert_eq!(
            prior_range(d("2024-02-01"), d("2024-02-29"), CompareArg::PriorYear)?,
            (d("2023-02-01"), d("2023-02-28"))
        );
        Ok(())
    }

    #[test]
    fn custom_ranges_validate_order_and_preserve_inclusive_boundaries() -> Result<(), CliError> {
        let start = d("2024-02-01");
        let end = d("2024-02-29");
        assert!(ComparisonRange::new(end, start).is_err());
        assert!("2024-02-29..2024-02-01".parse::<CompareArg>().is_err());
        assert!("2024-02-30..2024-03-01".parse::<CompareArg>().is_err());
        for (input, from, to) in [
            ("2024-02-01..2024-02-29", start, end),
            ("2024-02-29..2024-02-29", end, end),
        ] {
            let comparison = input.parse::<CompareArg>()?;
            assert_eq!(
                comparison,
                CompareArg::Custom(ComparisonRange::new(from, to)?)
            );
            assert_eq!(prior_range(start, end, comparison)?, (from, to));
        }
        Ok(())
    }

    #[test]
    fn range_labels() {
        assert_eq!(range_label(d("2026-01-01"), d("2026-12-31")), "2026");
        assert_eq!(range_label(d("2026-04-01"), d("2026-06-30")), "2026-Q2");
        assert_eq!(range_label(d("2026-02-01"), d("2026-02-28")), "2026-02");
        assert_eq!(
            range_label(d("2026-02-01"), d("2026-03-31")),
            "2026-02-01..2026-03-31"
        );
        assert_eq!(
            range_label(d("2026-01-05"), d("2026-01-31")),
            "2026-01-05..2026-01-31"
        );
    }
}
