//! RFC 4180 CSV output rendering.
//!
//! Each function writes a header row followed by data rows to an in-memory
//! buffer and returns the resulting string.  The `csv` crate handles quoting
//! and escaping.

use crate::db::{AccountRow, AccountWithBalanceRow, BalanceRow, CompanyRow, TransactionRow};
use crate::error::CliError;

/// Convert a `csv::Error` to `CliError::General`.
fn csv_err(e: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {e}"))
}

/// Derive the normal-balance direction from a lowercase account-type string.
fn normal_balance_for(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "debit",
        "liability" | "equity" | "revenue" => "credit",
        _ => "unknown",
    }
}

/// Render a list of companies as CSV.
///
/// Columns: `slug`, `name`, `created_at`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_companies(companies: &[CompanyRow]) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["slug", "name", "description", "created_at"])
        .map_err(|e| csv_err(&e))?;

    for c in companies {
        let desc = c.description.as_deref().unwrap_or("");
        wtr.write_record([
            c.slug.as_str(),
            c.name.as_str(),
            desc,
            c.created_at.as_str(),
        ])
        .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Render a list of accounts as CSV.
///
/// Columns: `code`, `name`, `type`, `normal_balance`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_accounts(accounts: &[AccountRow]) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["code", "name", "type", "normal_balance"])
        .map_err(|e| csv_err(&e))?;

    for a in accounts {
        wtr.write_record([
            &a.code,
            &a.name,
            &a.account_type,
            normal_balance_for(&a.account_type),
        ])
        .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Render a list of accounts with balance totals as CSV.
///
/// Columns: `code`, `name`, `type`, `normal_balance`, `debit_total`, `credit_total`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_accounts_with_balances(
    accounts: &[AccountWithBalanceRow],
) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["code", "name", "type", "normal_balance", "debit_total", "credit_total"])
        .map_err(|e| csv_err(&e))?;

    for a in accounts {
        wtr.write_record([
            &a.code,
            &a.name,
            &a.account_type,
            normal_balance_for(&a.account_type),
            &a.debit_total.to_string(),
            &a.credit_total.to_string(),
        ])
        .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Render a list of transactions as CSV.
///
/// Columns: `id`, `date`, `description`, `metadata`, `currency`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_transactions(transactions: &[TransactionRow]) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record([
        "id",
        "date",
        "description",
        "metadata",
        "currency",
        "reference",
    ])
    .map_err(|e| csv_err(&e))?;

    for txn in transactions {
        let id_str = txn.id.to_string();
        let meta = txn.metadata.as_deref().unwrap_or("");
        let reference = txn.reference.as_deref().unwrap_or("");
        wtr.write_record([
            id_str.as_str(),
            &txn.date,
            &txn.description,
            meta,
            &txn.currency,
            reference,
        ])
        .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Render a trial balance as CSV.
///
/// Columns: `code`, `name`, `type`, `debit_total`, `credit_total`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_trial_balance(balances: &[BalanceRow]) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["code", "name", "type", "debit_total", "credit_total"])
        .map_err(|e| csv_err(&e))?;

    for b in balances {
        let debit = b.debit_total.to_string();
        let credit = b.credit_total.to_string();
        wtr.write_record([
            b.code.as_str(),
            b.name.as_str(),
            b.account_type.as_str(),
            debit.as_str(),
            credit.as_str(),
        ])
        .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Render a single account balance as CSV.
///
/// Columns: `code`, `name`, `type`, `debit_total`, `credit_total`, `currency`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_account_balance(balance: &BalanceRow, currency: &str) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record([
        "code",
        "name",
        "type",
        "debit_total",
        "credit_total",
        "currency",
    ])
    .map_err(|e| csv_err(&e))?;

    let debit = balance.debit_total.to_string();
    let credit = balance.credit_total.to_string();
    wtr.write_record([
        balance.code.as_str(),
        balance.name.as_str(),
        balance.account_type.as_str(),
        debit.as_str(),
        credit.as_str(),
        currency,
    ])
    .map_err(|e| csv_err(&e))?;

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

// ---------------------------------------------------------------------------
// Orphaned correlations
// ---------------------------------------------------------------------------

