//! Database layer: connection management, schema, and CRUD operations.

pub mod accounts;
pub mod attachments;
pub mod audit;
pub mod budgets;
pub mod clearings;
pub mod companies;
pub mod connection;
pub(crate) mod direction;
pub mod ids;
pub mod investments;
pub mod metadata;
pub mod metadata_filter;
pub mod periods;
pub mod policy;
pub mod proposals;
pub mod rules;
pub mod schedules;
pub mod schema;
pub mod spend_limits;
pub mod transactions;
pub mod verification;

pub use accounts::{
    CreateAccountParams, ListAccountParams, account_exists, create_account, get_account,
    list_account_codes, list_accounts, row_to_account,
};
pub use attachments::{
    AttachmentRow, StoreAttachmentParams, get_attachment, hash_and_store_file, list_attachments,
    store_attachment,
};
pub use audit::{
    Actor, AuditLogRow, ClearAction, ClearOutcome, ClearRequest, EntryStatus, ListAuditParams,
    atomically, clear_entry, correlate_partner, count_audit, delete_account, delete_budget,
    delete_company, list_audit, set_annual_budget, set_budget, set_entry_status, unclear_entry,
};
pub use budgets::{
    BudgetCoverage, BudgetMonths, BudgetVarianceParams, BudgetVarianceReport, ListBudgetParams,
    SetAnnualBudgetParams, SetBudgetParams, compute_budget_variance, list_budgets,
};
pub use clearings::{
    ClearingState, ClearingStatus, EntryTarget, find_entry_by_reference, get_entry_target,
};
pub use companies::{company_exists, create_company, get_company, list_companies};
pub use connection::Db;
pub use schema::{ensure_schema, get_schema_version};
pub use transactions::{
    ConflictStrategy, ListTransactionParams, OrphanedCorrelation, PostEntryParams, PostResult,
    PostTransactionParams, count_transactions, find_orphaned_correlations,
    get_entries_for_transaction, get_transaction, list_transactions, post_transaction,
};

use std::fmt::Write;

use beankeeper::types::{Currency, Money};
use chrono::NaiveDate;
use rusqlite::{Connection, params, params_from_iter};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub company_slug: String,
    pub code: String,
    pub name: String,
    pub account_type: beankeeper::types::AccountType,
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
    pub currency: Currency,
    pub date: String,
    pub posted_at: String,
    pub reference: Option<String>,
    /// Unknown for transactions predating creation auditing.
    pub posted_by: Option<String>,
    pub agent_uri: Option<String>,
    pub token_jti: Option<String>,
}

/// A row from the `entries` table.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: i64,
    pub transaction_id: i64,
    pub account_code: String,
    pub company_slug: String,
    pub direction: beankeeper::types::DebitOrCredit,
    pub amount: i64,
    pub memo: Option<String>,
    pub tax_category: Option<String>,
    pub status: EntryStatus,
}

/// A row from the `budgets` table.
#[derive(Debug, Clone)]
pub struct BudgetRow {
    pub id: i64,
    pub company_slug: String,
    pub account_code: String,
    pub currency: Currency,
    pub year: i32,
    pub month: i32,
    pub amount: i64,
    pub notes: Option<String>,
    pub created_at: String,
}

impl BudgetRow {
    /// Interpret the stored amount in the row's own currency.
    pub(crate) fn money(&self) -> Money {
        Money::from_minor(i128::from(self.amount), self.currency)
    }
}

