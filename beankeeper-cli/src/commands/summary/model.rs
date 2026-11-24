//! Pure comparisons, stable view targets and brief factual wording.

use crate::db::holds::StaleHoldAge;
use beankeeper::types::{Currency, Money};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::{Number, Value, json};

use super::{Periods, Standing, SummaryReport, money, net, sub, sum};
use crate::db::BalanceRow;
use crate::error::CliError;
use crate::output::amounts::{MoneyField, format_money};
use crate::percent::{Percent, change_percent};

pub(crate) struct Comparison {
    id: String,
    label: String,
    current: MoneyField,
    prior: MoneyField,
    change: MoneyField,
    change_percent: Option<Percent>,
}

impl Comparison {
    fn new(
        id: &str,
        label: &str,
        current: i64,
        prior: i64,
        currency: Currency,
    ) -> Result<Self, CliError> {
        Ok(Self {
            id: id.into(),
            label: label.into(),
            current: MoneyField::new("current", money(current, currency)),
            prior: MoneyField::new("prior", money(prior, currency)),
            change: MoneyField::new("change", money(sub(current, prior)?, currency)),
            change_percent: change_percent(current, prior),
        })
    }

    pub(crate) fn chart_series(&self) -> (&str, [Money; 2]) {
        (&self.label, [self.prior.money(), self.current.money()])
    }
}

fn percent_number(percent: Option<Percent>) -> Option<Number> {
    percent.and_then(|percent| {
        percent
            .to_fixed_half_even(1)
            .parse()
            .ok()
            .and_then(Number::from_f64)
    })
}

impl Serialize for Comparison {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(9))?;
        fields.serialize_entry("id", &self.id)?;
        fields.serialize_entry("label", &self.label)?;
        self.current.write_to(&mut fields)?;
        self.prior.write_to(&mut fields)?;
        self.change.write_to(&mut fields)?;
        fields.serialize_entry("change_percent", &percent_number(self.change_percent))?;
        fields.end()
    }
}

pub(super) fn comparison(
    current: &[BalanceRow],
    prior: &[BalanceRow],
    currency: Currency,
) -> Result<Vec<Comparison>, CliError> {
    let total = |rows: &[BalanceRow], kind: beankeeper::types::AccountType| {
        sum(rows.iter().filter(|row| row.account_type == kind))
    };
    let income = total(current, beankeeper::types::AccountType::Revenue)?;
    let last_income = total(prior, beankeeper::types::AccountType::Revenue)?;
    let spending = total(current, beankeeper::types::AccountType::Expense)?;
    let last_spending = total(prior, beankeeper::types::AccountType::Expense)?;
    Ok(vec![
        Comparison::new("income", "Income", income, last_income, currency)?,
        Comparison::new("spending", "Spending", spending, last_spending, currency)?,
        Comparison::new(
            "left_over",
            "Left over",
            sub(income, spending)?,
            sub(last_income, last_spending)?,
            currency,
        )?,
    ])
}

