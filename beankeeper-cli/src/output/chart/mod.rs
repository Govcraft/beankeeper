//! Version 1, exact-money chart projections. This module cannot mutate a ledger.
//!
//! Chart projections consume typed report results and issue no ledger queries.
//! Only the validating constructor can create a chart; output types deliberately
//! have no Deserialize implementation or publicly mutable fields.

mod source;

pub(crate) use source::{
    ComparisonView, cash_flow, comparison, income_statement, periods, summary, trend,
};

use beankeeper::types::{AccountType, Amount, Currency, Money, MoneyError};
use serde::Serialize;

use crate::error::CliError;
use crate::output::amounts::format_money;

const DEFAULT_LIMIT: usize = 8;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Line,
    Bar,
}

/// Financial meaning, independent of labels, signs, and consumer palettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Asset,
    Liability,
    NetWorth,
    Equity,
    Income,
    Expense,
    Leftover,
    CashFlow,
    Mixed,
}

impl From<AccountType> for Role {
    fn from(kind: AccountType) -> Self {
        match kind {
            AccountType::Asset => Self::Asset,
            AccountType::Liability => Self::Liability,
            AccountType::Equity => Self::Equity,
            AccountType::Revenue => Self::Income,
            AccountType::Expense => Self::Expense,
        }
    }
}

impl Role {
    /// A combined series keeps its meaning only if all contributors agree.
    fn combined(roles: impl IntoIterator<Item = Self>) -> Self {
        let mut roles = roles.into_iter();
        let Some(first) = roles.next() else {
            return Self::Mixed;
        };
        if roles.all(|role| role == first) {
            first
        } else {
            Self::Mixed
        }
    }
}

/// Intermediate numeric row. Amounts stay exact until the final projection.
struct Row {
    role: Role,
    name: String,
    values: Vec<Money>,
    account: Option<String>,
}

impl Row {
    fn new(
        role: Role,
        name: &str,
        account: Option<&str>,
        values: impl IntoIterator<Item = i64>,
        currency: Currency,
    ) -> Self {
        Self::from_money(
            role,
            name,
            account,
            values
                .into_iter()
                .map(|value| Money::from_minor(i128::from(value), currency)),
        )
    }

    fn from_money(
        role: Role,
        name: &str,
        account: Option<&str>,
        values: impl IntoIterator<Item = Money>,
    ) -> Self {
        Self {
            role,
            name: name.into(),
            values: values.into_iter().collect(),
            account: account.map(str::to_owned),
        }
    }

    fn comparison(role: Role, name: &str, account: Option<&str>, values: [Money; 2]) -> Self {
        Self::from_money(role, name, account, values)
    }
}

#[derive(Serialize)]
struct Series {
    role: Role,
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
                role: row.role,
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
        role: Role,
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
            role,
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
        role: Role::combined(rest.iter().map(|row| row.role)),
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

/// Attach an already computed chart to the existing report data at serialization.
#[derive(Serialize)]
pub(crate) struct WithChart<T, C = Chart> {
    #[serde(flatten)]
    pub(crate) report: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chart: Option<C>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_serialize_as_a_closed_set_and_convert_account_types() -> Result<(), serde_json::Error>
    {
        for (role, text) in [
            (Role::Asset, "asset"),
            (Role::Liability, "liability"),
            (Role::NetWorth, "net_worth"),
            (Role::Equity, "equity"),
            (Role::Income, "income"),
            (Role::Expense, "expense"),
            (Role::Leftover, "leftover"),
            (Role::CashFlow, "cash_flow"),
            (Role::Mixed, "mixed"),
        ] {
            assert_eq!(serde_json::to_value(role)?, text);
        }
        for (kind, role) in [
            (AccountType::Asset, Role::Asset),
            (AccountType::Liability, Role::Liability),
            (AccountType::Equity, Role::Equity),
            (AccountType::Revenue, Role::Income),
            (AccountType::Expense, Role::Expense),
        ] {
            assert_eq!(Role::from(kind), role);
        }
        Ok(())
    }

    #[test]
    fn other_preserves_shared_roles_and_marks_mixed_accounts() -> Result<(), CliError> {
        for (last, expected) in [
            (Role::Expense, Role::Expense),
            (Role::Liability, Role::Mixed),
        ] {
            let rows = (0..10)
                .map(|index| {
                    Row::new(
                        if index == 9 { last } else { Role::Expense },
                        "Cash",
                        Some(&format!("{index:04}")),
                        [10 - index],
                        Currency::USD,
                    )
                })
                .collect();
            let (rows, omitted) = compact(rows, Currency::USD, false)?;
            assert_eq!(omitted, 2);
            assert_eq!(rows[8].role, expected);
            assert_eq!(rows[8].values, [Money::usd(3)]);
        }
        Ok(())
    }

    #[test]
    fn minimum_integer_and_other_use_exact_wide_arithmetic() -> Result<(), CliError> {
        let rows = (0..10)
            .map(|index| {
                Row::new(
                    Role::Expense,
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
