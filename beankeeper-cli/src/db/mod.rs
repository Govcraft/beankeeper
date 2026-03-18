//! Database layer: connection management, schema, and CRUD operations.

pub mod accounts;
pub mod attachments;
pub mod companies;
pub mod connection;
pub mod schema;
pub mod transactions;

pub use accounts::{
    ListAccountParams, account_exists, create_account, delete_account, get_account,
    list_account_codes, list_accounts, row_to_account,
};
pub use attachments::{
    AttachmentRow, StoreAttachmentParams, get_attachment, hash_and_store_file, list_attachments,
    store_attachment,
};
pub use companies::{company_exists, create_company, delete_company, get_company, list_companies};
pub use connection::Db;
pub use schema::{ensure_schema, get_schema_version};
pub use transactions::{
    ListTransactionParams, OrphanedCorrelation, PostEntryParams, PostTransactionParams,
    count_transactions, find_orphaned_correlations, get_entries_for_transaction, get_transaction,
    list_transactions, post_transaction,
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
/// Optionally filters by account type, name substring, and as-of date.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn list_accounts_with_balances(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
    name_filter: Option<&str>,
    as_of: Option<&str>,
) -> Result<Vec<AccountWithBalanceRow>, CliError> {
    let base = if as_of.is_some() {
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

    if let Some(date) = as_of {
        let _ = write!(sql, " AND (t.date IS NULL OR t.date <= ?{param_idx})");
        param_values.push(Box::new(date.to_string()));
        let _ = param_idx;
    }

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
/// Optionally filters by account type and/or an as-of date (inclusive).
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_trial_balance(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
    as_of: Option<&str>,
) -> Result<Vec<BalanceRow>, CliError> {
    // Build the query dynamically based on whether we need date filtering.
    // When as_of is provided we must join through transactions to access the
    // date column. We use a parenthesised join so the LEFT JOIN still includes
    // accounts with no matching entries (zero balances).
    let base = if as_of.is_some() {
        "SELECT a.code, a.name, a.type, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN (entries e JOIN transactions t ON t.id = e.transaction_id) \
           ON e.company_slug = a.company_slug AND e.account_code = a.code"
    } else {
        "SELECT a.code, a.name, a.type, \
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

    if let Some(date) = as_of {
        let _ = write!(sql, " AND (t.date IS NULL OR t.date <= ?{param_idx})");
        param_values.push(Box::new(date.to_string()));
        let _ = param_idx;
    }

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
/// Returns `(debit_total, credit_total)` in minor units. If an `as_of` date
/// is provided, only entries from transactions on or before that date are
/// included.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_account_balance(
    conn: &Connection,
    company_slug: &str,
    code: &str,
    as_of: Option<&str>,
) -> Result<(i64, i64), CliError> {
    if let Some(date) = as_of {
        let row = conn.query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
             FROM entries e \
             JOIN transactions t ON t.id = e.transaction_id \
             WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.date <= ?3",
            params![company_slug, code, date],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
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
/// with no tax category). Optionally filters by date range.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on any database error.
pub fn compute_tax_summary(
    conn: &Connection,
    company_slug: &str,
    from_date: Option<&str>,
    to_date: Option<&str>,
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
        let params = transactions::PostTransactionParams {
            company_slug: "acme",
            description: "Sale",
            metadata: None,
            currency: "USD",
            date,
            entries: &entries,
            correlate: None,
            reference: None,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    #[test]
    fn trial_balance_empty() {
        let db = setup();
        let balances = compute_trial_balance(db.conn(), "acme", None, None).unwrap_or_default();
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

        let balances = compute_trial_balance(db.conn(), "acme", None, None).unwrap_or_default();
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

        let balances =
            compute_trial_balance(db.conn(), "acme", Some("asset"), None).unwrap_or_default();
        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].code, "1000");
    }

    #[test]
    fn trial_balance_with_as_of_date() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let balances =
            compute_trial_balance(db.conn(), "acme", None, Some("2024-01-31")).unwrap_or_default();

        let cash = balances.iter().find(|b| b.code == "1000");
        assert!(cash.is_some());
        let cash = cash.unwrap_or_else(|| panic!("missing cash"));
        assert_eq!(cash.debit_total, 5000);
    }

    #[test]
    fn account_balance_empty() {
        let db = setup();
        let (dr, cr) = compute_account_balance(db.conn(), "acme", "1000", None).unwrap_or((0, 0));
        assert_eq!(dr, 0);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_after_posting() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) = compute_account_balance(db.conn(), "acme", "1000", None).unwrap_or((0, 0));
        assert_eq!(dr, 8000);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_with_as_of() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) = compute_account_balance(db.conn(), "acme", "1000", Some("2024-01-31"))
            .unwrap_or((0, 0));
        assert_eq!(dr, 5000);
        assert_eq!(cr, 0);
    }
}
