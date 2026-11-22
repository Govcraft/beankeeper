//! Historical spending activity and period-end net worth.

use crate::operation::Operation;
use std::collections::BTreeMap;

use beankeeper::types::{AccountType, Currency, Money};
use chrono::NaiveDate;
use clap::{Args, ValueEnum};
use comfy_table::{Cell, CellAlignment, Table};
use serde::{Serialize, Serializer};

use super::report::ReportOutput;
use super::report_dates::DateRange;
use super::report_periods::{Column, PeriodArg, period_columns};
use crate::cli::{Cli, OutputFormat};
use crate::db::{self, BalanceRow, connection::Db};
use crate::error::CliError;
use crate::output::amounts::{MoneySequence, format_money};
use crate::output::chart;

/// Financial quantity tracked over calendar periods.
#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrendMetric {
    /// Net expense activity by expense account, including refunds.
    Spending,
    /// Cumulative assets minus liabilities at each period end.
    NetWorth,
}

impl TrendMetric {
    fn title(self) -> &'static str {
        match self {
            Self::Spending => "Spending Trends",
            Self::NetWorth => "Net Worth Trends",
        }
    }
}

/// Options for `bk report trend`.
#[derive(Debug, Clone, Args)]
pub struct TrendArgs {
    /// Quantity to track: spending by expense account or period-end net worth.
    #[arg(long, value_enum)]
    pub metric: TrendMetric,
    /// First date (inclusive). Defaults to January 1 of the end date's year.
    #[arg(long)]
    pub from: Option<String>,
    /// Last date (inclusive). Defaults to today.
    #[arg(long)]
    pub to: Option<String>,
    /// Calendar granularity; first and last periods are clipped to the range.
    #[arg(long, value_enum, default_value = "month")]
    pub period: PeriodArg,
    /// Transaction currency. Required for companies with multiple currencies.
    #[arg(long)]
    pub currency: Option<crate::cli::CurrencyArg>,
    /// Compare the full range with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD.
    #[arg(long)]
    pub compare: Option<super::report_periods::CompareArg>,
    /// Explain the change for one account code.
    #[arg(long, requires = "compare")]
    pub explain_change: Option<String>,
}

/// Typed inputs to a trend computation on an already-open ledger.
#[derive(Debug, Clone, Copy)]
pub struct TrendRequest {
    /// Inclusive date window.
    pub dates: DateRange,
    /// Currency of balances and movement.
    pub currency: Currency,
    /// Financial quantity to track.
    pub metric: TrendMetric,
    /// Calendar granularity for values.
    pub period: PeriodArg,
}

/// One inclusive calendar interval on the report's axis.
#[derive(Debug, Serialize)]
pub struct TrendColumn {
    /// Interval label.
    pub label: String,
    /// First included day.
    pub from: NaiveDate,
    /// Last included day.
    pub to: NaiveDate,
}

/// An account or summary series ordered by report column.
#[derive(Debug)]
pub struct TrendSeries {
    code: Option<String>,
    name: String,
    amounts: Vec<Money>,
}

impl TrendSeries {
    /// Account code when this series represents a ledger account.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Account or summary label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Currency-bearing values in the report's column order.
    #[must_use]
    pub fn amounts(&self) -> &[Money] {
        &self.amounts
    }
}

impl Serialize for TrendSeries {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Row<'a> {
            code: Option<&'a str>,
            name: &'a str,
            #[serde(flatten)]
            amounts: MoneySequence<'a>,
        }
        Row {
            code: self.code(),
            name: self.name(),
            amounts: MoneySequence::new("amounts", self.amounts()),
        }
        .serialize(serializer)
    }
}

/// Historical spending or net worth in the request's currency.
#[derive(Debug)]
pub struct TrendReport {
    request: TrendRequest,
    columns: Vec<TrendColumn>,
    series: Vec<TrendSeries>,
}

impl TrendReport {
    /// Selected metric.
    #[must_use]
    pub const fn metric(&self) -> TrendMetric {
        self.request.metric
    }

    /// Calendar granularity.
    #[must_use]
    pub const fn period(&self) -> PeriodArg {
        self.request.period
    }

