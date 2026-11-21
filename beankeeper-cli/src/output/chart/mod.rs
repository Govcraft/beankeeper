//! Version 1, exact-money chart projections. This module cannot mutate a ledger.
//!
//! Chart projections consume typed report results and issue no ledger queries.
//! Only the validating constructor can create a chart; output types deliberately
//! have no Deserialize implementation or publicly mutable fields.

mod source;

pub(crate) use source::{
    ComparisonView, cash_flow, comparison, income_statement, periods, summary, trend,
};

use beankeeper::types::{Amount, Currency, Money, MoneyError};
use serde::Serialize;

use crate::cli::{Cli, Command, ReportCommand};
use crate::error::CliError;

const DEFAULT_LIMIT: usize = 8;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Line,
    Bar,
}

/// Intermediate numeric row. Amounts stay exact until the final projection.
struct Row {
    name: String,
    values: Vec<Money>,
    account: Option<String>,
}

impl Row {
    fn new(
        name: &str,
        account: Option<&str>,
        values: impl IntoIterator<Item = i64>,
        currency: Currency,
    ) -> Self {
        Self {
            name: name.into(),
            values: values
                .into_iter()
                .map(|value| Money::from_minor(i128::from(value), currency))
                .collect(),
            account: account.map(str::to_owned),
        }
    }
    fn comparison(name: &str, account: Option<&str>, values: [Money; 2]) -> Self {
        Self {
            name: name.into(),
            values: values.into(),
            account: account.map(str::to_owned),
        }
    }
}

fn format_money(value: Money) -> String {
    value
        .amount()
        .format_decimal(value.currency().minor_units())
}

#[derive(Serialize)]
struct Series {
    name: String,
    values: Vec<String>,
    unit: &'static str,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

struct Context {
    currency: Currency,
    target: source::Target,
    all: bool,
}

#[derive(Serialize)]
pub(crate) struct Chart {
    schema_version: u8,
    kind: Kind,
    title: &'static str,
    x: Vec<String>,
    /// Account identifiers aligned with category labels, never display names.
    x_accounts: Vec<Option<String>>,
    series: Vec<Series>,
    desktop_view: source::Target,
    truncated: bool,
    omitted_count: usize,
}

impl Chart {
    fn new(
        kind: Kind,
        title: &'static str,
        x: Vec<String>,
        rows: Vec<Row>,
        context: Context,
    ) -> Result<Self, CliError> {
        let Context {
            currency,
            target,
            all,
        } = context;
        if rows.iter().any(|row| row.values.len() != x.len()) {
            return Err(invalid_source("chart series must align with x labels"));
        }
        let (rows, omitted_count) = compact(rows, currency, all)?;
        let series = rows
            .into_iter()
            .map(|row| Series {
                name: row.name,
                values: row.values.into_iter().map(format_money).collect(),
                unit: "major",
                currency,
                account: row.account,
            })
            .collect();
        let x_accounts = vec![None; x.len()];
        Ok(Self {
            schema_version: 1,
            kind,
            title,
            x,
            x_accounts,
            series,
            desktop_view: target,
            truncated: omitted_count > 0,
            omitted_count,
        })
    }

    /// Transpose one-point rows into a category axis without losing account IDs.
    fn categories(
        title: &'static str,
        rows: Vec<Row>,
        currency: Currency,
        target: source::Target,
        all: bool,
    ) -> Result<Self, CliError> {
        let mut chart = Self::new(
            Kind::Bar,
            title,
            vec!["Amount".into()],
            rows,
            Context {
                currency,
                target,
                all,
            },
        )?;
        chart.x = chart.series.iter().map(|row| row.name.clone()).collect();
        chart.x_accounts = chart.series.iter().map(|row| row.account.clone()).collect();
        let values = chart
            .series
            .iter_mut()
            .flat_map(|row| std::mem::take(&mut row.values))
            .collect();
        chart.series = vec![Series {
            name: title.into(),
            values,
            unit: "major",
            currency,
            account: None,
        }];
        Ok(chart)
    }
}

fn invalid_source(message: &str) -> CliError {
    CliError::General(format!("cannot project chart: {message}"))
}

/// Rank by sum of absolute values, breaking ties by account then label.
/// Checked arithmetic prevents a partial or silently clipped Other bucket.
fn compact(
    mut rows: Vec<Row>,
    currency: Currency,
    all: bool,
) -> Result<(Vec<Row>, usize), CliError> {
    if all || rows.len() <= DEFAULT_LIMIT {
        return Ok((rows, 0));
    }
    let score = |row: &Row| -> u128 {
        // Source values are i64 and reports are bounded to 120 periods.
        row.values
            .iter()
            .map(|value| value.amount().minor_units().unsigned_abs())
            .sum()
    };
    rows.sort_by(|left, right| {
        score(right)
            .cmp(&score(left))
            .then_with(|| left.account.cmp(&right.account))
            .then_with(|| left.name.cmp(&right.name))
    });
    let rest = rows.split_off(DEFAULT_LIMIT);
    let count = rest.len();
    let mut other = Row {
        name: "Other".into(),
        values: vec![Money::new(Amount::ZERO, currency); rest[0].values.len()],
        account: None,
    };
    for row in rest {
        for (total, value) in other.values.iter_mut().zip(row.values) {
            *total = total.checked_add(value).map_err(|error| match error {
                MoneyError::Overflow => invalid_source("Other exceeds the supported amount range"),
                _ => invalid_source(&error.to_string()),
            })?;
        }
    }
    rows.push(other);
    Ok((rows, count))
}

pub(crate) fn validate_request(cli: &Cli) -> Result<(), CliError> {
    if !cli.output.chart {
        return Ok(());
    }
    if !cli.is_json() {
        return Err(CliError::Usage(
            "--chart requires --json or --format json".into(),
        ));
    }
    match &cli.command {
        Command::Summary(_) => Ok(()),
        Command::Report(args) => match &args.command {
            ReportCommand::Trend(_)
            | ReportCommand::CashFlow(_)
            | ReportCommand::IncomeStatement { .. }
            | ReportCommand::TrialBalance { period: Some(_), .. }
            | ReportCommand::TrialBalance { compare: Some(_), .. } => Ok(()),
            _ => Err(CliError::Usage("--chart supports summary, trend, cash-flow, income-statement and period/comparison trial-balance reports".into())),
        },
        _ => Err(CliError::Usage("--chart supports summary and financial reports".into())),
    }
}

pub(crate) fn requested(
    cli: &Cli,
    project: impl FnOnce() -> Result<Chart, CliError>,
) -> Result<Option<Chart>, CliError> {
    cli.output.chart.then(project).transpose()
}

/// Attach an already computed chart to the existing report data at serialization.
#[derive(Serialize)]
pub(crate) struct WithChart<T> {
    #[serde(flatten)]
    pub(crate) report: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chart: Option<Chart>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn minimum_integer_and_other_use_exact_wide_arithmetic() -> Result<(), CliError> {
        let rows = (0..10)
            .map(|index| {
                Row::new(
                    "Same",
                    Some(&format!("50{index:02}")),
                    [i64::MIN],
                    Currency::USD,
                )
            })
            .collect();
        let (rows, omitted) = compact(rows, Currency::USD, false)?;
        assert_eq!(omitted, 2);
        assert_eq!(rows[0].account.as_deref(), Some("5000"));
        assert_eq!(rows[8].values, [Money::usd(i128::from(i64::MIN) * 2)]);
        let minimum = Money::from_minor(i128::from(i64::MIN), Currency::KWD);
        assert_eq!(format_money(minimum), "-9223372036854775.808");
        Ok(())
    }
}
