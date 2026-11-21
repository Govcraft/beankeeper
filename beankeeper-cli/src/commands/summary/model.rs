//! Pure comparisons, stable view targets and brief factual wording.

use beankeeper::types::{Amount, Currency};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;
use serde_json::{Value, json};

use super::{Periods, net, sub, sum};
use crate::db::BalanceRow;
use crate::error::CliError;

#[derive(Serialize)]
pub(crate) struct Comparison {
    id: String,
    label: String,
    current: i64,
    prior: i64,
    change: i64,
    change_percent: Option<f64>,
}

impl Comparison {
    fn new(id: &str, label: &str, current: i64, prior: i64) -> Result<Self, CliError> {
        let change = sub(current, prior)?;
        let change_percent = if prior == 0 {
            None
        } else {
            (Decimal::from(change) * Decimal::from(100) / Decimal::from(prior).abs())
                .round_dp(1)
                .to_f64()
        };
        Ok(Self {
            id: id.into(),
            label: label.into(),
            current,
            prior,
            change,
            change_percent,
        })
    }

    pub(crate) fn chart_series(&self) -> (&str, [i64; 2]) {
        (&self.label, [self.prior, self.current])
    }
}

pub(super) fn comparison(
    current: &[BalanceRow],
    prior: &[BalanceRow],
) -> Result<Vec<Comparison>, CliError> {
    let total =
        |rows: &[BalanceRow], kind: &str| sum(rows.iter().filter(|row| row.account_type == kind));
    let income = total(current, "revenue")?;
    let last_income = total(prior, "revenue")?;
    let spending = total(current, "expense")?;
    let last_spending = total(prior, "expense")?;
    Ok(vec![
        Comparison::new("income", "Income", income, last_income)?,
        Comparison::new("spending", "Spending", spending, last_spending)?,
        Comparison::new(
            "left_over",
            "Left over",
            sub(income, spending)?,
            sub(last_income, last_spending)?,
        )?,
    ])
}