    /// Currency of all values.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.request.currency
    }

    /// Resolved inclusive date window.
    #[must_use]
    pub const fn dates(&self) -> DateRange {
        self.request.dates
    }

    /// Calendar intervals in series order.
    #[must_use]
    pub fn columns(&self) -> &[TrendColumn] {
        &self.columns
    }

    /// Series with one value per column, in stable report order.
    #[must_use]
    pub fn series(&self) -> &[TrendSeries] {
        &self.series
    }

    pub(crate) fn labels(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|column| column.label.as_str())
    }
}

impl Serialize for TrendReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Data<'a> {
            metric: TrendMetric,
            period: PeriodArg,
            #[serde(flatten)]
            dates: DateRange,
            #[serde(serialize_with = "crate::output::json::serialize_display")]
            currency: Currency,
            columns: &'a [TrendColumn],
            series: &'a [TrendSeries],
        }
        Data {
            metric: self.metric(),
            period: self.period(),
            dates: self.dates(),
            currency: self.currency(),
            columns: self.columns(),
            series: self.series(),
        }
        .serialize(serializer)
    }
}

struct Series {
    code: Option<String>,
    name: String,
    amounts: Vec<i64>,
}

pub(super) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    request: TrendRequest,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let report = compute_trend(db, company, request)?;
    match output.format {
        OutputFormat::Json => {
            let meta = cli
                .meta(Operation::ReportTrend, Some(company))
                .with_currency(report.currency());
            let chart =
                chart::requested(cli, || chart::trend(company, &report, cli.output.chart_all))?;
            output.write_json(meta, &report, chart)?;
        }
        OutputFormat::Table => writeln!(output.writer, "{}", render_table(&report))?,
        OutputFormat::Csv => writeln!(output.writer, "{}", render_csv(&report)?)?,
    }
    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] {} generated ({} columns)",
            request.metric.title().to_lowercase(),
            report.columns.len()
        );
    }
    Ok(())
}

impl TrendArgs {
    pub(super) fn resolve(
        &self,
        db: &Db,
        company: &str,
        today: NaiveDate,
    ) -> Result<TrendRequest, CliError> {
        let dates = DateRange::resolve(self.from.as_deref(), self.to.as_deref(), today)?;
        let currency = super::report_currency::resolve_report_currency(
            db.conn(),
            company,
            crate::cli::CurrencyArg::resolve_optional(self.currency.as_ref())?,
        )?;
        Ok(TrendRequest {
            dates,
            currency,
            metric: self.metric,
            period: self.period,
        })
    }
}

/// Compute a trend in one currency on an already-open ledger.
///
/// # Errors
/// Returns [`CliError`] for unknown companies, unsupported dates, arithmetic
/// overflow, or database failures.
pub fn compute_trend(
    db: &Db,
    company: &str,
    request: TrendRequest,
) -> Result<TrendReport, CliError> {
    db::audit::atomically(db.conn(), || {
        db::companies::get_company(db.conn(), company)?;
        let columns = period_columns(request.dates.from(), request.dates.to(), request.period)?;
        let rows = columns
            .iter()
            .map(|column| column_rows(db, company, request.metric, request.currency, column));
        let raw = match request.metric {
            TrendMetric::Spending => spending_series(rows)?,
            TrendMetric::NetWorth => net_worth_series(rows)?,
        };
        let series = raw
            .into_iter()
            .map(|series| TrendSeries {
                code: series.code,
                name: series.name,
                amounts: series
                    .amounts
                    .into_iter()
                    .map(|minor| Money::from_minor(i128::from(minor), request.currency))
                    .collect(),
            })
            .collect();
        Ok(TrendReport {
            request,
            columns: columns
                .into_iter()
                .map(|column| TrendColumn {
                    label: column.label,
                    from: column.from,
                    to: column.to,
                })
                .collect(),
            series,
        })
    })
}

fn column_rows(
    db: &Db,
    company: &str,
    metric: TrendMetric,
    currency: Currency,
    column: &Column,
) -> Result<Vec<BalanceRow>, CliError> {
    let from = column.from.to_string();
    let to = column.to.to_string();
    let (filter, from) = match metric {
        TrendMetric::Spending => (Some(AccountType::Expense), Some(from.as_str())),
        TrendMetric::NetWorth => (None, None),
    };
    db::compute_trial_balance(db.conn(), company, filter, from, Some(&to), currency)
}