pub(super) fn movers(
    current: &[BalanceRow],
    prior: &[BalanceRow],
    currency: Currency,
) -> Result<Vec<Comparison>, CliError> {
    let mut changes = current
        .iter()
        .filter(|row| row.account_type == beankeeper::types::AccountType::Expense)
        .map(|row| {
            let prior = prior
                .iter()
                .find(|old| old.code == row.code)
                .map(net)
                .transpose()?
                .unwrap_or(0);
            Comparison::new(&row.code, &row.name, net(row)?, prior, currency)
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    changes.retain(|row| !row.change.money().is_zero());
    changes.sort_by(|a, b| {
        b.change
            .minor_units()
            .unsigned_abs()
            .cmp(&a.change.minor_units().unsigned_abs())
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

const SUMMARY_VIEW_ID: &str = "financial-summary";

#[derive(Serialize)]
pub(super) struct SelectedAccounts<'a> {
    #[serde(rename = "cash_account", skip_serializing_if = "<[_]>::is_empty")]
    pub cash: &'a [String],
    #[serde(rename = "receivable_account", skip_serializing_if = "<[_]>::is_empty")]
    pub receivable: &'a [String],
    #[serde(rename = "payable_account", skip_serializing_if = "<[_]>::is_empty")]
    pub payable: &'a [String],
}

#[derive(Serialize)]
struct ViewState<'a> {
    company: &'a str,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    compare_from: chrono::NaiveDate,
    compare_to: chrono::NaiveDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_hold_age_days: Option<StaleHoldAge>,
    #[serde(flatten)]
    accounts: &'a SelectedAccounts<'a>,
}

#[derive(Serialize)]
struct DesktopLink<'a> {
    view_id: &'static str,
    state: ViewState<'a>,
}

pub(super) fn desktop_view(
    company: &str,
    currency: Currency,
    periods: &Periods,
    stale_hold_age_days: StaleHoldAge,
    accounts: &SelectedAccounts<'_>,
) -> Value {
    let state = ViewState {
        company,
        currency,
        from: periods.current.from,
        to: periods.current.to,
        compare_from: periods.prior.from,
        compare_to: periods.prior.to,
        stale_hold_age_days: (stale_hold_age_days != StaleHoldAge::default())
            .then_some(stale_hold_age_days),
        accounts,
    };
    let uri = legacy_view_uri(&state);
    json!({
        "view": SUMMARY_VIEW_ID,
        "company": state.company, "currency": state.currency.code(),
        "from": state.from, "to": state.to,
        "compare_from": state.compare_from, "compare_to": state.compare_to,
        "cash_accounts": accounts.cash,
        "receivable_accounts": accounts.receivable,
        "payable_accounts": accounts.payable,
        "uri": uri,
        "link": DesktopLink { view_id: SUMMARY_VIEW_ID, state },
    })
}

fn legacy_view_uri(state: &ViewState<'_>) -> String {
    use std::fmt::Write as _;

    let mut uri = format!(
        "beankeeper://summary?company={}&currency={}&from={}&to={}&compare_from={}&compare_to={}",
        encode(state.company),
        state.currency.code(),
        state.from,
        state.to,
        state.compare_from,
        state.compare_to,
    );
    if let Some(age) = state.stale_hold_age_days {
        let _ = write!(uri, "&stale_hold_age_days={age}");
    }
    for (name, codes) in [
        ("cash_account", state.accounts.cash),
        ("receivable_account", state.accounts.receivable),
        ("payable_account", state.accounts.payable),
    ] {
        for code in codes {
            // Writing to a String cannot fail.
            let _ = write!(uri, "&{name}={}", encode(code));
        }
    }
    uri
}

fn amount_with_currency(amount: Money) -> String {
    format!("{} {}", amount.currency().code(), format_money(amount))
}

fn change_fact(row: &Comparison) -> String {
    let value = amount_with_currency(row.current.money());
    let change = row.change.minor_units();
    if change == 0 {
        return format!(
            "{} was {value}, unchanged from the comparison period.",
            row.label
        );
    }
    let direction = if change > 0 { "up" } else { "down" };
    // The checked source change is an i64, so its magnitude fits in i128.
    let difference = amount_with_currency(Money::from_minor(
        change.abs(),
        row.change.money().currency(),
    ));
    match row.change_percent {
        Some(percent) => format!(
            "{} was {value}, {direction} {difference} ({}%) from the comparison period.",
            row.label,
            percent.to_fixed_half_even(1).trim_start_matches('-'),
        ),
        None => format!(
            "{} was {value}, {direction} {difference} from zero in the comparison period.",
            row.label,
        ),
    }
}

pub(super) fn highlights(
    standing: &Standing,
    comparison: &[Comparison],
    movers: &[Comparison],
) -> Vec<String> {
    let cash_fact = if standing.cash_accounts_identified {
        format!(
            "Cash on hand in the selected accounts is {}.",
            amount_with_currency(standing.cash_on_hand.money()),
        )
    } else {
        "No cash accounts were identified; select your cash accounts to get a cash total.".into()
    };
    let owed_fact = if standing.receivable_accounts_identified {
        format!(
            "You are owed {} in the selected accounts.",
            amount_with_currency(standing.owed_to_you.money()),
        )
    } else {
        "No accounts for money owed to you were identified.".into()
    };
    let mut facts = vec![
        format!(
            "{cash_fact} Net worth is {} at recorded book values.",
            amount_with_currency(standing.net_worth.money()),
        ),
        format!(
            "{owed_fact} You owe {} in total.",
            amount_with_currency(standing.you_owe.money()),
        ),
    ];
    facts.extend(comparison.iter().map(change_fact));
    facts.extend(movers.iter().take(2).map(change_fact));
    facts
}

pub(super) fn plain(report: &SummaryReport) -> String {
    let periods = &report.periods;
    let mut lines = vec![format!(
        "Your money, {} to {} (compared with {} to {})",
        periods.current.from, periods.current.to, periods.prior.from, periods.prior.to,
    )];
    lines.extend(report.highlights.iter().cloned());
    for (name, item) in &report.attention {
        if let (Some(count), Some(total)) = (item.count, item.total())
            && count > 0
        {
            lines.push(format!(
                "{}: {count}; total {}. {}",
                name.replace('_', " "),
                amount_with_currency(total),
                item.reason,
            ));
        }
    }
    lines.push("Payment due dates are unavailable; older open items do not prove that a payment is overdue.".into());
    lines.push(format!(
        "Optional Desktop view: {}",
        report.view["uri"].as_str().unwrap_or(""),
    ));
    lines.join("\n")
}

pub(super) fn csv(report: &SummaryReport) -> Result<String, CliError> {
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
    let currency = report.currency.code();
    for (id, amount) in [
        ("cash_on_hand", report.standing.cash_on_hand),
        ("owed_to_you", report.standing.owed_to_you),
        ("you_owe", report.standing.you_owe),
        ("net_worth", report.standing.net_worth),
    ] {
        writer
            .write_record([
                "standing",
                id,
                &id.replace('_', " "),
                amount.money().currency().code(),
                &amount.minor_units().to_string(),
                "",
                "",
                "",
                "",
                "",
                "",
            ])
            .map_err(|error| csv_error(&error))?;
    }
    for (section, series) in [
        ("comparison", &report.comparison),
        ("biggest_movers", &report.movers),
    ] {
        for row in series {
            writer
                .write_record([
                    section,
                    &row.id,
                    &row.label,
                    row.current.money().currency().code(),
                    "",
                    &row.current.minor_units().to_string(),
                    &row.prior.minor_units().to_string(),
                    &row.change.minor_units().to_string(),
                    &percent_number(row.change_percent)
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    "",
                    "",
                ])
                .map_err(|error| csv_error(&error))?;
        }
    }
    for (&id, item) in &report.attention {
        let total = item
            .total()
            .map(|value| value.amount().minor_units().to_string())
            .unwrap_or_default();
        let count = item
            .count
            .map(|value| value.to_string())
            .unwrap_or_default();
        writer
            .write_record([
                "needs_attention",
                id,
                &id.replace('_', " "),
                currency,
                &total,
                "",
                "",
                "",
                "",
                &count,
                if item.available { "true" } else { "false" },
            ])
            .map_err(|error| csv_error(&error))?;
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
    fn comparison_projects_exact_ties_and_currency_without_the_envelope()
    -> Result<(), Box<dyn std::error::Error>> {
        for (current, expected) in [(17, 6.2), (15, -6.2), (19, 18.8)] {
            let row = Comparison::new("x", "x", current, 16, Currency::BHD)?;
            let json = serde_json::to_value(&row)?;
            assert_eq!(json["current"], current);
            assert_eq!(json["current_display"], format!("0.{current:03}"));
            assert_eq!(json["prior_display"], "0.016");
            assert_eq!(json["change_percent"], expected);
        }
        Ok(())
    }

    #[test]
    fn percentages_handle_zero_negative_and_minimum_baselines() -> Result<(), CliError> {
        let percentage = |current, prior| -> Result<Option<Number>, CliError> {
            let row = Comparison::new("x", "x", current, prior, Currency::USD)?;
            Ok(percent_number(row.change_percent))
        };
        assert_eq!(percentage(1, 0)?, None);
        assert_eq!(percentage(0, 0)?, None);
        assert_eq!(percentage(10, -10)?.and_then(|p| p.as_f64()), Some(200.0));
        assert_eq!(
            percentage(i64::MIN, i64::MIN)?.and_then(|p| p.as_f64()),
            Some(0.0)
        );
        assert!(percentage(i64::MAX, -1).is_err());
        assert_eq!(encode("a/b &c"), "a%2Fb%20%26c");
        assert_eq!(super::super::add(1, 1)?, 2);
        Ok(())
    }
}
