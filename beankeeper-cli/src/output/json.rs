//! JSON output rendering with `serde_json`.
//!
//! Each function converts database row types into serialisable wrappers and
//! produces pretty-printed JSON.  Amounts are always raw integers (minor
//! units) and enum values are always lowercase.

use std::collections::HashMap;

use serde::Serialize;

use crate::db::{AccountRow, BalanceRow, CompanyRow, EntryRow, TransactionRow};
use crate::error::CliError;

// ---------------------------------------------------------------------------
// Serialisable wrapper types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct CompanyJson {
    slug: String,
    name: String,
    created_at: String,
}

#[derive(Serialize)]
pub struct AccountJson {
    code: String,
    name: String,
    r#type: String,
    normal_balance: String,
}

#[derive(Serialize)]
pub struct TransactionJson {
    id: i64,
    description: String,
    metadata: Option<String>,
    currency: String,
    date: String,
    entries: Vec<EntryJson>,
}

#[derive(Serialize)]
pub struct EntryJson {
    account_code: String,
    direction: String,
    amount: i64,
}

#[derive(Serialize)]
pub struct TrialBalanceJson {
    accounts: Vec<TrialBalanceAccountJson>,
    total_debits: i64,
    total_credits: i64,
    balanced: bool,
}

#[derive(Serialize)]
pub struct TrialBalanceAccountJson {
    code: String,
    name: String,
    r#type: String,
    debit_total: i64,
    credit_total: i64,
}

#[derive(Serialize)]
pub struct BalanceJson {
    code: String,
    name: String,
    r#type: String,
    debit_total: i64,
    credit_total: i64,
    currency: String,
}

#[derive(Serialize)]
pub struct ErrorJson {
    error: ErrorDetailJson,
}

#[derive(Serialize)]
pub struct ErrorDetailJson {
    code: String,
    message: String,
    hint: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a lowercase account-type string to its normal balance direction.
fn normal_balance_for(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "debit",
        "liability" | "equity" | "revenue" => "credit",
        _ => "unknown",
    }
}