fn overflow() -> CliError {
    CliError::Validation("trend amount exceeds the supported minor-unit range".to_string())
}

fn net(row: &BalanceRow, account_type: AccountType) -> Result<i64, CliError> {
    let (positive, negative) = if account_type.is_debit_normal() {
        (row.debit_total, row.credit_total)
    } else {
        (row.credit_total, row.debit_total)
    };
    positive.checked_sub(negative).ok_or_else(overflow)
}

fn worth_totals(rows: &[BalanceRow]) -> Result<(i64, i64), CliError> {
    let mut assets = 0_i64;
    let mut liabilities = 0_i64;
    for row in rows {
        let kind = row.account_type;
        let total = match kind {
            AccountType::Asset => &mut assets,
            AccountType::Liability => &mut liabilities,
            AccountType::Equity | AccountType::Revenue | AccountType::Expense => continue,
        };
        *total = total.checked_add(net(row, kind)?).ok_or_else(overflow)?;
    }
    Ok((assets, liabilities))
}

struct AccountTrend {
    name: String,
    amounts: Vec<i64>,
}

impl Series {
    fn summary(name: &str, amounts: Vec<i64>) -> Self {
        Self {
            code: None,
            name: name.into(),
            amounts,
        }
    }
}

fn spending_series(
    columns: impl Iterator<Item = Result<Vec<BalanceRow>, CliError>>,
) -> Result<Vec<Series>, CliError> {
    let mut accounts = BTreeMap::<String, AccountTrend>::new();
    let mut totals = Vec::new();
    for rows in columns {
        let completed_periods = totals.len();
        let mut total = 0_i64;
        for row in rows? {
            let amount = net(&row, AccountType::Expense)?;
            let series = accounts.entry(row.code).or_insert_with(|| AccountTrend {
                name: row.name,
                amounts: Vec::new(),
            });
            if series.amounts.len() > completed_periods {
                return Err(CliError::Database(format!(
                    "duplicate account in spending trend period: {}",
                    series.name
                )));
            }
            series.amounts.resize(completed_periods, 0);
            series.amounts.push(amount);
            total = total.checked_add(amount).ok_or_else(overflow)?;
        }
        totals.push(total);
    }

    let mut series: Vec<Series> = accounts
        .into_iter()
        .map(|(code, mut account)| {
            account.amounts.resize(totals.len(), 0);
            Series {
                code: Some(code),
                name: account.name,
                amounts: account.amounts,
            }
        })
        .collect();
    series.push(Series::summary("Total Spending", totals));
    Ok(series)
}

fn net_worth_series(
    columns: impl Iterator<Item = Result<Vec<BalanceRow>, CliError>>,
) -> Result<Vec<Series>, CliError> {
    let mut asset_series = Vec::new();
    let mut liability_series = Vec::new();
    let mut net_worth_series = Vec::new();
    for rows in columns {
        let (assets, liabilities) = worth_totals(&rows?)?;
        asset_series.push(assets);
        liability_series.push(liabilities);
        net_worth_series.push(assets.checked_sub(liabilities).ok_or_else(overflow)?);
    }
    Ok(vec![
        Series::summary("Total Assets", asset_series),
        Series::summary("Total Liabilities", liability_series),
        Series::summary("Net Worth", net_worth_series),
    ])
}

fn render_table(report: &TrendReport) -> String {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    let mut headers = vec!["Code".to_string(), "Category".to_string()];
    headers.extend(report.columns.iter().map(|column| column.label.clone()));
    table.set_header(headers);
    for series in &report.series {
        let mut cells = vec![
            Cell::new(series.code.as_deref().unwrap_or("")),
            Cell::new(&series.name),
        ];
        cells.extend(
            series
                .amounts
                .iter()
                .map(|amount| Cell::new(format_money(*amount)).set_alignment(CellAlignment::Right)),
        );
        table.add_row(cells);
    }
    format!(
        "{} by {} ({} to {}) ({})\n\n{table}",
        report.metric().title(),
        report.period().as_str(),
        report.dates().from(),
        report.dates().to(),
        report.currency()
    )
}

