//! Version 1, exact-money chart projections. This module cannot mutate a ledger.
//!
//! Wire adapters consume already computed reports, never issue fresh queries.
//! Only the validating constructor can create a chart; output types deliberately
//! have no Deserialize implementation or publicly mutable fields.

mod source;

use beankeeper::types::{Amount, Currency};
use serde::Serialize;
use serde_json::Value;

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
    values: Vec<i128>,
    account: Option<String>,
}

#[derive(Serialize)]
struct Series {
    name: String,
    values: Vec<String>,
    unit: &'static str,
    currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

struct Context {
    currency: Currency,
    target: source::Target,
    all: bool,
}

#[derive(Serialize)]
struct Chart {
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
        let (rows, omitted_count) = compact(rows, all)?;
        let series = rows
            .into_iter()
            .map(|row| Series {
                name: row.name,
                values: row
                    .values
                    .into_iter()
                    .map(|value| Amount::new(value).format_decimal(currency.minor_units()))
                    .collect(),
                unit: "major",
                currency: currency.code().into(),
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
            currency: currency.code().into(),
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
fn compact(mut rows: Vec<Row>, all: bool) -> Result<(Vec<Row>, usize), CliError> {
    if all || rows.len() <= DEFAULT_LIMIT {
        return Ok((rows, 0));
    }
    let score = |row: &Row| -> u128 {
        // Source values are i64 and reports are bounded to 120 periods.
        row.values.iter().map(|value| value.unsigned_abs()).sum()
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
        values: vec![0; rest.first().map_or(0, |row| row.values.len())],
        account: None,
    };
    for row in rest {
        for (total, value) in other.values.iter_mut().zip(row.values) {
            *total = total
                .checked_add(value)
                .ok_or_else(|| invalid_source("Other exceeds the supported amount range"))?;
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

pub(crate) fn attach(cli: &Cli, company: &str, envelope: &mut Value) -> Result<(), CliError> {
    let chart = source::project(cli, company, envelope)?;
    envelope["data"]["chart"] =
        serde_json::to_value(chart).map_err(|error| invalid_source(&error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    fn cli() -> Result<Cli, CliError> {
        Cli::try_parse_from([
            "bk",
            "--json",
            "--chart",
            "report",
            "trend",
            "--metric",
            "net-worth",
        ])
        .map_err(|error| CliError::Usage(error.to_string()))
    }

    #[test]
    fn chart_boundary_rejects_misalignment_missing_values_and_unknown_units() -> Result<(), CliError>
    {
        let valid = json!({"data": {
            "currency":"USD", "from":"2024-01-01", "to":"2024-01-31",
            "metric":"net-worth", "period":"month", "columns":[{"label":"2024-01"}],
            "series":[{"code":null, "name":"Net worth", "amounts":[1]}]
        }});
        for invalid in [
            json!({"series":[{"name":"Net worth", "amounts":[]}]}),
            json!({"series":[{"name":"Net worth", "amounts":["1.00"]}]}),
            json!({"currency":"BOGUS"}),
            json!({"metric":"invented"}),
            json!({"period":"day"}),
            json!({"from":"2024-02-30"}),
        ] {
            let mut envelope = valid.clone();
            for (key, value) in invalid
                .as_object()
                .ok_or_else(|| invalid_source("fixture"))?
            {
                envelope["data"][key] = value.clone();
            }
            assert!(attach(&cli()?, "co", &mut envelope).is_err());
            assert!(envelope["data"].get("chart").is_none());
        }
        Ok(())
    }

    #[test]
    fn minimum_integer_and_other_use_exact_wide_arithmetic() -> Result<(), CliError> {
        let rows = (0..10)
            .map(|index| Row {
                name: "Same".into(),
                account: Some(format!("50{index:02}")),
                values: vec![i128::from(i64::MIN)],
            })
            .collect();
        let (rows, omitted) = compact(rows, false)?;
        assert_eq!(omitted, 2);
        assert_eq!(rows[0].account.as_deref(), Some("5000"));
        assert_eq!(rows[8].values, [i128::from(i64::MIN) * 2]);
        let mut envelope = json!({"data": {
            "currency":"KWD", "from":"2024-01-01", "to":"2024-01-31",
            "metric":"net-worth", "period":"month", "columns":[{"label":"2024-01"}],
            "series":[{"name":"Net worth", "amounts":[i64::MIN]}]
        }});
        attach(&cli()?, "co", &mut envelope)?;
        assert_eq!(
            envelope["data"]["chart"]["series"][0]["values"],
            json!(["-9223372036854775.808"])
        );
        Ok(())
    }
}