/// Map a `CliError` variant to a machine-readable error code string.
fn error_code_string(err: &CliError) -> String {
    match err {
        CliError::Usage(_) => "USAGE_ERROR",
        CliError::Validation(_) | CliError::Bean(_) => "VALIDATION_ERROR",
        CliError::Database(_) | CliError::Sqlite(_) => "DATABASE_ERROR",
        CliError::NotFound(_) => "NOT_FOUND",
        CliError::General(_) => "GENERAL_ERROR",
        CliError::Io(_) => "IO_ERROR",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Render functions
// ---------------------------------------------------------------------------

/// Render a list of companies as a JSON array.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_companies(companies: &[CompanyRow]) -> Result<String, CliError> {
    let rows: Vec<CompanyJson> = companies
        .iter()
        .map(|c| CompanyJson {
            slug: c.slug.clone(),
            name: c.name.clone(),
            created_at: c.created_at.clone(),
        })
        .collect();

    serde_json::to_string_pretty(&rows)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render a list of accounts as a JSON array.
///
/// Account types and normal-balance directions are always lowercase.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_accounts(accounts: &[AccountRow]) -> Result<String, CliError> {
    let rows: Vec<AccountJson> = accounts
        .iter()
        .map(|a| AccountJson {
            code: a.code.clone(),
            name: a.name.clone(),
            r#type: a.account_type.clone(),
            normal_balance: normal_balance_for(&a.account_type).to_string(),
        })
        .collect();

    serde_json::to_string_pretty(&rows)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render transactions with their entries as a JSON array.
///
/// `entries_map` maps transaction IDs to their entry rows.  Transactions
/// without entries in the map are rendered with an empty `entries` array.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_transactions<S: ::std::hash::BuildHasher>(
    transactions: &[TransactionRow],
    entries_map: &HashMap<i64, Vec<EntryRow>, S>,
) -> Result<String, CliError> {
    let rows: Vec<TransactionJson> = transactions
        .iter()
        .map(|txn| {
            let entries = entries_map
                .get(&txn.id)
                .map(|rows| {
                    rows.iter()
                        .map(|e| EntryJson {
                            account_code: e.account_code.clone(),
                            direction: e.direction.clone(),
                            amount: e.amount,
                        })
                        .collect()
                })
                .unwrap_or_default();

            TransactionJson {
                id: txn.id,
                description: txn.description.clone(),
                metadata: txn.metadata.clone(),
                currency: txn.currency.clone(),
                date: txn.date.clone(),
                entries,
            }
        })
        .collect();

    serde_json::to_string_pretty(&rows)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render a trial balance as a JSON object with accounts, totals, and a
/// `balanced` boolean.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_trial_balance(balances: &[BalanceRow]) -> Result<String, CliError> {
    let mut total_debits: i64 = 0;
    let mut total_credits: i64 = 0;

    let accounts: Vec<TrialBalanceAccountJson> = balances
        .iter()
        .map(|b| {
            total_debits = total_debits.saturating_add(b.debit_total);
            total_credits = total_credits.saturating_add(b.credit_total);
            TrialBalanceAccountJson {
                code: b.code.clone(),
                name: b.name.clone(),
                r#type: b.account_type.clone(),
                debit_total: b.debit_total,
                credit_total: b.credit_total,
            }
        })
        .collect();

    let result = TrialBalanceJson {
        accounts,
        total_debits,
        total_credits,
        balanced: total_debits == total_credits,
    };

    serde_json::to_string_pretty(&result)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render a single account balance as a JSON object.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_account_balance(balance: &BalanceRow, currency: &str) -> Result<String, CliError> {
    let json = BalanceJson {
        code: balance.code.clone(),
        name: balance.name.clone(),
        r#type: balance.account_type.clone(),
        debit_total: balance.debit_total,
        credit_total: balance.credit_total,
        currency: currency.to_string(),
    };

    serde_json::to_string_pretty(&json)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render a `CliError` as a JSON string suitable for writing to stderr.
///
/// This always succeeds -- if serde serialisation fails internally, a
/// hand-crafted JSON string is returned as a fallback.
#[must_use]
pub fn render_error(err: &CliError) -> String {
    let json = ErrorJson {
        error: ErrorDetailJson {
            code: error_code_string(err),
            message: err.to_string(),
            hint: None,
        },
    };

    serde_json::to_string_pretty(&json).unwrap_or_else(|_| {
        // Fallback: hand-craft minimal JSON.
        let msg = err.to_string().replace('\"', "\\\"");
        format!(r#"{{"error":{{"code":"INTERNAL","message":"{msg}"}}}}"#)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_companies_empty() {
        let result = render_companies(&[]);
        assert!(result.is_ok());
        let json = result.unwrap_or_default();
        assert_eq!(json.trim(), "[]");
    }

    #[test]
    fn render_companies_single() {
        let rows = vec![CompanyRow {
            slug: "acme".into(),
            name: "Acme Corp".into(),
            created_at: "2025-01-01T00:00:00".into(),
        }];
        let json = render_companies(&rows).unwrap_or_default();
        assert!(json.contains(r#""slug": "acme""#));
        assert!(json.contains(r#""name": "Acme Corp""#));
    }

    #[test]
    fn render_accounts_normal_balance() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            created_at: "2025-01-01".into(),
        }];
        let json = render_accounts(&rows).unwrap_or_default();
        assert!(json.contains(r#""type": "asset""#));
        assert!(json.contains(r#""normal_balance": "debit""#));
    }

    #[test]
    fn render_accounts_revenue_is_credit() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "4000".into(),
            name: "Revenue".into(),
            account_type: "revenue".into(),
            created_at: "2025-01-01".into(),
        }];
        let json = render_accounts(&rows).unwrap_or_default();
        assert!(json.contains(r#""normal_balance": "credit""#));
    }

    #[test]
    fn render_transactions_with_entries() {
        let txns = vec![TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Sale".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
        }];
        let mut map = HashMap::new();
        map.insert(
            1,
            vec![
                EntryRow {
                    id: 1,
                    transaction_id: 1,
                    account_code: "1000".into(),
                    company_slug: "acme".into(),
                    direction: "debit".into(),
                    amount: 5000,
                },
                EntryRow {
                    id: 2,
                    transaction_id: 1,
                    account_code: "4000".into(),
                    company_slug: "acme".into(),
                    direction: "credit".into(),
                    amount: 5000,
                },
            ],
        );
        let json = render_transactions(&txns, &map).unwrap_or_default();
        assert!(json.contains(r#""description": "Sale""#));
        assert!(json.contains(r#""account_code": "1000""#));
        assert!(json.contains(r#""direction": "debit""#));
        assert!(json.contains(r#""amount": 5000"#));
    }

    #[test]
    fn render_transactions_missing_entries() {
        let txns = vec![TransactionRow {
            id: 99,
            company_slug: "acme".into(),
            description: "Orphan".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
        }];
        let json = render_transactions(&txns, &HashMap::new()).unwrap_or_default();
        assert!(json.contains(r#""entries": []"#));
    }

    #[test]
    fn render_trial_balance_balanced() {
        let rows = vec![
            BalanceRow {
                code: "1000".into(),
                name: "Cash".into(),
                account_type: "asset".into(),
                debit_total: 10000,
                credit_total: 0,
            },
            BalanceRow {
                code: "4000".into(),
                name: "Revenue".into(),
                account_type: "revenue".into(),
                debit_total: 0,
                credit_total: 10000,
            },
        ];
        let json = render_trial_balance(&rows).unwrap_or_default();
        assert!(json.contains(r#""balanced": true"#));
        assert!(json.contains(r#""total_debits": 10000"#));
        assert!(json.contains(r#""total_credits": 10000"#));
    }

    #[test]
    fn render_trial_balance_unbalanced() {
        let rows = vec![BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 10000,
            credit_total: 5000,
        }];
        let json = render_trial_balance(&rows).unwrap_or_default();
        assert!(json.contains(r#""balanced": false"#));
    }

    #[test]
    fn render_account_balance_json() {
        let row = BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 15000,
            credit_total: 5000,
        };
        let json = render_account_balance(&row, "USD").unwrap_or_default();
        assert!(json.contains(r#""code": "1000""#));
        assert!(json.contains(r#""currency": "USD""#));
        assert!(json.contains(r#""debit_total": 15000"#));
    }

    #[test]
    fn render_error_usage() {
        let err = CliError::Usage("missing --company flag".into());
        let json = render_error(&err);
        assert!(json.contains(r#""code": "USAGE_ERROR""#));
        assert!(json.contains("missing --company flag"));
    }

    #[test]
    fn render_error_not_found() {
        let err = CliError::NotFound("account 9999 not found".into());
        let json = render_error(&err);
        assert!(json.contains(r#""code": "NOT_FOUND""#));
    }

    #[test]
    fn render_error_database() {
        let err = CliError::Database("corruption detected".into());
        let json = render_error(&err);
        assert!(json.contains(r#""code": "DATABASE_ERROR""#));
    }

    #[test]
    fn render_error_validation() {
        let err = CliError::Validation("unbalanced".into());
        let json = render_error(&err);
        assert!(json.contains(r#""code": "VALIDATION_ERROR""#));
    }

    #[test]
    fn render_error_io() {
        let err = CliError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gone",
        ));
        let json = render_error(&err);
        assert!(json.contains(r#""code": "IO_ERROR""#));
    }

    #[test]
    fn normal_balance_for_all_types() {
        assert_eq!(normal_balance_for("asset"), "debit");
        assert_eq!(normal_balance_for("expense"), "debit");
        assert_eq!(normal_balance_for("liability"), "credit");
        assert_eq!(normal_balance_for("equity"), "credit");
        assert_eq!(normal_balance_for("revenue"), "credit");
        assert_eq!(normal_balance_for("unknown_type"), "unknown");
    }
}