/// A computed row for the budget-variance report.
#[derive(Debug, Clone)]
pub struct BudgetVarianceRow {
    pub code: String,
    pub name: String,
    pub account_type: beankeeper::types::AccountType,
    pub budget_amount: beankeeper::types::Money,
    pub actual_amount: beankeeper::types::Money,
    pub variance_amount: beankeeper::types::Money,
    pub variance_percent: Option<crate::percent::Percent>,
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

/// Account with aggregated debit and credit totals in one currency.
#[derive(Debug, Clone)]
pub struct AccountWithBalanceRow {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub default_tax_category: Option<String>,
    pub debit_total: Money,
    pub credit_total: Money,
}

/// Inclusive date bounds for account activity. Absent bounds are open-ended.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountBalanceWindow {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

impl AccountBalanceWindow {
    /// Construct a window with optional endpoints.
    ///
    /// # Errors
    /// Returns [`CliError::Validation`] if the start follows the end.
    pub fn new(from: Option<NaiveDate>, to: Option<NaiveDate>) -> Result<Self, CliError> {
        if let (Some(from), Some(to)) = (from, to)
            && from > to
        {
            return Err(CliError::Validation(format!(
                "--from ({from}) must be on or before --to ({to})"
            )));
        }
        Ok(Self { from, to })
    }
}

/// List accounts with totals for the selected currency and date window.
/// Accounts with no activity in scope are returned with zero totals.
///
/// # Errors
/// Returns [`CliError::Sqlite`] on database errors, including an overflowing sum.
pub fn list_accounts_with_balances(
    conn: &Connection,
    filter: &ListAccountParams<'_>,
    window: AccountBalanceWindow,
    currency: Currency,
) -> Result<Vec<AccountWithBalanceRow>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT a.code, a.name, a.type, a.default_tax_category,
                COALESCE(activity.debit_total, 0), COALESCE(activity.credit_total, 0)
         FROM accounts a
         LEFT JOIN (
             SELECT e.account_code,
                    SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END) AS debit_total,
                    SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END) AS credit_total
             FROM transactions t
             JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug
             WHERE t.company_slug = ?1 AND t.currency = ?2
               AND (?3 IS NULL OR substr(t.date, 1, 10) >= ?3)
               AND (?4 IS NULL OR substr(t.date, 1, 10) <= ?4)
             GROUP BY e.account_code
         ) activity ON activity.account_code = a.code
         WHERE a.company_slug = ?1
           AND (?5 IS NULL OR a.type = ?5 COLLATE NOCASE)
           AND (?6 IS NULL OR a.name LIKE '%' || ?6 || '%')
         ORDER BY a.code",
    )?;
    let rows = stmt.query_map(
        params![
            filter.company_slug,
            currency.code(),
            window.from.map(|date| date.to_string()),
            window.to.map(|date| date.to_string()),
            filter.type_filter.map(beankeeper::types::AccountType::code),
            filter.name_filter,
        ],
        |row| {
            Ok(AccountWithBalanceRow {
                code: row.get(0)?,
                name: row.get(1)?,
                account_type: row.get(2)?,
                default_tax_category: row.get(3)?,
                debit_total: Money::from_minor(i128::from(row.get::<_, i64>(4)?), currency),
                credit_total: Money::from_minor(i128::from(row.get::<_, i64>(5)?), currency),
            })
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(CliError::from)
}

/// Computes debit and credit totals for every account in one currency.
///
/// Date filters are inclusive. Accounts with no matching entries have zero totals.
///
/// # Errors
///
/// Returns [`CliError::Sqlite`] on database errors.
pub fn compute_trial_balance(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Currency,
) -> Result<Vec<BalanceRow>, CliError> {
    let mut sql = String::from(
        "SELECT a.code, a.name, a.type, \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM accounts a \
         LEFT JOIN (entries e JOIN transactions t ON t.id = e.transaction_id \
           AND t.company_slug = e.company_slug AND t.currency = ?2",
    );
    let filter = type_filter.map(str::to_lowercase);
    let mut bindings = vec![company_slug, currency.code()];
    push_transaction_dates(&mut sql, &mut bindings, from_date, to_date);
    sql.push_str(
        ") ON e.company_slug = a.company_slug AND e.account_code = a.code \
         WHERE a.company_slug = ?1",
    );
    if let Some(filter) = filter.as_deref() {
        bindings.push(filter);
        let _ = write!(sql, " AND a.type = ?{}", bindings.len());
    }
    sql.push_str(" GROUP BY a.code, a.name, a.type ORDER BY a.code");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| {
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

/// Computes a single account's debit and credit totals in one currency.
///
/// Date filters are inclusive. Totals are returned in minor units.
///
/// # Errors
///
/// Returns [`CliError::Sqlite`] on database errors.
pub fn compute_account_balance(
    conn: &Connection,
    company_slug: &str,
    code: &str,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Currency,
) -> Result<(i64, i64), CliError> {
    let mut sql = String::from(
        "SELECT \
         COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
         COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3",
    );
    let mut bindings = vec![company_slug, code, currency.code()];
    push_transaction_dates(&mut sql, &mut bindings, from_date, to_date);

    Ok(conn.query_row(&sql, params_from_iter(bindings), |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?)
}

/// Computes a tax summary in one currency, grouped by tax category.
///
/// Entries without a tax category are excluded. Date filters are inclusive.
///
/// # Errors
///
/// Returns [`CliError::Sqlite`] on database errors.
pub fn compute_tax_summary(
    conn: &Connection,
    company_slug: &str,
    from_date: Option<&str>,
    to_date: Option<&str>,
    currency: Currency,
) -> Result<Vec<TaxSummaryRow>, CliError> {
    let mut sql = String::from(
        "SELECT e.tax_category, \
         SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), \
         SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
         WHERE e.company_slug = ?1 AND e.tax_category IS NOT NULL AND t.currency = ?2",
    );
    let mut bindings = vec![company_slug, currency.code()];
    push_transaction_dates(&mut sql, &mut bindings, from_date, to_date);
    sql.push_str(" GROUP BY e.tax_category ORDER BY e.tax_category");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| {
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

fn push_transaction_dates<'a>(
    sql: &mut String,
    bindings: &mut Vec<&'a str>,
    from: Option<&'a str>,
    to: Option<&'a str>,
) {
    if let Some(from) = from {
        bindings.push(from);
        let _ = write!(sql, " AND t.date >= ?{}", bindings.len());
    }
    if let Some(to) = to {
        bindings.push(to);
        let _ = write!(sql, " AND t.date <= ?{}", bindings.len());
    }
}

#[cfg(test)]
mod tests {
    use crate::db::CreateAccountParams;

    use super::*;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            "acme",
            "Acme Corp",
            None,
        )
        .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "1000",
                name: "Cash",
                account_type: "asset",
                default_tax_category: None,
            },
        )
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "4000",
                name: "Revenue",
                account_type: "revenue",
                default_tax_category: None,
            },
        )
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "2000",
                name: "Payables",
                account_type: "liability",
                default_tax_category: None,
            },
        )
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
        let balances = compute_trial_balance(db.conn(), "acme", None, None, None, Currency::USD)
            .unwrap_or_default();
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

        let balances = compute_trial_balance(db.conn(), "acme", None, None, None, Currency::USD)
            .unwrap_or_default();
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
            compute_trial_balance(db.conn(), "acme", Some("asset"), None, None, Currency::USD)
                .unwrap_or_default();
        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].code, "1000");
    }

    #[test]
    fn trial_balance_with_as_of_date() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let balances = compute_trial_balance(
            db.conn(),
            "acme",
            None,
            None,
            Some("2024-01-31"),
            Currency::USD,
        )
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
            compute_account_balance(db.conn(), "acme", "1000", None, None, Currency::USD)
                .unwrap_or((0, 0));
        assert_eq!(dr, 0);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_after_posting() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) =
            compute_account_balance(db.conn(), "acme", "1000", None, None, Currency::USD)
                .unwrap_or((0, 0));
        assert_eq!(dr, 8000);
        assert_eq!(cr, 0);
    }

    #[test]
    fn account_balance_with_as_of() {
        let db = setup();
        post_sample(&db, "2024-01-15", 5000);
        post_sample(&db, "2024-02-15", 3000);

        let (dr, cr) = compute_account_balance(
            db.conn(),
            "acme",
            "1000",
            None,
            Some("2024-01-31"),
            Currency::USD,
        )
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
            Currency::USD,
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
            Currency::USD,
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

        let usd = compute_trial_balance(db.conn(), "acme", None, None, None, Currency::USD)?;
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
            Currency::MXN,
        )?;
        assert_eq!(mxn.len(), 3);
        let cash = mxn
            .iter()
            .find(|b| b.code == "1000")
            .ok_or("missing cash")?;
        assert_eq!(cash.debit_total, 700);

        let eur = compute_trial_balance(db.conn(), "acme", None, None, None, Currency::EUR)?;
        assert_eq!(eur.len(), 3);
        assert!(
            eur.iter()
                .all(|b| b.debit_total == 0 && b.credit_total == 0)
        );

        let (dr, cr) =
            compute_account_balance(db.conn(), "acme", "1000", None, None, Currency::MXN)?;
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
            Currency::USD,
        )?;
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|b| b.debit_total == 0 && b.credit_total == 0)
        );

        // Same for an `--to` cutoff before any activity.
        let rows = compute_trial_balance(
            db.conn(),
            "acme",
            None,
            None,
            Some("2023-12-31"),
            Currency::USD,
        )?;
        assert_eq!(rows.len(), 3);
        Ok(())
    }
}
