use beankeeper::types::Currency;
use chrono::NaiveDate;
use serde::Serialize;

use super::{Chart, Context, Kind, Row};
use crate::commands::report_cash_flow::CashFlowStatement;
use crate::commands::report_compare::ComparisonReport;
use crate::commands::report_dates::DateRange;
use crate::commands::report_periods::{MultiColumnReport, MultiReportKind, PeriodArg};
use crate::commands::report_trend::{TrendMetric, TrendReport};
use crate::commands::summary::{Comparison, Periods};
use crate::error::CliError;
use crate::output::statements::IncomeStatement;

#[derive(Serialize)]
struct Bounds {
    from: NaiveDate,
    to: NaiveDate,
}

impl Bounds {
    fn label(&self) -> String {
        format!("{}..{}", self.from, self.to)
    }
}

impl From<DateRange> for Bounds {
    fn from(dates: DateRange) -> Self {
        Self {
            from: dates.from(),
            to: dates.to(),
        }
    }
}

impl From<(NaiveDate, NaiveDate)> for Bounds {
    fn from((from, to): (NaiveDate, NaiveDate)) -> Self {
        Self { from, to }
    }
}

#[derive(Serialize)]
struct ComparisonBounds {
    compare_from: NaiveDate,
    compare_to: NaiveDate,
}

#[derive(Serialize)]
pub(super) struct RangeState {
    company: String,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    #[serde(flatten)]
    bounds: Bounds,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    comparison: Option<ComparisonBounds>,
}

#[derive(Serialize)]
pub(super) struct TrendState {
    #[serde(flatten)]
    range: RangeState,
    period: PeriodArg,
    metric: TrendMetric,
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

fn range(company: &str, currency: Currency, bounds: Bounds, prior: Option<Bounds>) -> RangeState {
    RangeState {
        company: company.into(),
        currency,
        bounds,
        comparison: prior.map(|prior| ComparisonBounds {
            compare_from: prior.from,
            compare_to: prior.to,
        }),
    }
}

pub(crate) fn summary(
    company: &str,
    currency: Currency,
    periods: &Periods,
    comparison: &[Comparison],
    all: bool,
) -> Result<Chart, CliError> {
    let [current, prior] = periods.windows().map(Bounds::from);
    let x = vec![prior.label(), current.label()];
    let target = Target::IncomeStatement(range(company, currency, current, Some(prior)));
    let rows = comparison
        .iter()
        .map(|row| {
            let (label, values) = row.chart_series();
            Row::from_money(label, None, values)
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

pub(crate) fn cash_flow(
    company: &str,
    cash: &CashFlowStatement,
    all: bool,
) -> Result<Chart, CliError> {
    let currency = cash.currency();
    let target = Target::CashFlow(CashState {
        range: range(company, currency, cash.dates().into(), None),
        cash_account: cash
            .cash_accounts()
            .iter()
            .map(|account| account.code.clone())
            .collect(),
    });
    let rows = cash
        .operating
        .lines
        .iter()
        .chain(&cash.investing.lines)
        .chain(&cash.financing.lines)
        .map(|line| Row::from_money(line.name(), Some(line.code()), [line.amount()]))
        .collect();
    Chart::categories(
        "Cash movement by counter-account",
        rows,
        currency,
        target,
        all,
    )
}

pub(crate) fn income_statement(
    company: &str,
    income: &IncomeStatement,
    all: bool,
) -> Result<Chart, CliError> {
    let currency = income.currency();
    let target = Target::IncomeStatement(range(company, currency, income.dates().into(), None));
    let rows = income
        .expenses()
        .accounts()
        .iter()
        .map(|line| Row::from_money(line.name(), line.code(), [line.amount()]))
        .collect();
    Chart::categories("Spending by category", rows, currency, target, all)
}

pub(crate) fn trend(company: &str, trend: &TrendReport, all: bool) -> Result<Chart, CliError> {
    let currency = trend.currency();
    let spending = matches!(trend.metric(), TrendMetric::Spending);
    let target = Target::Trend(TrendState {
        range: range(company, currency, trend.dates().into(), None),
        period: trend.period(),
        metric: trend.metric(),
    });
    // The spending total overlaps the categories. Net worth keeps all three balances.
    let rows = trend
        .series()
        .iter()
        .filter(|series| !spending || series.code().is_some())
        .map(|series| {
            Row::from_money(
                series.name(),
                series.code(),
                series.amounts().iter().copied(),
            )
        })
        .collect();
    Chart::new(
        Kind::Line,
        if spending {
            "Spending by category over time"
        } else {
            "Net worth at recorded book values"
        },
        trend.labels().map(str::to_owned).collect(),
        rows,
        Context {
            currency,
            target,
            all,
        },
    )
}

pub(crate) fn periods(
    company: &str,
    report: &MultiColumnReport,
    all: bool,
) -> Result<Chart, CliError> {
    let currency = report.currency;
    let x = report
        .columns
        .iter()
        .map(|column| column.label.clone())
        .collect();
    let state = range(
        company,
        currency,
        Bounds {
            from: report.from,
            to: report.to,
        },
        None,
    );
    let target = match report.kind {
        MultiReportKind::IncomeStatement => Target::IncomeStatement(state),
        MultiReportKind::TrialBalance => Target::Transactions(state),
    };
    let rows = report
        .accounts
        .iter()
        .map(|row| {
            Row::new(
                &row.name,
                Some(&row.code),
                row.amounts.iter().copied(),
                currency,
            )
        })
        .collect();
    Chart::new(
        Kind::Line,
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

/// Supported detail targets for the two-window comparison computation.
#[derive(Clone, Copy)]
pub(crate) enum ComparisonView<'a> {
    IncomeStatement,
    TrialBalance,
    Trend(PeriodArg, TrendMetric),
    CashFlow(&'a [String]),
}

pub(crate) fn comparison(
    company: &str,
    report: &ComparisonReport,
    view: ComparisonView<'_>,
    all: bool,
) -> Result<Chart, CliError> {
    let currency = report.currency();
    let [current, prior] = report.windows().map(Bounds::from);
    let x = vec![current.label(), prior.label()];
    let range = range(company, currency, current, Some(prior));
    let (target, title) = match view {
        ComparisonView::IncomeStatement => (
            Target::IncomeStatement(range),
            "Account activity on its normal side",
        ),
        ComparisonView::TrialBalance => (
            Target::Transactions(range),
            "Account activity on its normal side",
        ),
        ComparisonView::Trend(period, metric) => (
            Target::Trend(TrendState {
                range,
                period,
                metric,
            }),
            match metric {
                TrendMetric::Spending => "Spending by category over time",
                TrendMetric::NetWorth => "Net worth at recorded book values",
            },
        ),
        ComparisonView::CashFlow(cash_accounts) => (
            Target::CashFlow(CashState {
                range,
                cash_account: cash_accounts.to_vec(),
            }),
            "Cash movement by counter-account",
        ),
    };
    let rows = if matches!(view, ComparisonView::Trend(_, TrendMetric::NetWorth)) {
        report
            .net_worth_amounts()
            .map(|(label, values)| Row::comparison(label, None, values))
            .collect()
    } else {
        report
            .account_amounts()
            .map(|(name, code, values)| Row::comparison(name, Some(code), values))
            .collect()
    };
    Chart::new(
        Kind::Bar,
        title,
        x,
        rows,
        Context {
            currency,
            target,
            all,
        },
    )
}
