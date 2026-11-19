//! Database layer: connection management, schema, and CRUD operations.

pub mod accounts;
pub mod attachments;
pub mod audit;
pub mod budgets;
pub mod clearings;
pub mod companies;
pub mod connection;
pub mod schema;
pub mod transactions;

pub use accounts::{
    ListAccountParams, account_exists, create_account, get_account, list_account_codes,
    list_accounts, row_to_account,
};
pub use attachments::{
    AttachmentRow, StoreAttachmentParams, get_attachment, hash_and_store_file, list_attachments,
    store_attachment,
};
pub use audit::{
    Actor, AuditLogRow, ClearAction, ClearOutcome, ClearRequest, EntryStatus, ListAuditParams,
    atomically, clear_entry, correlate_partner, delete_account, delete_budget, delete_company,
    list_audit, set_annual_budget, set_budget, set_entry_status, unclear_entry,
};
pub use budgets::{
    BudgetVarianceParams, ListBudgetParams, SetAnnualBudgetParams, SetBudgetParams,
    compute_budget_variance, list_budgets,
};
pub use clearings::{ClearingStatus, EntryTarget, find_entry_by_reference, get_entry_target};
pub use companies::{company_exists, create_company, get_company, list_companies};
pub use connection::Db;
pub use schema::{ensure_schema, get_schema_version};
pub use transactions::{
    ConflictStrategy, ListTransactionParams, OrphanedCorrelation, PostEntryParams, PostResult,
    PostTransactionParams, count_transactions, find_orphaned_correlations,
    get_entries_for_transaction, get_transaction, list_transactions, post_transaction,
};

use std::fmt::Write;

use rusqlite::{Connection, params};

use crate::error::CliError;

// ---------------------------------------------------------------------------
// Row types returned by queries
// ---------------------------------------------------------------------------

/// A row from the `companies` table.
#[derive(Debug, Clone)]
pub struct CompanyRow {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: String,
}

/// A row from the `accounts` table.
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub company_slug: String,
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub created_at: String,
    pub default_tax_category: Option<String>,
}

/// A row from the `transactions` table.
#[derive(Debug, Clone)]
pub struct TransactionRow {
    pub id: i64,
    pub company_slug: String,
    pub description: String,
    pub metadata: Option<String>,
    pub currency: String,
    pub date: String,
    pub posted_at: String,
    pub reference: Option<String>,
}

/// A row from the `entries` table.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: i64,
    pub transaction_id: i64,
    pub account_code: String,
    pub company_slug: String,
    pub direction: String,
    pub amount: i64,
    pub memo: Option<String>,
    pub tax_category: Option<String>,
    pub status: String,
}

/// A row from the `budgets` table.
#[derive(Debug, Clone)]
pub struct BudgetRow {
    pub id: i64,
    pub company_slug: String,
    pub account_code: String,
    pub currency: String,
    pub year: i32,
    pub month: i32,
    pub amount: i64,
    pub notes: Option<String>,
    pub created_at: String,
}

/// A computed row for the budget-variance report.
#[derive(Debug, Clone)]
pub struct BudgetVarianceRow {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub budget_amount: i64,
    pub actual_amount: i64,
    pub variance_amount: i64,
    pub variance_percent: Option<f64>,
    pub favorable: bool,
}

/// Aggregated totals per tax category, used in the tax summary report.
#[derive(Debug, Clone)]
pub struct TaxSummaryRow {
    pub tax_category: String,
    pub debit_total: i64,
    pub credit_total: i64,
}

/// Aggregated balance data for an account, used in trial balance and balance reports.
#[derive(Debug, Clone)]
pub struct BalanceRow {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub debit_total: i64,
    pub credit_total: i64,
}

/// Account with aggregated debit/credit balance totals.
#[derive(Debug, Clone)]
pub struct AccountWithBalanceRow {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub default_tax_category: Option<String>,
    pub debit_total: i64,
    pub credit_total: i64,
}

