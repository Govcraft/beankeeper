//! Additive monetary presentation shared by CLI envelopes and MCP results.

use beankeeper::types::{Currency, Money};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;

/// One currency-bearing amount and its legacy companion field.
#[derive(Clone, Copy)]
pub(crate) struct MoneyField {
    key: &'static str,
    money: Money,
}

impl MoneyField {
    pub(crate) const fn new(key: &'static str, money: Money) -> Self {
        Self { key, money }
    }

    pub(crate) const fn money(self) -> Money {
        self.money
    }

    pub(crate) const fn minor_units(self) -> i128 {
        self.money.amount().minor_units()
    }

    pub(crate) fn decimal(self) -> String {
        format_money(self.money)
    }
}

impl Serialize for MoneyField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut values = serializer.serialize_map(Some(2))?;
        self.write_to(&mut values)?;
        values.end()
    }
}

impl MoneyField {
    pub(crate) fn write_to<S: SerializeMap>(self, values: &mut S) -> Result<(), S::Error> {
        values.serialize_entry(self.key, &self.minor_units())?;
        values.serialize_entry(&DisplayKey(self.key), &self.decimal())
    }
}

/// An optional money field with both companion values present, even when null.
pub(crate) struct OptionalMoneyField(pub(crate) &'static str, pub(crate) Option<Money>);

impl Serialize for OptionalMoneyField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut values = serializer.serialize_map(Some(2))?;
        if let Some(money) = self.1 {
            MoneyField::new(self.0, money).write_to(&mut values)?;
        } else {
            values.serialize_entry(self.0, &None::<i128>)?;
            values.serialize_entry(&DisplayKey(self.0), &None::<String>)?;
        }
        values.end()
    }
}

/// One ordered monetary slice and its corresponding display array.
pub(crate) struct MoneySequence<'a> {
    key: &'static str,
    values: &'a [Money],
}

impl<'a> MoneySequence<'a> {
    pub(crate) const fn new(key: &'static str, values: &'a [Money]) -> Self {
        Self { key, values }
    }
}

impl Serialize for MoneySequence<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(2))?;
        fields.serialize_entry(
            self.key,
            &ProjectedMoney {
                values: self.values,
                project: |money: Money| money.amount().minor_units(),
            },
        )?;
        fields.serialize_entry(
            &DisplayKey(self.key),
            &ProjectedMoney {
                values: self.values,
                project: format_money,
            },
        )?;
        fields.end()
    }
}

struct ProjectedMoney<'a, F> {
    values: &'a [Money],
    project: F,
}

impl<F, T> Serialize for ProjectedMoney<'_, F>
where
    F: Fn(Money) -> T,
    T: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.values.len()))?;
        for money in self.values {
            sequence.serialize_element(&(self.project)(*money))?;
        }
        sequence.end()
    }
}

/// The decimal companion label always derives from the integer key.
struct DisplayKey(&'static str);

impl Serialize for DisplayKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{}_display", self.0))
    }
}

pub(crate) fn format_money(money: Money) -> String {
    money
        .amount()
        .format_decimal(money.currency().minor_units())
}

pub(crate) fn format_money_grouped(money: Money) -> String {
    let decimal = format_money(money);
    let whole_end = decimal.find('.').unwrap_or(decimal.len());
    let whole_start = usize::from(decimal.starts_with('-'));
    let whole = &decimal[whole_start..whole_end];
    if whole.len() <= 3 {
        return decimal;
    }

    let mut grouped = String::with_capacity(decimal.len() + (whole.len() - 1) / 3);
    grouped.push_str(&decimal[..whole_start]);
    for (index, digit) in whole.bytes().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.push_str(&decimal[whole_end..]);
    grouped
}

/// Add `<field>_display` decimal strings to monetary fields, retaining integers.
///
/// Currency is inherited from the containing report or transaction. An object's
/// own currency overrides it. Unknown scales produce `null`, never a guessed
/// decimal. User metadata, link targets and raw audit snapshots are untouched.
#[must_use]
pub fn with_amount_displays(mut value: Value, currency: Option<Currency>) -> Value {
    match &mut value {
        Value::Array(values) => {
            for value in values {
                *value = with_amount_displays(value.take(), currency);
            }
        }
        Value::Object(object) => {
            let currency = match object.get("currency") {
                Some(value) => value
                    .as_str()
                    .and_then(|code| Currency::from_code(code).ok()),
                None => currency,
            };
            let mut displays = Vec::new();
            for (key, value) in object.iter_mut() {
                if monetary_field(key) && (value.is_number() || value.is_null() || value.is_array())
                {
                    displays.push((format!("{key}_display"), display(value, currency)));
                } else if !matches!(key.as_str(), "metadata" | "link" | "before" | "after")
                    && !key.ends_with("_display")
                {
                    *value = with_amount_displays(value.take(), currency);
                }
            }
            // During migration, an explicitly typed companion takes precedence.
            for (key, value) in displays {
                object.entry(key).or_insert(value);
            }
        }
        _ => {}
    }
    value
}

fn display(value: &Value, currency: Option<Currency>) -> Value {
    if let Value::Array(values) = value {
        return Value::Array(values.iter().map(|v| display(v, currency)).collect());
    }
    match (value.as_i64(), currency) {
        (Some(amount), Some(currency)) => Value::String(format_money(Money::from_minor(
            i128::from(amount),
            currency,
        ))),
        _ => Value::Null,
    }
}