/// Export a typed trend report with minor-unit integers.
///
/// # Errors
/// Returns [`CliError`] if CSV serialization fails.
pub fn render_csv(report: &TrendReport) -> Result<String, CliError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    let mut header = vec!["code".to_string(), "name".to_string()];
    header.extend(report.columns.iter().map(|column| column.label.clone()));
    writer
        .write_record(header)
        .map_err(|error| csv_error(&error))?;
    for series in &report.series {
        let mut row = vec![series.code.clone().unwrap_or_default(), series.name.clone()];
        row.extend(
            series
                .amounts()
                .iter()
                .map(|money| money.amount().minor_units().to_string()),
        );
        writer
            .write_record(row)
            .map_err(|error| csv_error(&error))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|error| CliError::General(format!("CSV flush failed: {error}")))?;
    String::from_utf8(bytes)
        .map_err(|error| CliError::General(format!("invalid CSV UTF-8: {error}")))
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: AccountType, debit_total: i64, credit_total: i64) -> BalanceRow {
        BalanceRow {
            code: "1000".into(),
            name: "Account".into(),
            account_type: kind,
            debit_total,
            credit_total,
        }
    }

    #[test]
    fn aggregation_rejects_amount_overflow() {
        assert!(
            net(
                &row(AccountType::Expense, i64::MIN, 1),
                AccountType::Expense
            )
            .is_err()
        );
        assert!(
            worth_totals(&[
                row(AccountType::Asset, i64::MAX, 0),
                row(AccountType::Asset, 1, 0)
            ])
            .is_err()
        );
        let rows = vec![
            row(AccountType::Asset, i64::MAX, 0),
            row(AccountType::Liability, 1, 0),
        ];
        assert!(net_worth_series(std::iter::once(Ok(rows))).is_err());
    }

    #[test]
    fn contra_balances_preserve_signs() -> Result<(), CliError> {
        let rows = vec![
            row(AccountType::Asset, 0, 10),
            row(AccountType::Liability, 20, 0),
        ];
        let series = net_worth_series(std::iter::once(Ok(rows)))?;
        assert_eq!(series[0].amounts, [-10]);
        assert_eq!(series[1].amounts, [-20]);
        assert_eq!(series[2].amounts, [10]);
        Ok(())
    }

    #[test]
    fn spending_aligns_reordered_added_and_missing_accounts() -> Result<(), CliError> {
        let expense = |code: &str, amount| BalanceRow {
            code: code.to_string(),
            name: code.to_string(),
            ..row(AccountType::Expense, amount, 0)
        };
        let columns = [
            vec![expense("5000", 10), expense("5100", 20)],
            vec![
                expense("5100", 200),
                expense("5000", 100),
                expense("5200", 300),
            ],
            vec![expense("5200", 3000), expense("5100", 2000)],
        ];
        let series = spending_series(columns.into_iter().map(Ok))?;
        let rows: Vec<_> = series
            .iter()
            .map(|series| (series.code.as_deref(), series.amounts.as_slice()))
            .collect();
        assert_eq!(
            rows,
            [
                (Some("5000"), &[10, 100, 0][..]),
                (Some("5100"), &[20, 200, 2000][..]),
                (Some("5200"), &[0, 300, 3000][..]),
                (None, &[30, 600, 5000][..]),
            ]
        );
        Ok(())
    }

    #[test]
    fn chart_rejects_a_trend_series_that_does_not_fill_its_axis() -> Result<(), CliError> {
        let date = NaiveDate::default();
        let report = TrendReport {
            request: TrendRequest {
                metric: TrendMetric::NetWorth,
                period: PeriodArg::Month,
                dates: DateRange::new(date, date)?,
                currency: Currency::KWD,
            },
            columns: vec![TrendColumn {
                label: "2024-01".into(),
                from: date,
                to: date,
            }],
            series: vec![TrendSeries {
                code: None,
                name: "Net worth".into(),
                amounts: vec![],
            }],
        };
        assert!(chart::trend("co", &report, false).is_err());
        Ok(())
    }
}