/// Lists accounts with their debit/credit balance totals in a single query.
///
/// Optionally filters by account type, name substring, and date range.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn list_accounts_with_balances(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
    name_filter: Option<&str>,
    from_date: Option<&str>,
    to_date: Option<&str>,
) -> Result<Vec<AccountWithBalanceRow>, CliError> {
    let has_date_filter = from_date.is_some() || to_date.is_some();
    let base = if has_date_filter {
        "SELECT a.code, a.name, a.type, a.default_tax_category, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN (entries e JOIN transactions t ON t.id = e.transaction_id) \
           ON e.company_slug = a.company_slug AND e.account_code = a.code"
    } else {
        "SELECT a.code, a.name, a.type, a.default_tax_category, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN entries e ON e.company_slug = a.company_slug AND e.account_code = a.code"
    };

    let mut sql = String::from(base);
    sql.push_str(" WHERE a.company_slug = ?1");

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(company_slug.to_string()));

    if let Some(filter) = type_filter {
        let _ = write!(sql, " AND a.type = ?{param_idx}");
        param_values.push(Box::new(filter.to_lowercase()));
        param_idx += 1;
    }

    if let Some(name) = name_filter {
        let _ = write!(sql, " AND a.name LIKE '%' || ?{param_idx} || '%'");
        param_values.push(Box::new(name.to_string()));
        param_idx += 1;
    }

    let _ = param_idx;

    sql.push_str(" GROUP BY a.code, a.name, a.type, a.default_tax_category ORDER BY a.code");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(AccountWithBalanceRow {
            code: row.get(0)?,
            name: row.get(1)?,
            account_type: row.get(2)?,
            default_tax_category: row.get(3)?,
            debit_total: row.get(4)?,
            credit_total: row.get(5)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Computes a trial balance for a company.
///
/// Returns one [`BalanceRow`] per account (including those with zero totals).
/// Optionally filters by account type, transaction currency and/or a date
/// range (inclusive).
///
/// When `currency` is given, only entries of transactions in that currency
/// are summed. The filter is applied inside the joined entries/transactions
/// group, so accounts with no matching entries are still listed with zero
/// totals.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_trial_balance(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Option<&str>,
) -> Result<Vec<BalanceRow>, CliError> {
    // Build the query dynamically based on whether we need date or currency
    // filtering. Either requires joining through transactions. We use a
    // parenthesised join so the LEFT JOIN still includes accounts with no
    // matching entries (zero balances); the currency filter lives in the
    // inner join's ON clause (not the WHERE clause) for the same reason.
    let has_date_filter = from_date.is_some() || to_date.is_some();
    let joins_transactions = has_date_filter || currency.is_some();
    let base = if joins_transactions {
        "SELECT a.code, a.name, a.type, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN (entries e JOIN transactions t ON t.id = e.transaction_id"
    } else {
        "SELECT a.code, a.name, a.type, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN entries e ON e.company_slug = a.company_slug AND e.account_code = a.code"
    };

    let mut sql = String::from(base);

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(company_slug.to_string()));

    if joins_transactions {
        // Date and currency filters live inside the joined group so accounts
        // whose entries all fall outside the window still appear with zero.
        if let Some(cur) = currency {
            let _ = write!(sql, " AND t.currency = ?{param_idx}");
            param_values.push(Box::new(cur.to_string()));
            param_idx += 1;
        }
        if let Some(from) = from_date {
            let _ = write!(sql, " AND t.date >= ?{param_idx}");
            param_values.push(Box::new(from.to_string()));
            param_idx += 1;
        }
        if let Some(to) = to_date {
            let _ = write!(sql, " AND t.date <= ?{param_idx}");
            param_values.push(Box::new(to.to_string()));
            param_idx += 1;
        }
        sql.push_str(") ON e.company_slug = a.company_slug AND e.account_code = a.code");
    }

    sql.push_str(" WHERE a.company_slug = ?1");

    if let Some(filter) = type_filter {
        let _ = write!(sql, " AND a.type = ?{param_idx}");
        param_values.push(Box::new(filter.to_lowercase()));
        param_idx += 1;
    }

    let _ = param_idx;

    sql.push_str(" GROUP BY a.code, a.name, a.type ORDER BY a.code");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(BalanceRow {
            code: row.get(0)?,
            name: row.get(1)?,
            account_type: row.get(2)?,
            debit_total: row.get(3)?,
            credit_total: row.get(4)?,
        })
    })?;

    let mut balances = Vec::new();
    for row in rows {
        balances.push(row?);
    }
    Ok(balances)
}

