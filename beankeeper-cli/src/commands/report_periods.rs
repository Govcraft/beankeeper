//! Multi-column reports (issue #22).
//!
//! `report income-statement` and `report trial-balance` accept two optional
//! modes that turn the single-column report into a multi-column one:
//!
//! - `--period month|quarter|year` splits `--from`..`--to` into one column per
//!   calendar period (the first and last periods are clipped to the range) and
//!   adds a Total column.
//! - `--compare prior-period|prior-year` adds a column for the equivalent
//!   earlier range plus the change in amount and percent.
//!
//! Each account shows its net amount on its normal side (debit-normal for
//! assets and expenses, credit-normal for liabilities, equity and revenue).
//! The per-column amounts come from [`db::compute_trial_balance`], called once
//! per column window. Rendering lives in [`crate::output::report_periods`].
//!
//! When neither option is given, [`try_run`] returns `None` and the regular
//! single-column report runs unchanged.

use std::collections::HashMap;

use beankeeper::types::Currency;
use chrono::{Datelike, Months, NaiveDate};
use clap::ValueEnum;
use serde::Serialize;

use super::report_dates::{DateRange, parse_date};

use crate::cli::{AccountTypeArg, Cli, Command, OutputFormat, ReportCommand};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output::{self, chart};

/// Maximum number of period columns a single `--period` report may produce.
pub const MAX_PERIODS: usize = 120;

/// Calendar granularity for `--period`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeriodArg {
    /// One column per calendar month (labelled `2026-01`).
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
                    parse_date("--compare start", from)?,
                    parse_date("--compare end", to)?,
                )?))
            }
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
    pub account_type: String,
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

/// The columns for a computed report. A comparison always has both endpoints.
#[derive(Debug, Clone)]
pub(crate) enum ReportColumns {
    Periods(PeriodArg, Vec<Column>),
    Compare(CompareArg, [Column; 2]),
}

impl ReportColumns {
    fn as_slice(&self) -> &[Column] {
        match self {
            Self::Periods(_, columns) => columns,
            Self::Compare(_, columns) => columns,
        }
    }

    fn layout(&self) -> Layout {
        match self {
            Self::Periods(period, _) => Layout::Periods(*period),
            Self::Compare(compare, _) => Layout::Compare(*compare),
        }
    }
}

/// A fully computed multi-column report, ready for rendering.
#[derive(Debug, Clone)]
pub struct MultiColumnReport {
    pub kind: MultiReportKind,
    pub currency: Currency,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub accounts: Vec<AccountLine>,
    pub summary: Vec<SummaryLine>,
    columns: ReportColumns,
}

impl MultiColumnReport {
    /// Column layout used for rendering values.
    #[must_use]
    pub fn layout(&self) -> Layout {
        self.columns.layout()
    }

    /// Columns in display order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        self.columns.as_slice()
    }

    pub(crate) fn report_columns(&self) -> &ReportColumns {
        &self.columns
    }
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
    match (kind, layout) {
        (MultiReportKind::IncomeStatement, Layout::Periods(_)) => "report.income-statement.periods",
        (MultiReportKind::IncomeStatement, Layout::Compare(_)) => "report.income-statement.compare",
        (MultiReportKind::TrialBalance, Layout::Periods(_)) => "report.trial-balance.periods",
        (MultiReportKind::TrialBalance, Layout::Compare(_)) => "report.trial-balance.compare",
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

/// Run the multi-column form of a report when `--period` or `--compare` is set.
///
/// Returns `None` when the subcommand is not in a multi-column mode, so the
/// caller falls through to the regular single-column report.
#[must_use]
pub fn try_run(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    format: OutputFormat,
    use_color: bool,
) -> Option<Result<(), CliError>> {
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput::new(format, use_color, &mut stdout);
    try_run_to(cli, db, company, sub, &mut output)
}

pub(crate) fn try_run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut super::report::ReportOutput<'_>,
) -> Option<Result<(), CliError>> {
    let (kind, layout) = requested_layout(sub)?;
    let (from, to, account_type, currency) = match sub {
        ReportCommand::IncomeStatement {
            from, to, currency, ..
        } => (from, to, None, currency),
        ReportCommand::TrialBalance {
            from,
            to,
            account_type,
            currency,
            ..
        } => (from, to, account_type.as_ref(), currency),
        _ => return None,
    };
    Some(run(
        cli,
        db,
        company,
        &Request {
            kind,
            layout,
            from: from.as_deref(),
            to: to.as_deref(),
            account_type,
            currency: currency.as_deref(),
        },
        output,
    ))
}

