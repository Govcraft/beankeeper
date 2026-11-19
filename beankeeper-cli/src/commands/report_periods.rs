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

use crate::cli::{AccountTypeArg, Cli, Command, OutputFormat, ReportCommand};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;

/// Maximum number of period columns a single `--period` report may produce.
pub const MAX_PERIODS: usize = 120;

/// Calendar granularity for `--period`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
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

/// Earlier range used by `--compare`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareArg {
    /// The range of equal length immediately before `--from`. Ranges made of
    /// whole calendar months shift back by that many months (Q2 -> Q1).
    PriorPeriod,
    /// The same range one year earlier.
    PriorYear,
}

impl CompareArg {
    /// Kebab-case name used in JSON output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PriorPeriod => "prior-period",
            Self::PriorYear => "prior-year",
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

/// A fully computed multi-column report, ready for rendering.
#[derive(Debug, Clone)]
pub struct MultiColumnReport {
    pub kind: MultiReportKind,
    pub layout: Layout,
    pub currency_code: String,
    pub minor_units: u8,
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
    let (kind, layout) = requested_layout(sub)?;
    let (from, to, account_type) = match sub {
        ReportCommand::IncomeStatement { from, to, .. } => (from, to, None),
        ReportCommand::TrialBalance {
            from,
            to,
            account_type,
            ..
        } => (from, to, account_type.as_ref()),
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
        },
        format,
        use_color,
    ))
}

/// Parsed inputs for a multi-column report.
struct Request<'a> {
    kind: MultiReportKind,
    layout: Layout,
    from: Option<&'a str>,
    to: Option<&'a str>,
    account_type: Option<&'a AccountTypeArg>,
}

fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    req: &Request<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let option = match req.layout {
        Layout::Periods(_) => "--period",
        Layout::Compare(_) => "--compare",
    };
    let (from, to) = parse_range(option, req.from, req.to)?;

    let columns = match req.layout {
        Layout::Periods(p) => period_columns(from, to, p)?,
        Layout::Compare(c) => {
            let (prior_from, prior_to) = prior_range(from, to, c)?;
            vec![
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
            ]
        }
    };

    let type_filter = req.account_type.map(|t| format!("{t:?}").to_lowercase());
    let (accounts, summary) = build_lines(
        db.conn(),
        company,
        req.kind,
        type_filter.as_deref(),
        &columns,
    )?;

    let currency = resolve_company_currency(db, company);
    let report = MultiColumnReport {
        kind: req.kind,
        layout: req.layout,
        currency_code: currency.code().to_string(),
        minor_units: currency.minor_units(),
        from,
        to,
        columns,
        accounts,
        summary,
    };

    match format {
        OutputFormat::Table => {
            println!(
                "{}",
                output::report_periods::render_table(&report, use_color)
            );
        }
        OutputFormat::Json => {
            let id = mode_command_id(req.kind, req.layout);
            let meta = output::json::meta(id, Some(company));
            println!("{}", output::report_periods::render_json(&report, meta)?);
        }
        OutputFormat::Csv => {
            print!("{}", output::report_periods::render_csv(&report)?);
        }
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] {} generated ({} columns)",
            req.kind.title().to_lowercase(),
            report.columns.len()
        );
    }
    Ok(())
}

/// Parse and validate `--from`/`--to`, both of which are required here.
fn parse_range(
    option: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(NaiveDate, NaiveDate), CliError> {
    let (Some(from), Some(to)) = (from, to) else {
        return Err(CliError::Validation(format!(
            "{option} requires both --from and --to (YYYY-MM-DD)"
        )));
    };
    let from = parse_date("--from", from)?;
    let to = parse_date("--to", to)?;
    if from > to {
        return Err(CliError::Validation(format!(
            "--from ({from}) must be on or before --to ({to})"
        )));
    }
    Ok((from, to))
}

fn parse_date(option: &str, value: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        CliError::Validation(format!(
            "invalid {option} date {value:?}: expected YYYY-MM-DD"
        ))
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

/// Fetch balances for every column window and pivot them into account lines
/// plus the report's summary lines.
fn build_lines(
    conn: &rusqlite::Connection,
    company: &str,
    kind: MultiReportKind,
    type_filter: Option<&str>,
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
        let from = col.from.format("%Y-%m-%d").to_string();
        let to = col.to.format("%Y-%m-%d").to_string();
        let rows = match kind {
            MultiReportKind::IncomeStatement => {
                let mut rows = db::compute_trial_balance(
                    conn,
                    company,
                    Some("revenue"),
                    Some(&from),
                    Some(&to),
                )?;
                rows.extend(db::compute_trial_balance(
                    conn,
                    company,
                    Some("expense"),
                    Some(&from),
                    Some(&to),
                )?);
                rows
            }
            MultiReportKind::TrialBalance => {
                db::compute_trial_balance(conn, company, type_filter, Some(&from), Some(&to))?
            }
        };

        for row in &rows {
            let net = normal_side_net(row);
            debits[i] = debits[i].saturating_add(row.debit_total);
            credits[i] = credits[i].saturating_add(row.credit_total);
            match row.account_type.as_str() {
                "revenue" => revenue[i] = revenue[i].saturating_add(net),
                "expense" => expenses[i] = expenses[i].saturating_add(net),
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
                .map(|(r, e)| r.saturating_sub(*e))
                .collect();
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

/// Resolve the reporting currency (most common transaction currency, USD
/// fallback) -- mirrors the single-column reports.
fn resolve_company_currency(db: &Db, company: &str) -> Currency {
    let result: Result<String, _> = db.conn().query_row(
        "SELECT currency FROM transactions WHERE company_slug = ?1 \
         GROUP BY currency ORDER BY COUNT(*) DESC LIMIT 1",
        rusqlite::params![company],
        |row| row.get(0),
    );
    match result {
        Ok(code) => Currency::from_code(&code).unwrap_or(Currency::USD),
        Err(_) => Currency::USD,
    }
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