pub(super) fn movers(
    current: &[BalanceRow],
    prior: &[BalanceRow],
) -> Result<Vec<Comparison>, CliError> {
    let mut changes = current
        .iter()
        .filter(|row| row.account_type == "expense")
        .map(|row| {
            let prior = prior
                .iter()
                .find(|old| old.code == row.code)
                .map(net)
                .transpose()?
                .unwrap_or(0);
            Comparison::new(&row.code, &row.name, net(row)?, prior)
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    changes.retain(|row| row.change != 0);
    changes.sort_by(|a, b| {
        b.change
            .unsigned_abs()
            .cmp(&a.change.unsigned_abs())
            .then_with(|| a.id.cmp(&b.id))
    });
    changes.truncate(5);
    Ok(changes)
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(super) fn desktop_view(company: &str, currency: Currency, periods: &Periods) -> Value {
    json!({
        "view":"financial-summary", "company":company, "currency":currency.code(),
        "from":periods.current.from, "to":periods.current.to,
        "compare_from":periods.prior.from, "compare_to":periods.prior.to,
        "uri":format!("beankeeper://summary?company={}&currency={}&from={}&to={}&compare_from={}&compare_to={}",
            encode(company),currency.code(),periods.current.from,periods.current.to,periods.prior.from,periods.prior.to),
    })
}

pub(super) fn select_view_accounts(view: &mut Value, selections: [(&str, &[String]); 3]) {
    let mut uri = view["uri"].as_str().unwrap_or_default().to_owned();
    for (kind, codes) in selections {
        view[format!("{kind}_accounts")] = json!(codes);
        for code in codes {
            uri.push('&');
            uri.push_str(kind);
            uri.push_str("_account=");
            uri.push_str(&encode(code));
        }
    }
    view["uri"] = Value::String(uri);
}

fn money(amount: i64, currency: Currency) -> String {
    format!(
        "{} {}",
        currency.code(),
        Amount::new(i128::from(amount)).format_decimal(currency.minor_units())
    )
}

fn change_fact(row: &Comparison, currency: Currency) -> String {
    let value = money(row.current, currency);
    if row.change == 0 {
        return format!(
            "{} was {value}, unchanged from the comparison period.",
            row.label
        );
    }
    let direction = if row.change > 0 { "up" } else { "down" };
    let difference =
        Amount::new(i128::from(row.change).abs()).format_decimal(currency.minor_units());
    match row.change_percent {
        Some(percent) => format!(
            "{} was {value}, {direction} {} {difference} ({:.1}%) from the comparison period.",
            row.label,
            currency.code(),
            percent.abs()
        ),
        None => format!(
            "{} was {value}, {direction} {} {difference} from zero in the comparison period.",
            row.label,
            currency.code()
        ),
    }
}

pub(super) fn highlights(
    standing: &Value,
    comparison: &[Comparison],
    movers: &[Comparison],
    currency: Currency,
) -> Vec<String> {
    let amount = |key| standing[key].as_i64().unwrap_or(0);
    let cash_known = standing["cash_accounts"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty());
    let receivables_known = standing["receivable_accounts"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty());
    let cash_fact = if cash_known {
        format!(
            "Cash on hand in the selected accounts is {}.",
            money(amount("cash_on_hand"), currency)
        )
    } else {
        "No cash accounts were identified; select your cash accounts to get a cash total.".into()
    };
    let owed_fact = if receivables_known {
        format!(
            "You are owed {} in the selected accounts.",
            money(amount("owed_to_you"), currency)
        )
    } else {
        "No accounts for money owed to you were identified.".into()
    };
    let mut facts = vec![
        format!(
            "{cash_fact} Net worth is {} at recorded book values.",
            money(amount("net_worth"), currency)
        ),
        format!(
            "{owed_fact} You owe {} in total.",
            money(amount("you_owe"), currency)
        ),
    ];
    facts.extend(comparison.iter().map(|row| change_fact(row, currency)));
    facts.extend(movers.iter().take(2).map(|row| change_fact(row, currency)));
    facts
}

pub(super) fn plain(data: &Value) -> String {
    let mut lines = vec![format!(
        "Your money, {} to {} (compared with {} to {})",
        text(&data["periods"]["current"]["from"]),
        text(&data["periods"]["current"]["to"]),
        text(&data["periods"]["prior"]["from"]),
        text(&data["periods"]["prior"]["to"])
    )];
    if let Some(facts) = data["highlights"].as_array() {
        lines.extend(facts.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if let Some(items) = data["needs_attention"]["items"].as_object() {
        for (name, item) in items {
            if let Some(count) = item["count"].as_u64()
                && count > 0
            {
                lines.push(format!(
                    "{}: {count}; total {} {}. {}",
                    name.replace('_', " "),
                    text(&data["currency"]),
                    text(&item["total_display"]),
                    text(&item["reason"])
                ));
            }
        }
    }
    lines.push("Payment due dates are unavailable; older open items do not prove that a payment is overdue.".into());
    lines.push(format!(
        "Optional Desktop view: {}",
        text(&data["desktop_view"]["uri"])
    ));
    lines.join("\n")
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

pub(super) fn csv(data: &Value) -> Result<String, CliError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    let header = [
        "section",
        "id",
        "label",
        "currency",
        "amount",
        "current",
        "prior",
        "change",
        "change_percent",
        "count",
        "available",
    ];
    writer
        .write_record(header)
        .map_err(|error| csv_error(&error))?;
    let currency = text(&data["currency"]);
    for id in ["cash_on_hand", "owed_to_you", "you_owe", "net_worth"] {
        writer
            .write_record([
                "standing",
                id,
                &id.replace('_', " "),
                currency,
                &data["standing"][id].to_string(),
                "",
                "",
                "",
                "",
                "",
                "",
            ])
            .map_err(|error| csv_error(&error))?;
    }
    for section in ["comparison", "biggest_movers"] {
        if let Some(series) = data[section]["series"].as_array() {
            for row in series {
                let number = |key| {
                    if row[key].is_null() {
                        String::new()
                    } else {
                        row[key].to_string()
                    }
                };
                writer
                    .write_record([
                        section,
                        text(&row["id"]),
                        text(&row["label"]),
                        currency,
                        "",
                        &number("current"),
                        &number("prior"),
                        &number("change"),
                        &number("change_percent"),
                        "",
                        "",
                    ])
                    .map_err(|error| csv_error(&error))?;
            }
        }
    }
    if let Some(items) = data["needs_attention"]["items"].as_object() {
        for (id, item) in items {
            let number = |key| {
                if item[key].is_null() {
                    String::new()
                } else {
                    item[key].to_string()
                }
            };
            writer
                .write_record([
                    "needs_attention",
                    id,
                    &id.replace('_', " "),
                    currency,
                    &number("total"),
                    "",
                    "",
                    "",
                    "",
                    &number("count"),
                    &number("available"),
                ])
                .map_err(|error| csv_error(&error))?;
        }
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes).map_err(|e| CliError::General(format!("invalid CSV UTF-8: {e}")))
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_handle_zero_negative_and_minimum_baselines() -> Result<(), CliError> {
        assert_eq!(Comparison::new("x", "x", 1, 0)?.change_percent, None);
        assert_eq!(Comparison::new("x", "x", 0, 0)?.change_percent, None);
        assert_eq!(
            Comparison::new("x", "x", 10, -10)?.change_percent,
            Some(200.0)
        );
        assert_eq!(
            Comparison::new("x", "x", i64::MIN, i64::MIN)?.change_percent,
            Some(0.0)
        );
        assert!(Comparison::new("x", "x", i64::MAX, -1).is_err());
        assert_eq!(encode("a/b &c"), "a%2Fb%20%26c");
        assert_eq!(super::super::add(1, 1)?, 2);
        Ok(())
    }
}