/// Parsed inputs for a multi-column report.
struct Request<'a> {
    kind: MultiReportKind,
    layout: Layout,
    from: Option<&'a str>,
    to: Option<&'a str>,
    account_type: Option<&'a AccountTypeArg>,
    currency: Option<&'a str>,
}

fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    req: &Request<'_>,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let report = compute(db, company, req)?;

    match output.format {
        OutputFormat::Table => {
            writeln!(
                output.writer,
                "{}",
                output::report_periods::render_table(&report, output.use_color)
            )?;
        }
        OutputFormat::Json => {
            let id = mode_command_id(req.kind, req.layout);
            let meta = cli.meta(id, Some(company));
            let chart = chart::requested(cli, || {
                chart::periods(company, &report, cli.output.chart_all)
            })?;
            output.write_json(meta, output::report_periods::report_json(&report), chart)?;
        }
        OutputFormat::Csv => {
            write!(
                output.writer,
                "{}",
                output::report_periods::render_csv(&report)?
            )?;
        }
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] {} generated ({} columns)",
            req.kind.title().to_lowercase(),
            report.columns().len()
        );
    }
    Ok(())
}

fn compute(db: &Db, company: &str, req: &Request<'_>) -> Result<MultiColumnReport, CliError> {
    let dates = DateRange::resolve(req.from, req.to, chrono::Local::now().date_naive())?;
    let from = dates.from();
    let to = dates.to();
    let columns = match req.layout {
        Layout::Periods(period) => {
            ReportColumns::Periods(period, period_columns(from, to, period)?)
        }
        Layout::Compare(compare) => {
            let (prior_from, prior_to) = prior_range(from, to, compare)?;
            ReportColumns::Compare(
                compare,
                [
                    Column {
                        label: range_label(from, to),
                        from,
                        to,
                    },
                    Column {
                        label: range_label(prior_from, prior_to),
                        from: prior_from,
                        to: prior_to,
                    },
                ],
            )
        }
    };

    let currency =
        super::report_currency::resolve_report_currency(db.conn(), company, req.currency)?;
    let type_filter = req.account_type.map(|t| format!("{t:?}").to_lowercase());
    let (accounts, summary) = build_lines(
        db.conn(),
        company,
        req.kind,
        type_filter.as_deref(),
        currency.code(),
        columns.as_slice(),
    )?;
    Ok(MultiColumnReport {
        kind: req.kind,
        currency,
        from,
        to,
        columns,
        accounts,
        summary,
    })
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

/// Net amount of a balance row on its account's normal side.
#[must_use]
pub fn normal_side_net(row: &db::BalanceRow) -> i64 {
    match row.account_type.as_str() {
        "asset" | "expense" => row.debit_total.saturating_sub(row.credit_total),
        _ => row.credit_total.saturating_sub(row.debit_total),
    }
}

/// Fetch the balance rows for one column window.
pub(super) fn fetch_window_rows(
    conn: &rusqlite::Connection,
    company: &str,
    kind: MultiReportKind,
    type_filter: Option<&str>,
    currency: &str,
    (from, to): (NaiveDate, NaiveDate),
) -> Result<Vec<db::BalanceRow>, CliError> {
    let from = from.format("%Y-%m-%d").to_string();
    let to = to.format("%Y-%m-%d").to_string();
    let (from, to) = (from.as_str(), to.as_str());
    let rows = match kind {
        MultiReportKind::IncomeStatement => {
            let mut rows = db::compute_trial_balance(
                conn,
                company,
                Some("revenue"),
                Some(from),
                Some(to),
                Some(currency),
            )?;
            rows.extend(db::compute_trial_balance(
                conn,
                company,
                Some("expense"),
                Some(from),
                Some(to),
                Some(currency),
            )?);
            rows
        }
        MultiReportKind::TrialBalance => db::compute_trial_balance(
            conn,
            company,
            type_filter,
            Some(from),
            Some(to),
            Some(currency),
        )?,
    };
    Ok(rows)
}

fn amount_overflow() -> CliError {
    CliError::Validation("report amount exceeds the supported minor-unit range".into())
}

/// Fetch balances for every column window and pivot them into account lines
/// plus the report's summary lines.
fn build_lines(
    conn: &rusqlite::Connection,
    company: &str,
    kind: MultiReportKind,
    type_filter: Option<&str>,
    currency: &str,
    columns: &[Column],
) -> Result<(Vec<AccountLine>, Vec<SummaryLine>), CliError> {
    let n = columns.len();
    let mut accounts: Vec<AccountLine> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut debits = vec![0i64; n];
    let mut credits = vec![0i64; n];
    let mut revenue = vec![0i64; n];
    let mut expenses = vec![0i64; n];

    for (i, col) in columns.iter().enumerate() {
        let rows = fetch_window_rows(
            conn,
            company,
            kind,
            type_filter,
            currency,
            (col.from, col.to),
        )?;

        for row in &rows {
            let (positive, negative) = if matches!(row.account_type.as_str(), "asset" | "expense") {
                (row.debit_total, row.credit_total)
            } else {
                (row.credit_total, row.debit_total)
            };
            let net = positive.checked_sub(negative).ok_or_else(amount_overflow)?;
            debits[i] = debits[i]
                .checked_add(row.debit_total)
                .ok_or_else(amount_overflow)?;
            credits[i] = credits[i]
                .checked_add(row.credit_total)
                .ok_or_else(amount_overflow)?;
            match row.account_type.as_str() {
                "revenue" => {
                    revenue[i] = revenue[i].checked_add(net).ok_or_else(amount_overflow)?;
                }
                "expense" => {
                    expenses[i] = expenses[i].checked_add(net).ok_or_else(amount_overflow)?;
                }
                _ => {}
            }
            let pos = *index.entry(row.code.clone()).or_insert_with(|| {
                accounts.push(AccountLine {
                    code: row.code.clone(),
                    name: row.name.clone(),
                    account_type: row.account_type.clone(),
                    amounts: vec![0; n],
                });
                accounts.len() - 1
            });
            if let Some(line) = accounts.get_mut(pos) {
                line.amounts[i] = net;
            }
        }
    }

    // An account can be missing from some windows (no activity there), so
    // insertion order is not stable: sort like the single-column reports
    // (revenue before expenses on the income statement, then by code).
    accounts.sort_by(|a, b| {
        let rank = |l: &AccountLine| {
            u8::from(kind == MultiReportKind::IncomeStatement && l.account_type != "revenue")
        };
        rank(a).cmp(&rank(b)).then_with(|| a.code.cmp(&b.code))
    });

    let summary = match kind {
        MultiReportKind::IncomeStatement => {
            let net_income = revenue
                .iter()
                .zip(&expenses)
                .map(|(r, e)| r.checked_sub(*e).ok_or_else(amount_overflow))
                .collect::<Result<Vec<_>, _>>()?;
            vec![
                SummaryLine {
                    key: "total_revenue",
                    label: "Total Revenue",
                    amounts: revenue,
                },
                SummaryLine {
                    key: "total_expenses",
                    label: "Total Expenses",
                    amounts: expenses,
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
                amounts: debits,
            },
            SummaryLine {
                key: "total_credits",
                label: "Total Credits",
                amounts: credits,
            },
        ],
    };

    Ok((accounts, summary))
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