fn monetary_field(key: &str) -> bool {
    matches!(
        key,
        "cash_on_hand"
            | "owed_to_you"
            | "you_owe"
            | "net_worth"
            | "max_post"
            | "daily_limit"
            | "amount"
            | "principal"
            | "interest"
            | "payment"
            | "regular_payment"
            | "total_interest"
            | "total_paid"
            | "price"
            | "cost_basis"
            | "remaining_cost_basis"
            | "proceeds"
            | "realized_gain"
            | "latest_price"
            | "market_value"
            | "unrealized_gain"
            | "amounts"
            | "entry_amount"
            | "original_amount"
            | "open_amount"
            | "cleared_amount"
            | "remaining_amount"
            | "reconciled_amount"
            | "signed_amount"
            | "signed_remaining"
            | "settlement_amount"
            | "debit"
            | "credit"
            | "balance"
            | "debit_total"
            | "credit_total"
            | "total_debits"
            | "total_credits"
            | "threshold"
            | "total"
            | "budget"
            | "actual"
            | "variance"
            | "current"
            | "prior"
            | "change"
            | "total_revenue"
            | "total_expenses"
            | "net_income"
            | "total_assets"
            | "total_liabilities"
            | "total_equity"
            | "computed_earnings"
            | "total_liabilities_and_equity"
            | "difference"
            | "opening_balance"
            | "closing_balance"
            | "book_balance"
            | "cleared_balance"
            | "total_cleared_balance"
            | "reconciled_balance"
            | "uncleared_total"
            | "statement_balance"
            | "cleared_total"
            | "account_balance"
            | "total_open"
            | "unapplied_credit"
            | "net"
            | "net_change"
            | "opening_cash"
            | "closing_cash"
            | "expected"
            | "applied_this_statement"
            | "applied_total"
            | "reconciled_this_statement"
            | "remaining"
            | "short_pay"
            | "overpay"
            | "excess"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattened_money_uses_its_own_currency_for_the_companion() -> Result<(), serde_json::Error> {
        #[derive(Serialize)]
        struct Row {
            #[serde(flatten)]
            amount: MoneyField,
        }
        let value = serde_json::to_value(Row {
            amount: MoneyField::new("amount", Money::from_minor(1234, Currency::BHD)),
        })?;
        assert_eq!(value, json!({"amount":1234, "amount_display":"1.234"}));
        assert_eq!(
            with_amount_displays(value.clone(), Some(Currency::USD)),
            value
        );
        Ok(())
    }

    #[test]
    fn investment_money_uses_security_currency_and_preserves_exact_quantities() {
        let data = json!({"currency":"BHD", "quantity":"0.12345678", "cost_basis":1234,
            "remaining_cost_basis":1000, "price":1200, "latest_price":null,
            "market_value":null, "unrealized_gain":null, "proceeds":1001,
            "realized_gain":-233, "relief":[{"quantity":"0.1", "cost_basis":1}]});
        let rendered = with_amount_displays(data.clone(), None);
        assert_eq!(rendered["cost_basis_display"], "1.234");
        assert_eq!(rendered["remaining_cost_basis_display"], "1.000");
        assert_eq!(rendered["price_display"], "1.200");
        assert_eq!(rendered["proceeds_display"], "1.001");
        assert_eq!(rendered["realized_gain_display"], "-0.233");
        assert_eq!(rendered["relief"][0]["cost_basis_display"], "0.001");
        assert_eq!(rendered["quantity"], data["quantity"]);
        assert!(rendered["market_value_display"].is_null());
        assert!(rendered.get("quantity_display").is_none());
    }

    #[test]
    fn currencies_inherit_and_override_without_changing_source_values() {
        let data = json!([
            {"currency":"USD", "entries":[{"amount":180_000, "cleared_amount":0}]},
            {"currency":"JPY", "entries":[{"amount":1800}]},
            {"currency":"BHD", "amount":-1234, "metadata":{"amount":99}},
            {"currency":"KWD", "amount":i64::MIN}
        ]);
        let rendered = with_amount_displays(data.clone(), None);
        assert_eq!(rendered[0]["entries"][0]["amount_display"], "1800.00");
        assert_eq!(rendered[0]["entries"][0]["cleared_amount_display"], "0.00");
        assert_eq!(rendered[1]["entries"][0]["amount_display"], "1800");
        assert_eq!(rendered[2]["amount_display"], "-1.234");
        assert_eq!(rendered[3]["amount_display"], "-9223372036854775.808");
        assert_eq!(rendered[2]["metadata"], data[2]["metadata"]);
        assert_eq!(rendered[0]["entries"][0]["amount"], 180_000);
        assert_eq!(with_amount_displays(rendered.clone(), None), rendered);
    }

    #[test]
    fn report_arrays_nulls_and_nonmonetary_values() {
        let data = json!({"currency":"BHD", "accounts":[{"amounts":[1234,-1,0], "total":1233}],
            "statement_balance":null,"count":3,"id":22,"age_days":30,"change_percent":12.5,
            "current":{"from":"2026-01-01"}});
        let rendered = with_amount_displays(data, None);
        assert_eq!(
            rendered["accounts"][0]["amounts_display"],
            json!(["1.234", "-0.001", "0.000"])
        );
        assert_eq!(rendered["accounts"][0]["total_display"], "1.233");
        assert!(rendered["statement_balance_display"].is_null());
        for field in [
            "count_display",
            "id_display",
            "age_days_display",
            "change_percent_display",
            "current_display",
        ] {
            assert!(rendered.get(field).is_none());
        }
        let unknown = with_amount_displays(json!({"amount":123}), None);
        assert_eq!(unknown["amount"], 123);
        assert!(unknown.get("amount_display").is_some_and(Value::is_null));
    }
}
