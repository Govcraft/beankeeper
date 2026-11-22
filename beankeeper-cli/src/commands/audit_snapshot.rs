//! Decimal companions for the financial fields in stored audit snapshots.
//!
//! These are read-side projections of the historical audit formats. Keep
//! additional snapshot properties and user annotations intact.

use beankeeper::types::{Currency, Money};
use serde_json::Value;

use crate::db::audit::AuditEntity;
use crate::output::amounts::format_money;

pub(super) fn embedded_currency(snapshot: &Value) -> Option<Currency> {
    snapshot
        .get("currency")
        .and_then(Value::as_str)
        .and_then(|code| code.parse().ok())
}

fn snapshot_currency(snapshot: &Value, fallback: Option<Currency>) -> Option<Currency> {
    if snapshot.get("currency").is_some() {
        embedded_currency(snapshot)
    } else {
        fallback
    }
}

pub(super) fn project_snapshot(
    snapshot: &mut Value,
    entity: AuditEntity,
    currency: Option<Currency>,
) {
    let currency = snapshot_currency(snapshot, currency);
    match entity {
        AuditEntity::Transaction | AuditEntity::Schedule => {
            project_entries(snapshot, currency);
        }
        AuditEntity::Entry => {
            display(snapshot, "amount", currency);
            display(snapshot, "entry_amount", currency);
            display(snapshot, "cleared_amount", currency);
            display(snapshot, "remaining_amount", currency);
            if let Some(clearings) = snapshot.get_mut("clearings").and_then(Value::as_array_mut) {
                for clearing in clearings {
                    let currency = snapshot_currency(clearing, currency);
                    display(clearing, "amount", currency);
                }
            }
        }
        AuditEntity::Budget => display(snapshot, "amount", currency),
        AuditEntity::Proposal => {
            display(snapshot, "debit_total", currency);
            if let Some(payload) = snapshot.get_mut("payload") {
                let currency = snapshot_currency(payload, currency);
                project_entries(payload, currency);
            }
        }
        AuditEntity::Price => display(snapshot, "price", currency),
        AuditEntity::Lot => {
            display(snapshot, "cost_basis", currency);
            display(snapshot, "remaining_cost_basis", currency);
            if let Some(relief) = snapshot.get_mut("relief") {
                let currency = snapshot_currency(relief, currency);
                display(relief, "cost_basis", currency);
            }
        }
        AuditEntity::SpendLimit => {
            display(snapshot, "max_post", currency);
            display(snapshot, "daily_limit", currency);
        }
        _ => {}
    }
}

fn project_entries(snapshot: &mut Value, currency: Option<Currency>) {
    if let Some(entries) = snapshot.get_mut("entries").and_then(Value::as_array_mut) {
        for entry in entries {
            let currency = snapshot_currency(entry, currency);
            display(entry, "amount", currency);
        }
    }
}

fn display(snapshot: &mut Value, key: &str, currency: Option<Currency>) {
    let Some(fields) = snapshot.as_object_mut() else {
        return;
    };
    let Some(raw) = fields
        .get(key)
        .filter(|value| value.is_number() || value.is_null())
    else {
        return;
    };
    let decimal = raw
        .as_i64()
        .zip(currency)
        .map(|(amount, currency)| format_money(Money::from_minor(i128::from(amount), currency)))
        .map_or(Value::Null, Value::String);
    fields.entry(format!("{key}_display")).or_insert(decimal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_snapshot_currency_keeps_original_minor_units() {
        let mut snapshot = json!({"amount":1234, "metadata":{"amount":300}});
        project_snapshot(&mut snapshot, AuditEntity::Budget, None);
        assert_eq!(
            snapshot,
            json!({"amount":1234,"amount_display":null,"metadata":{"amount":300}})
        );
    }

    #[test]
    fn historic_entity_paths_keep_nested_currency_and_metadata() {
        let mut proposal = json!({
            "currency":"JPY", "debit_total":12,
            "payload":{
                "currency":"BHD", "entries":[{"amount":12001}],
                "metadata":{"amount":298}
            }
        });
        project_snapshot(&mut proposal, AuditEntity::Proposal, None);
        assert_eq!(proposal["debit_total_display"], "12");
        assert_eq!(
            proposal["payload"]["entries"][0]["amount_display"],
            "12.001"
        );
        assert_eq!(proposal["payload"]["metadata"], json!({"amount":298}));
    }

    #[test]
    fn nonfinancial_snapshot_fields_do_not_acquire_money_displays() {
        let mut account = json!({"amount":1234, "entries":[{"amount":7}]});
        let original = account.clone();
        project_snapshot(&mut account, AuditEntity::Account, Some(Currency::USD));
        assert_eq!(account, original);
    }
}