/// Render orphaned intercompany correlations as CSV.
///
/// # Errors
///
/// Returns [`CliError`] if CSV writing fails.
pub fn render_orphaned_correlations(
    orphans: &[crate::db::OrphanedCorrelation],
) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(vec![]);
    wtr.write_record([
        "transaction_id",
        "company",
        "description",
        "date",
        "partner_id",
    ])
    .map_err(|e| CliError::General(format!("CSV write error: {e}")))?;

    for o in orphans {
        wtr.write_record([
            &o.transaction_id.to_string(),
            &o.company_slug,
            &o.description,
            &o.date,
            &o.partner_id.to_string(),
        ])
        .map_err(|e| CliError::General(format!("CSV write error: {e}")))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush error: {e}")))?;
    String::from_utf8(bytes).map_err(|e| CliError::General(format!("CSV encoding error: {e}")))
}

// ---------------------------------------------------------------------------
// Tax summary
// ---------------------------------------------------------------------------

/// Render a tax summary as CSV.
///
/// Columns: `tax_category`, `debit_total`, `credit_total`
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_tax_summary(rows: &[crate::db::TaxSummaryRow]) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(["tax_category", "debit_total", "credit_total"])
        .map_err(|e| csv_err(&e))?;

    for r in rows {
        let debit = r.debit_total.to_string();
        let credit = r.credit_total.to_string();
        wtr.write_record([r.tax_category.as_str(), debit.as_str(), credit.as_str()])
            .map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
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
        let csv_str = result.unwrap_or_default();
        // Should have header row only
        assert_eq!(csv_str.trim(), "slug,name,description,created_at");
    }

    #[test]
    fn render_companies_single() {
        let rows = vec![CompanyRow {
            slug: "acme".into(),
            name: "Acme Corp".into(),
            description: None,
            created_at: "2025-01-01".into(),
        }];
        let csv_str = render_companies(&rows).unwrap_or_default();
        assert!(csv_str.contains("slug,name,description,created_at"));
        assert!(csv_str.contains("acme,Acme Corp,,2025-01-01"));
    }

    #[test]
    fn render_companies_quoting() {
        let rows = vec![CompanyRow {
            slug: "acme".into(),
            name: "Acme, Corp".into(), // comma in name
            description: Some("A company".into()),
            created_at: "2025-01-01".into(),
        }];
        let csv_str = render_companies(&rows).unwrap_or_default();
        // The name should be quoted because it contains a comma
        assert!(csv_str.contains("\"Acme, Corp\""));
    }

    #[test]
    fn render_accounts_has_normal_balance() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let csv_str = render_accounts(&rows).unwrap_or_default();
        assert!(csv_str.contains("code,name,type,normal_balance"));
        assert!(csv_str.contains("1000,Cash,asset,debit"));
    }

    #[test]
    fn render_accounts_revenue_credit() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "4000".into(),
            name: "Revenue".into(),
            account_type: "revenue".into(),
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let csv_str = render_accounts(&rows).unwrap_or_default();
        assert!(csv_str.contains("4000,Revenue,revenue,credit"));
    }

    #[test]
    fn render_transactions_empty() {
        let csv_str = render_transactions(&[]).unwrap_or_default();
        assert_eq!(
            csv_str.trim(),
            "id,date,description,metadata,currency,reference"
        );
    }

    #[test]
    fn render_transactions_with_metadata() {
        let rows = vec![TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Sale".into(),
            metadata: Some("INV-001".into()),
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
            reference: None,
        }];
        let csv_str = render_transactions(&rows).unwrap_or_default();
        assert!(csv_str.contains("1,2025-03-15,Sale,INV-001,USD"));
    }

    #[test]
    fn render_transactions_without_metadata() {
        let rows = vec![TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Sale".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
            reference: None,
        }];
        let csv_str = render_transactions(&rows).unwrap_or_default();
        assert!(csv_str.contains("1,2025-03-15,Sale,,USD"));
    }

    #[test]
    fn render_trial_balance_csv() {
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
        let csv_str = render_trial_balance(&rows).unwrap_or_default();
        assert!(csv_str.contains("code,name,type,debit_total,credit_total"));
        assert!(csv_str.contains("1000,Cash,asset,10000,0"));
        assert!(csv_str.contains("4000,Revenue,revenue,0,10000"));
    }

    #[test]
    fn render_trial_balance_empty() {
        let csv_str = render_trial_balance(&[]).unwrap_or_default();
        assert_eq!(csv_str.trim(), "code,name,type,debit_total,credit_total");
    }

    #[test]
    fn render_account_balance_csv() {
        let row = BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 15000,
            credit_total: 5000,
        };
        let csv_str = render_account_balance(&row, "USD").unwrap_or_default();
        assert!(csv_str.contains("code,name,type,debit_total,credit_total,currency"));
        assert!(csv_str.contains("1000,Cash,asset,15000,5000,USD"));
    }
}