/// Computes the debit and credit totals for a single account.
///
/// Returns `(debit_total, credit_total)` in minor units. If a date range
/// is provided, only entries from transactions within that range (inclusive)
/// are included. If `currency` is provided, only entries from transactions
/// in that currency are included.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_account_balance(
    conn: &Connection,
    company_slug: &str,
    code: &str,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Option<&str>,
) -> Result<(i64, i64), CliError> {
    let has_date_filter = from_date.is_some() || to_date.is_some();

    if has_date_filter || currency.is_some() {
        let mut sql = String::from(
            "SELECT \
                COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
             FROM entries e \
             JOIN transactions t ON t.id = e.transaction_id \
             WHERE e.company_slug = ?1 AND e.account_code = ?2",
        );

        let mut param_idx = 3u32;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(company_slug.to_string()));
        param_values.push(Box::new(code.to_string()));

        if let Some(cur) = currency {
            let _ = write!(sql, " AND t.currency = ?{param_idx}");
            param_values.push(Box::new(cur.to_string()));
            param_idx += 1;
        }

        if let Some(from) = from_date {
            let _ = write!(sql, " AND t.date >= ?{param_idx}");
            param_values.push(Box::new(from.to_string()));
            param_idx += 1;
        }

        if let Some(to) = to_date {
            let _ = write!(sql, " AND t.date <= ?{param_idx}");
            param_values.push(Box::new(to.to_string()));
            let _ = param_idx;
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(AsRef::as_ref).collect();

        let row = conn.query_row(&sql, params_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(row)
    } else {
        let row = conn.query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN direction = 'debit' THEN amount ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN direction = 'credit' THEN amount ELSE 0 END), 0) \
             FROM entries \
             WHERE company_slug = ?1 AND account_code = ?2",
            params![company_slug, code],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(row)
    }
}

