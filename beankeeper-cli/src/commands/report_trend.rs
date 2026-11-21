//! Historical spending activity and period-end net worth.

use std::collections::BTreeMap;

use beankeeper::types::{Amount, Currency};
use chrono::NaiveDate;
use clap::{Args, ValueEnum};
use comfy_table::{Cell, CellAlignment, Table};
use serde::Serialize;

use super::report::ReportOutput;
use super::report_dates::DateRange;
use super::report_periods::{Column, PeriodArg, period_columns};
use crate::cli::{Cli, OutputFormat};
use crate::db::{self, BalanceRow, connection::Db};
use crate::error::CliError;
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
    pub currency: Option<String>,
    /// Compare the full range with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD.
    #[arg(long)]
    pub compare: Option<super::report_periods::CompareArg>,
    /// Explain the change for one account code.
    #[arg(long, requires = "compare")]
    pub explain_change: Option<String>,
}

#[derive(Serialize)]
struct TrendColumn {
    label: String,
    from: NaiveDate,
    to: NaiveDate,
}

#[derive(Serialize)]
struct Series {
    code: Option<String>,
    name: String,
    amounts: Vec<i64>,
}

#[derive(Serialize)]
pub(crate) struct TrendReport {
    metric: TrendMetric,
    period: PeriodArg,
    from: NaiveDate,
    to: NaiveDate,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    columns: Vec<TrendColumn>,
    series: Vec<Series>,
}

impl TrendReport {
    pub(crate) fn metric(&self) -> TrendMetric {
        self.metric
    }

    pub(crate) fn period(&self) -> PeriodArg {
        self.period
    }

    pub(crate) fn currency(&self) -> Currency {
        self.currency
    }

    pub(crate) fn bounds(&self) -> (NaiveDate, NaiveDate) {
        (self.from, self.to)
    }

    pub(crate) fn labels(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|column| column.label.as_str())
    }

    pub(crate) fn amounts(&self) -> impl Iterator<Item = (&str, Option<&str>, &[i64])> {
        self.series.iter().map(|row| {
            (
                row.name.as_str(),
                row.code.as_deref(),
                row.amounts.as_slice(),
            )
        })
    }
}

pub(crate) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &TrendArgs,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let report = compute(db, company, args)?;
    match output.format {
        OutputFormat::Json => {
            let meta = cli
                .meta("report.trend", Some(company))
                .with_currency(report.currency);
            let chart =
                chart::requested(cli, || chart::trend(company, &report, cli.output.chart_all))?;
            output.write_json(meta, &report, chart)?;
        }
        OutputFormat::Table => writeln!(
            output.writer,
            "{}",
            render_table(&report, report.currency.minor_units())
        )?,
        OutputFormat::Csv => writeln!(output.writer, "{}", render_csv(&report)?)?,
    }
    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] {} generated ({} columns)",
            args.metric.title().to_lowercase(),
            report.columns.len()
        );
    }
    Ok(())
}

fn compute(db: &Db, company: &str, args: &TrendArgs) -> Result<TrendReport, CliError> {
    let dates = DateRange::resolve(
        args.from.as_deref(),
        args.to.as_deref(),
        chrono::Local::now().date_naive(),
    )?;
    let columns = period_columns(dates.from(), dates.to(), args.period)?;
    let currency = super::report_currency::resolve_report_currency(
        db.conn(),
        company,
        args.currency.as_deref(),
    )?;
    let rows = columns
        .iter()
        .map(|column| column_rows(db, company, args.metric, currency.code(), column));
    let series = match args.metric {
        TrendMetric::Spending => spending_series(rows)?,
        TrendMetric::NetWorth => net_worth_series(rows)?,
    };
    Ok(TrendReport {
        metric: args.metric,
        period: args.period,
        from: dates.from(),
        to: dates.to(),
        currency,
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
}

fn column_rows(
    db: &Db,
    company: &str,
    metric: TrendMetric,
    currency: &str,
    column: &Column,
) -> Result<Vec<BalanceRow>, CliError> {
    let from = column.from.to_string();
    let to = column.to.to_string();
    let (filter, from) = match metric {
        TrendMetric::Spending => (Some("expense"), Some(from.as_str())),
        TrendMetric::NetWorth => (None, None),
    };
    db::compute_trial_balance(db.conn(), company, filter, from, Some(&to), Some(currency))
}

fn overflow() -> CliError {
    CliError::Validation("trend amount exceeds the supported minor-unit range".to_string())
}

fn net(row: &BalanceRow) -> Result<i64, CliError> {
    let (positive, negative) = if row.account_type == "liability" {
        (row.credit_total, row.debit_total)
    } else {
        (row.debit_total, row.credit_total)
    };
    positive.checked_sub(negative).ok_or_else(overflow)
}

fn total<'a>(rows: impl Iterator<Item = &'a BalanceRow>) -> Result<i64, CliError> {
    rows.map(net).try_fold(0_i64, |sum, amount| {
        sum.checked_add(amount?).ok_or_else(overflow)
    })
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
            let amount = net(&row)?;
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
        let rows = rows?;
        let assets = total(rows.iter().filter(|row| row.account_type == "asset"))?;
        let liabilities = total(rows.iter().filter(|row| row.account_type == "liability"))?;
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

fn render_table(report: &TrendReport, minor_units: u8) -> String {
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
        cells.extend(series.amounts.iter().map(|amount| {
            Cell::new(Amount::new(i128::from(*amount)).format_decimal(minor_units))
                .set_alignment(CellAlignment::Right)
        }));
        table.add_row(cells);
    }
    format!(
        "{} by {} ({} to {}) ({})\n\n{table}",
        report.metric.title(),
        report.period.as_str(),
        report.from,
        report.to,
        report.currency
    )
}

fn render_csv(report: &TrendReport) -> Result<String, CliError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    let mut header = vec!["code".to_string(), "name".to_string()];
    header.extend(report.columns.iter().map(|column| column.label.clone()));
    writer
        .write_record(header)
        .map_err(|error| csv_error(&error))?;
    for series in &report.series {
        let mut row = vec![series.code.clone().unwrap_or_default(), series.name.clone()];
        row.extend(series.amounts.iter().map(ToString::to_string));
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

    fn row(kind: &str, debit_total: i64, credit_total: i64) -> BalanceRow {
        BalanceRow {
            code: "1000".into(),
            name: "Account".into(),
            account_type: kind.into(),
            debit_total,
            credit_total,
        }
    }

    #[test]
    fn aggregation_rejects_amount_overflow() {
        assert!(net(&row("expense", i64::MIN, 1)).is_err());
        assert!(total([row("asset", i64::MAX, 0), row("asset", 1, 0)].iter()).is_err());
        let rows = vec![row("asset", i64::MAX, 0), row("liability", 1, 0)];
        assert!(net_worth_series(std::iter::once(Ok(rows))).is_err());
    }

    #[test]
    fn contra_balances_preserve_signs() -> Result<(), CliError> {
        let rows = vec![row("asset", 0, 10), row("liability", 20, 0)];
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
            ..row("expense", amount, 0)
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
    fn chart_rejects_a_trend_series_that_does_not_fill_its_axis() {
        let date = NaiveDate::default();
        let report = TrendReport {
            metric: TrendMetric::NetWorth,
            period: PeriodArg::Month,
            from: date,
            to: date,
            currency: Currency::KWD,
            columns: vec![TrendColumn {
                label: "2024-01".into(),
                from: date,
                to: date,
            }],
            series: vec![Series {
                code: None,
                name: "Net worth".into(),
                amounts: vec![],
            }],
        };
        assert!(chart::trend("co", &report, false).is_err());
    }
}
