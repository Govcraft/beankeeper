//! Additive monetary presentation shared by CLI envelopes and MCP results.

use beankeeper::types::{Amount, Currency};
use serde_json::Value;

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
            object.extend(displays);
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
        (Some(amount), Some(currency)) => {
            Value::String(Amount::new(i128::from(amount)).format_decimal(currency.minor_units()))
        }
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