/// Computes a tax summary for a company, grouping entries by `tax_category`.
///
/// Returns one [`TaxSummaryRow`] per distinct tax category (excluding entries
/// with no tax category). Optionally filters by date range and by
/// transaction currency.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_tax_summary(
    conn: &Connection,
    company_slug: &str,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Option<&str>,
) -> Result<Vec<TaxSummaryRow>, CliError> {
    let mut sql = String::from(
        "SELECT e.tax_category, \
         SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), \
         SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.tax_category IS NOT NULL",
    );

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(company_slug.to_string()));

    if let Some(cur) = currency {
        let _ = write!(sql, " AND t.currency = ?{param_idx}");
        param_values.push(Box::new(cur.to_string()));
        param_idx += 1;
    }

    if let Some(from) = from_date {
        let _ = write!(sql, " AND t.date >= ?{param_idx}");
        param_values.push(Box::new(from.to_string()));
        param_idx += 1;
    }

    if let Some(to) = to_date {
        let _ = write!(sql, " AND t.date <= ?{param_idx}");
        param_values.push(Box::new(to.to_string()));
        let _ = param_idx;
    }

    sql.push_str(" GROUP BY e.tax_category ORDER BY e.tax_category");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(TaxSummaryRow {
            tax_category: row.get(0)?,
            debit_total: row.get(1)?,
            credit_total: row.get(2)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(db.conn(), "acme", "Acme Corp", None)
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        create_account(db.conn(), "acme", "1000", "Cash", "asset", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "4000", "Revenue", "revenue", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "2000", "Payables", "liability", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn post_sample(db: &Db, date: &str, amount: i64) {
        post_sample_in(db, date, amount, "USD");
    }

    fn post_sample_in(db: &Db, date: &str, amount: i64, currency: &str) {
        let entries = vec![
            transactions::PostEntryParams {
                account_code: "1000".to_string(),
                direction: "debit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
            transactions::PostEntryParams {
                account_code: "4000".to_string(),
                direction: "credit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
        ];
        let actor = Actor::new("test");
        let params = transactions::PostTransactionParams {
            company_slug: "acme",
            description: "Sale",
            metadata: None,
            currency,
            date,
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: transactions::ConflictStrategy::Error,
            actor: &actor,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    #[test]
    fn trial_balance_empty() {
        let db = setup();
        let balances =
            compute_trial_balance(db.conn(), "acme", None, None, None, None).unwrap_or_default();
        // All 3 accounts with zero balances
        assert_eq!(balances.len(), 3);
        for b in &balances {
            assert_eq!(b.debit_total, 0);
            assert_eq!(b.credit_total, 0);
        }
    }

    #[test]
    fn trial_balance_after_posting() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);

        let balances =
            compute_trial_balance(db.conn(), "acme", None, None, None, None).unwrap_or_default();
        assert_eq!(balances.len(), 3);

        let cash = balances.iter().find(|b| b.code == "1000");
        assert!(cash.is_some());
        let cash = cash.unwrap_or_else(|| panic!("missing cash"));
        assert_eq!(cash.debit_total, 5000);
        assert_eq!(cash.credit_total, 0);

        let revenue = balances.iter().find(|b| b.code == "4000");
        assert!(revenue.is_some());
        let revenue = revenue.unwrap_or_else(|| panic!("missing revenue"));
        assert_eq!(revenue.debit_total, 0);
        assert_eq!(revenue.credit_total, 5000);
    }

    #[test]
    fn trial_balance_with_type_filter() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);

        let balances = compute_trial_balance(db.conn(), "acme", Some("asset"), None, None, None)
            .unwrap_or_default();
        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].code, "1000");
    }

    #[test]
    fn trial_balance_with_as_of_date() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let balances =
            compute_trial_balance(db.conn(), "acme", None, None, Some("2024-01-31"), None)
                .unwrap_or_default();

        let cash = balances.iter().find(|b| b.code == "1000");
        assert!(cash.is_some());
        let cash = cash.unwrap_or_else(|| panic!("missing cash"));
        assert_eq!(cash.debit_total, 5000);
    }

    #[test]
    fn account_balance_empty() {
        let db = setup();
        let (dr, cr) =
            compute_account_balance(db.conn(), "acme", "1000", None, None, None).unwrap_or((0, 0));
        assert_eq!(dr, 0);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_after_posting() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) =
            compute_account_balance(db.conn(), "acme", "1000", None, None, None).unwrap_or((0, 0));
        assert_eq!(dr, 8000);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_with_as_of() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) =
            compute_account_balance(db.conn(), "acme", "1000", None, Some("2024-01-31"), None)
                .unwrap_or((0, 0));
        assert_eq!(dr, 5000);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_with_date_range() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);
        post_sample(&db, "2024-03-15", 2000);

        let (dr, cr) = compute_account_balance(
            db.conn(),
            "acme",
            "1000",
            Some("2024-02-01"),
            Some("2024-02-29"),
            None,
        )
        .unwrap_or((0, 0));
        assert_eq!(dr, 3000);
        assert_eq!(cr, 0);
    }

    #[test]
    fn trial_balance_with_date_range() -> Result<(), Box<dyn std::error::Error>> {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);
        post_sample(&db, "2024-03-15", 2000);

        let balances = compute_trial_balance(
            db.conn(),
            "acme",
            None,
            Some("2024-02-01"),
            Some("2024-02-29"),
            None,
        )
        .unwrap_or_default();
        let cash = balances
            .iter()
            .find(|b| b.code == "1000")
            .ok_or("expected matching item")?;
        assert_eq!(cash.debit_total, 3000);
        Ok(())
    }

    #[test]
    fn balances_filter_by_currency() -> Result<(), Box<dyn std::error::Error>> {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample_in(&db, "2024-02-15", 700, "MXN");

        let usd = compute_trial_balance(db.conn(), "acme", None, None, None, Some("USD"))?;
        // Zero-balance accounts are still listed.
        assert_eq!(usd.len(), 3);
        let cash = usd
            .iter()
            .find(|b| b.code == "1000")
            .ok_or("missing cash")?;
        assert_eq!(cash.debit_total, 5000);

        let mxn = compute_trial_balance(
            db.conn(),
            "acme",
            None,
            None,
            Some("2024-12-31"),
            Some("MXN"),
        )?;
        assert_eq!(mxn.len(), 3);
        let cash = mxn
            .iter()
            .find(|b| b.code == "1000")
            .ok_or("missing cash")?;
        assert_eq!(cash.debit_total, 700);

        let eur = compute_trial_balance(db.conn(), "acme", None, None, None, Some("EUR"))?;
        assert_eq!(eur.len(), 3);
        assert!(
            eur.iter()
                .all(|b| b.debit_total == 0 && b.credit_total == 0)
        );

        let (dr, cr) = compute_account_balance(db.conn(), "acme", "1000", None, None, Some("MXN"))?;
        assert_eq!((dr, cr), (700, 0));
        Ok(())
    }

    #[test]
    fn date_window_keeps_accounts_with_only_out_of_range_activity()
    -> Result<(), Box<dyn std::error::Error>> {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);

        // Every entry predates the window: all accounts are still listed, at zero.
        let rows = compute_trial_balance(
            db.conn(),
            "acme",
            None,
            Some("2024-06-01"),
            Some("2024-06-30"),
            None,
        )?;
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|b| b.debit_total == 0 && b.credit_total == 0)
        );

        // Same for an `--to` cutoff before any activity.
        let rows = compute_trial_balance(db.conn(), "acme", None, None, Some("2023-12-31"), None)?;
        assert_eq!(rows.len(), 3);
        Ok(())
    }
}
