//! Typed adapters at the existing report JSON boundary.

use beankeeper::types::Currency;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Chart, Context, Kind, Row, invalid_source};
use crate::cli::{Cli, Command, ReportCommand};
use crate::error::CliError;

#[derive(Clone, Deserialize, Serialize)]
struct Bounds {
    from: NaiveDate,
    to: NaiveDate,
}

impl Bounds {
    fn label(&self) -> String {
        format!("{}..{}", self.from, self.to)
    }
}

#[derive(Clone, Serialize)]
struct Comparison {
    compare_from: NaiveDate,
    compare_to: NaiveDate,
}

#[derive(Clone, Serialize)]
pub(super) struct RangeState {
    company: String,
    currency: String,
    #[serde(flatten)]
    bounds: Bounds,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    comparison: Option<Comparison>,
}

#[derive(Serialize)]
pub(super) struct TrendState {
    #[serde(flatten)]
    range: RangeState,
    period: Period,
    metric: Metric,
}

#[derive(Serialize)]
pub(super) struct CashState {
    #[serde(flatten)]
    range: RangeState,
    cash_account: Vec<String>,
}

/// View variants prevent sending trend-only filters into a register or report.
#[derive(Serialize)]
#[serde(tag = "view_id", content = "state", rename_all = "kebab-case")]
pub(super) enum Target {
    Transactions(RangeState),
    IncomeStatement(RangeState),
    Trend(TrendState),
    CashFlow(CashState),
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Metric {
    Spending,
    NetWorth,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Period {
    Month,
    Quarter,
    Year,
}

#[derive(Deserialize)]
struct Column {
    label: String,
}

#[derive(Deserialize)]
struct TrendRow {
    code: Option<String>,
    name: String,
    amounts: Vec<i64>,
}

#[derive(Deserialize)]
struct Trend {
    #[serde(flatten)]
    bounds: Bounds,
    metric: Metric,
    period: Period,
    columns: Vec<Column>,
    series: Vec<TrendRow>,
}

#[derive(Deserialize)]
struct Category {
    code: Option<String>,
    name: String,
    amount: i64,
}

impl From<Category> for Row {
    fn from(row: Category) -> Self {
        Self {
            name: row.name,
            values: vec![i128::from(row.amount)],
            account: row.code,
        }
    }
}

#[derive(Deserialize)]
struct Section {
    lines: Vec<Category>,
}

#[derive(Deserialize)]
struct CashAccount {
    code: String,
}

#[derive(Deserialize)]
struct CashFlow {
    #[serde(flatten)]
    bounds: Bounds,
    cash_accounts: Vec<CashAccount>,
    operating: Section,
    investing: Section,
    financing: Section,
}

#[derive(Deserialize)]
struct IncomeSection {
    key: String,
    accounts: Vec<Category>,
}

#[derive(Deserialize)]
struct Income {
    #[serde(flatten)]
    bounds: Bounds,
    sections: Vec<IncomeSection>,
}

#[derive(Deserialize)]
struct PeriodRow {
    code: String,
    name: String,
    #[serde(default)]
    amounts: Vec<i64>,
    current: Option<i64>,
    prior: Option<i64>,
}

#[derive(Deserialize)]
struct PeriodReport {
    #[serde(flatten)]
    bounds: Bounds,
    columns: Option<Vec<Column>>,
    current: Option<Bounds>,
    prior: Option<Bounds>,
    accounts: Vec<PeriodRow>,
}

#[derive(Deserialize)]
struct SummaryPeriods {
    current: Bounds,
    prior: Bounds,
}

#[derive(Deserialize)]
struct SummaryRow {
    label: String,
    current: i64,
    prior: i64,
}

#[derive(Deserialize)]
struct SummaryComparison {
    series: Vec<SummaryRow>,
}

#[derive(Deserialize)]
struct Summary {
    periods: SummaryPeriods,
    comparison: SummaryComparison,
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, CliError> {
    serde_json::from_value(value.clone()).map_err(|error| invalid_source(&error.to_string()))
}

fn currency(envelope: &Value) -> Result<Currency, CliError> {
    let code = envelope["data"]["currency"]
        .as_str()
        .or_else(|| envelope["meta"]["currency"].as_str())
        .ok_or_else(|| invalid_source("report has no currency"))?;
    Currency::from_code(code).map_err(|error| invalid_source(&error.to_string()))
}

fn range(company: &str, currency: Currency, bounds: Bounds, prior: Option<Bounds>) -> RangeState {
    RangeState {
        company: company.into(),
        currency: currency.code().into(),
        bounds,
        comparison: prior.map(|prior| Comparison {
            compare_from: prior.from,
            compare_to: prior.to,
        }),
    }
}

pub(super) fn project(cli: &Cli, company: &str, envelope: &Value) -> Result<Chart, CliError> {
    let currency = currency(envelope)?;
    let data = &envelope["data"];
    let all = cli.output.chart_all;
    match &cli.command {
        Command::Summary(_) => {
            let summary: Summary = parse(data)?;
            let x = vec![
                summary.periods.prior.label(),
                summary.periods.current.label(),
            ];
            let target = Target::IncomeStatement(range(
                company,
                currency,
                summary.periods.current,
                Some(summary.periods.prior),
            ));
            let rows = summary
                .comparison
                .series
                .into_iter()
                .map(|row| Row {
                    name: row.label,
                    values: vec![i128::from(row.prior), i128::from(row.current)],
                    account: None,
                })
                .collect();
            Chart::new(
                Kind::Bar,
                "Income, spending and money left over",
                x,
                rows,
                Context {
                    currency,
                    target,
                    all,
                },
            )
        }
        Command::Report(args) => match &args.command {
            ReportCommand::Trend(_) => trend(data, company, currency, all),
            ReportCommand::CashFlow(_) => {
                let cash: CashFlow = parse(data)?;
                let target = Target::CashFlow(CashState {
                    range: range(company, currency, cash.bounds, None),
                    cash_account: cash
                        .cash_accounts
                        .into_iter()
                        .map(|account| account.code)
                        .collect(),
                });
                let rows = cash
                    .operating
                    .lines
                    .into_iter()
                    .chain(cash.investing.lines)
                    .chain(cash.financing.lines)
                    .map(Row::from)
                    .collect();
                Chart::categories(
                    "Cash movement by counter-account",
                    rows,
                    currency,
                    target,
                    all,
                )
            }
            ReportCommand::IncomeStatement {
                period: None,
                compare: None,
                ..
            } => {
                let income: Income = parse(data)?;
                let target = Target::IncomeStatement(range(company, currency, income.bounds, None));
                let rows = income
                    .sections
                    .into_iter()
                    .filter(|section| section.key == "expenses")
                    .flat_map(|section| section.accounts)
                    .map(Row::from)
                    .collect();
                Chart::categories("Spending by category", rows, currency, target, all)
            }
            ReportCommand::IncomeStatement { .. } | ReportCommand::TrialBalance { .. } => periods(
                data,
                company,
                currency,
                all,
                matches!(args.command, ReportCommand::IncomeStatement { .. }),
            ),
            _ => Err(invalid_source("unsupported report")),
        },
        _ => Err(invalid_source("unsupported command")),
    }
}

fn trend(data: &Value, company: &str, currency: Currency, all: bool) -> Result<Chart, CliError> {
    let trend: Trend = parse(data)?;
    let spending = matches!(trend.metric, Metric::Spending);
    let target = Target::Trend(TrendState {
        range: range(company, currency, trend.bounds, None),
        period: trend.period,
        metric: trend.metric,
    });
    // The total is already represented by the categories; including both would
    // double-count spending in a chart. Net worth keeps its three named balances.
    let rows = trend
        .series
        .into_iter()
        .filter(|row| !spending || row.code.is_some())
        .map(|row| Row {
            name: row.name,
            values: row.amounts.into_iter().map(i128::from).collect(),
            account: row.code,
        })
        .collect();
    Chart::new(
        Kind::Line,
        if spending {
            "Spending by category over time"
        } else {
            "Net worth at recorded book values"
        },
        trend
            .columns
            .into_iter()
            .map(|column| column.label)
            .collect(),
        rows,
        Context {
            currency,
            target,
            all,
        },
    )
}

fn periods(
    data: &Value,
    company: &str,
    currency: Currency,
    all: bool,
    income: bool,
) -> Result<Chart, CliError> {
    let report: PeriodReport = parse(data)?;
    let compare = report.prior.is_some();
    let x = match (report.columns, report.current, &report.prior) {
        (Some(columns), None, None) => columns.into_iter().map(|column| column.label).collect(),
        (None, Some(current), Some(prior)) => vec![current.label(), prior.label()],
        _ => return Err(invalid_source("missing period/comparison bounds")),
    };
    let state = range(company, currency, report.bounds, report.prior);
    let target = if income {
        Target::IncomeStatement(state)
    } else {
        Target::Transactions(state)
    };
    let rows = report
        .accounts
        .into_iter()
        .map(|row| {
            let values = if compare {
                vec![
                    row.current
                        .ok_or_else(|| invalid_source("missing current amount"))?,
                    row.prior
                        .ok_or_else(|| invalid_source("missing prior amount"))?,
                ]
            } else {
                row.amounts
            };
            Ok(Row {
                name: row.name,
                values: values.into_iter().map(i128::from).collect(),
                account: Some(row.code),
            })
        })
        .collect::<Result<_, CliError>>()?;
    Chart::new(
        if compare { Kind::Bar } else { Kind::Line },
        "Account activity on its normal side",
        x,
        rows,
        Context {
            currency,
            target,
            all,
        },
    )
}
