//! Budget CRUD operations and variance computation.

use std::fmt::Write;

use rusqlite::Connection;

use crate::db::{BudgetRow, BudgetVarianceRow};
use crate::error::CliError;

/// Parameters for setting a budget.
pub struct SetBudgetParams<'a> {
    pub company_slug: &'a str,
    pub account_code: &'a str,
    pub currency: &'a str,
    pub year: i32,
    pub month: i32,
    pub amount: i64,
    pub notes: Option<&'a str>,
}

/// Upserts a single month's budget for an account.
///
/// Uses `INSERT OR REPLACE` on the unique `(company_slug, account_code, currency,
/// year, month)` constraint so repeated calls overwrite the previous value.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_budget(
    conn: &Connection,
    p: &SetBudgetParams<'_>,
) -> Result<BudgetRow, CliError> {
    conn.execute(
        "INSERT OR REPLACE INTO budgets \
         (company_slug, account_code, currency, year, month, amount, notes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![p.company_slug, p.account_code, p.currency, p.year, p.month, p.amount, p.notes],
    )?;

    let id = conn.last_insert_rowid();
    let created_at: String = conn.query_row(
        "SELECT created_at FROM budgets WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get(0),
    )?;

    Ok(BudgetRow {
        id,
        company_slug: p.company_slug.to_string(),
        account_code: p.account_code.to_string(),
        currency: p.currency.to_string(),
        year: p.year,
        month: p.month,
        amount: p.amount,
        notes: p.notes.map(String::from),
        created_at,
    })
}

/// Parameters for setting an annual budget.
pub struct SetAnnualBudgetParams<'a> {
    pub company_slug: &'a str,
    pub account_code: &'a str,
    pub currency: &'a str,
    pub year: i32,
    pub annual_amount: i64,
    pub notes: Option<&'a str>,
}

/// Distributes an annual budget amount evenly across 12 months and upserts each.
///
/// Each month gets `annual_amount / 12` minor units, with the first
/// `annual_amount % 12` months receiving an extra 1 unit so the total is exact.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_annual_budget(
    conn: &Connection,
    params: &SetAnnualBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    let base = params.annual_amount / 12;
    let remainder = params.annual_amount % 12;
    let mut rows = Vec::with_capacity(12);

    for m in 1..=12 {
        let extra = i64::from(i64::from(m) <= remainder);
        let row = set_budget(
            conn,
            &SetBudgetParams {
                company_slug: params.company_slug,
                account_code: params.account_code,
                currency: params.currency,
                year: params.year,
                month: m,
                amount: base + extra,
                notes: params.notes,
            },
        )?;
        rows.push(row);
    }

    Ok(rows)
}

/// Parameters for listing budgets.
pub struct ListBudgetParams<'a> {
    pub company_slug: &'a str,
    pub year: i32,
    pub account_code: Option<&'a str>,
    pub month: Option<i32>,
}

/// Lists budget rows with optional filters.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_budgets(
    conn: &Connection,
    params: &ListBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    let mut sql =
        String::from("SELECT id, company_slug, account_code, currency, year, month, amount, notes, created_at FROM budgets WHERE company_slug = ?1 AND year = ?2");

    let mut param_idx = 3u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(params.company_slug.to_string()));
    param_values.push(Box::new(params.year));

    if let Some(code) = params.account_code {
        let _ = write!(sql, " AND account_code = ?{param_idx}");
        param_values.push(Box::new(code.to_string()));
        param_idx += 1;
    }

    if let Some(m) = params.month {
        let _ = write!(sql, " AND month = ?{param_idx}");
        param_values.push(Box::new(m));
        let _ = param_idx;
    }

    sql.push_str(" ORDER BY account_code, month");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(BudgetRow {
            id: row.get(0)?,
            company_slug: row.get(1)?,
            account_code: row.get(2)?,
            currency: row.get(3)?,
            year: row.get(4)?,
            month: row.get(5)?,
            amount: row.get(6)?,
            notes: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Deletes budget rows for an account. If `month` is `None`, deletes all months
/// for that account/year/currency.
///
/// Returns the number of rows deleted.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn delete_budget(
    conn: &Connection,
    company_slug: &str,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<i32>,
) -> Result<usize, CliError> {
    let count = if let Some(m) = month {
        conn.execute(
            "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 AND year = ?4 AND month = ?5",
            rusqlite::params![company_slug, account_code, currency, year, m],
        )?
    } else {
        conn.execute(
            "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 AND year = ?4",
            rusqlite::params![company_slug, account_code, currency, year],
        )?
    };

    Ok(count)
}

/// Parameters for computing budget variance.
pub struct BudgetVarianceParams<'a> {
    pub company_slug: &'a str,
    pub currency: &'a str,
    pub year: i32,
    pub from_month: i32,
    pub to_month: i32,
    pub account_type: Option<&'a str>,
    pub include_unbudgeted: bool,
}

/// Computes budget vs actual variance for a company within a year and month range.
///
/// Joins `budgets` against aggregated entry totals from `entries`/`transactions`
/// for the corresponding date range. When `include_unbudgeted` is true, accounts
/// with actuals but no budget are included (with budget = 0).
///
/// Variance direction:
/// - Expense accounts: Budget - Actual (positive = favorable/underspent)
/// - All other accounts: Actual - Budget (positive = favorable/exceeded target)
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn compute_budget_variance(
    conn: &Connection,
    p: &BudgetVarianceParams<'_>,
) -> Result<Vec<BudgetVarianceRow>, CliError> {
    let from_date = format!("{}-{:02}-01", p.year, p.from_month);
    let to_date = last_day_of_month(p.year, p.to_month);

    let sql = if p.include_unbudgeted {
        build_variance_query_with_unbudgeted(p.account_type)
    } else {
        build_variance_query_budget_only(p.account_type)
    };

    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(p.company_slug.to_string()),
        Box::new(p.currency.to_string()),
        Box::new(p.year),
        Box::new(p.from_month),
        Box::new(p.to_month),
        Box::new(from_date),
        Box::new(to_date.clone()),
    ];

    if let Some(at) = p.account_type {
        param_values.push(Box::new(at.to_string()));
    }

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(BudgetVarianceRow {
            code: row.get(0)?,
            name: row.get(1)?,
            account_type: row.get(2)?,
            budget_amount: row.get(3)?,
            actual_amount: row.get(4)?,
            variance_amount: row.get(5)?,
            variance_percent: row.get(6)?,
            favorable: row.get(7)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Build the variance query that only includes accounts with a budget.
fn build_variance_query_budget_only(account_type: Option<&str>) -> String {
    let type_filter = if account_type.is_some() {
        " AND a.type = ?8"
    } else {
        ""
    };

    format!(
        "WITH budget_totals AS ( \
            SELECT account_code, SUM(amount) AS total \
            FROM budgets \
            WHERE company_slug = ?1 AND currency = ?2 AND year = ?3 \
              AND month >= ?4 AND month <= ?5 \
            GROUP BY account_code \
        ), \
        actual_totals AS ( \
            SELECT e.account_code, \
                CASE WHEN a.type IN ('asset', 'expense') \
                    THEN COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0) \
                       - COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
                    ELSE COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
                       - COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0) \
                END AS total \
            FROM entries e \
            JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
            JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code \
            WHERE e.company_slug = ?1 AND t.currency = ?2 \
              AND t.date >= ?6 AND t.date <= ?7 \
            GROUP BY e.account_code \
        ) \
        SELECT a.code, a.name, a.type, \
            COALESCE(b.total, 0) AS budget, \
            COALESCE(act.total, 0) AS actual, \
            CASE WHEN a.type = 'expense' \
                THEN COALESCE(b.total, 0) - COALESCE(act.total, 0) \
                ELSE COALESCE(act.total, 0) - COALESCE(b.total, 0) \
            END AS variance, \
            CASE WHEN COALESCE(b.total, 0) = 0 THEN NULL \
                WHEN a.type = 'expense' \
                    THEN ROUND(CAST(COALESCE(b.total, 0) - COALESCE(act.total, 0) AS REAL) / b.total * 100, 1) \
                ELSE ROUND(CAST(COALESCE(act.total, 0) - COALESCE(b.total, 0) AS REAL) / b.total * 100, 1) \
            END AS variance_pct, \
            CASE WHEN a.type = 'expense' \
                THEN COALESCE(b.total, 0) >= COALESCE(act.total, 0) \
                ELSE COALESCE(act.total, 0) >= COALESCE(b.total, 0) \
            END AS favorable \
        FROM budget_totals b \
        JOIN accounts a ON a.company_slug = ?1 AND a.code = b.account_code \
        LEFT JOIN actual_totals act ON act.account_code = b.account_code \
        WHERE 1=1{type_filter} \
        ORDER BY a.code"
    )
}

/// Build the variance query that includes unbudgeted accounts with actuals.
fn build_variance_query_with_unbudgeted(account_type: Option<&str>) -> String {
    let type_filter = if account_type.is_some() {
        " AND a.type = ?8"
    } else {
        ""
    };

    format!(
        "WITH budget_totals AS ( \
            SELECT account_code, SUM(amount) AS total \
            FROM budgets \
            WHERE company_slug = ?1 AND currency = ?2 AND year = ?3 \
              AND month >= ?4 AND month <= ?5 \
            GROUP BY account_code \
        ), \
        actual_totals AS ( \
            SELECT e.account_code, \
                CASE WHEN a.type IN ('asset', 'expense') \
                    THEN COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0) \
                       - COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
                    ELSE COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
                       - COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0) \
                END AS total \
            FROM entries e \
            JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
            JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code \
            WHERE e.company_slug = ?1 AND t.currency = ?2 \
              AND t.date >= ?6 AND t.date <= ?7 \
            GROUP BY e.account_code \
        ), \
        combined AS ( \
            SELECT account_code FROM budget_totals \
            UNION \
            SELECT account_code FROM actual_totals \
        ) \
        SELECT a.code, a.name, a.type, \
            COALESCE(b.total, 0) AS budget, \
            COALESCE(act.total, 0) AS actual, \
            CASE WHEN a.type = 'expense' \
                THEN COALESCE(b.total, 0) - COALESCE(act.total, 0) \
                ELSE COALESCE(act.total, 0) - COALESCE(b.total, 0) \
            END AS variance, \
            CASE WHEN COALESCE(b.total, 0) = 0 THEN NULL \
                WHEN a.type = 'expense' \
                    THEN ROUND(CAST(COALESCE(b.total, 0) - COALESCE(act.total, 0) AS REAL) / b.total * 100, 1) \
                ELSE ROUND(CAST(COALESCE(act.total, 0) - COALESCE(b.total, 0) AS REAL) / b.total * 100, 1) \
            END AS variance_pct, \
            CASE WHEN a.type = 'expense' \
                THEN COALESCE(b.total, 0) >= COALESCE(act.total, 0) \
                ELSE COALESCE(act.total, 0) >= COALESCE(b.total, 0) \
            END AS favorable \
        FROM combined c \
        JOIN accounts a ON a.company_slug = ?1 AND a.code = c.account_code \
        LEFT JOIN budget_totals b ON b.account_code = c.account_code \
        LEFT JOIN actual_totals act ON act.account_code = c.account_code \
        WHERE 1=1{type_filter} \
        ORDER BY a.code"
    )
}

/// Returns the last day of a given month as a `YYYY-MM-DD` string.
fn last_day_of_month(year: i32, month: i32) -> String {
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 31,
    };
    format!("{year}-{month:02}-{days:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, create_account, create_company, post_transaction};
    use crate::db::transactions::{ConflictStrategy, PostEntryParams, PostTransactionParams};

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(db.conn(), "acme", "Acme Corp", None)
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        create_account(db.conn(), "acme", "1000", "Cash", "asset", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "4000", "Revenue", "revenue", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "5000", "Rent Expense", "expense", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "5100", "Supplies", "expense", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn post_expense(db: &Db, date: &str, expense_code: &str, amount: i64) {
        let entries = vec![
            PostEntryParams {
                account_code: expense_code.to_string(),
                direction: "debit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: "credit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
        ];
        let params = PostTransactionParams {
            company_slug: "acme",
            description: "Test expense",
            metadata: None,
            currency: "USD",
            date,
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: ConflictStrategy::Error,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    fn post_revenue(db: &Db, date: &str, amount: i64) {
        let entries = vec![
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: "debit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".to_string(),
                direction: "credit".to_string(),
                amount,
                memo: None,
                tax_category: None,
            },
        ];
        let params = PostTransactionParams {
            company_slug: "acme",
            description: "Test revenue",
            metadata: None,
            currency: "USD",
            date,
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: ConflictStrategy::Error,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    fn budget<'a>(account: &'a str, year: i32, month: i32, amount: i64, notes: Option<&'a str>) -> SetBudgetParams<'a> {
        SetBudgetParams { company_slug: "acme", account_code: account, currency: "USD", year, month, amount, notes }
    }

    fn annual<'a>(account: &'a str, year: i32, amount: i64, notes: Option<&'a str>) -> SetAnnualBudgetParams<'a> {
        SetAnnualBudgetParams { company_slug: "acme", account_code: account, currency: "USD", year, annual_amount: amount, notes }
    }

    fn variance<'a>(year: i32, from: i32, to: i32, acct_type: Option<&'a str>, unbudgeted: bool) -> BudgetVarianceParams<'a> {
        BudgetVarianceParams { company_slug: "acme", currency: "USD", year, from_month: from, to_month: to, account_type: acct_type, include_unbudgeted: unbudgeted }
    }

    #[test]
    fn set_budget_creates_row() {
        let db = setup();
        let row = set_budget(db.conn(), &budget("5000", 2026, 3, 250_000, Some("March rent"))).unwrap();
        assert_eq!(row.account_code, "5000");
        assert_eq!(row.year, 2026);
        assert_eq!(row.month, 3);
        assert_eq!(row.amount, 250_000);
        assert_eq!(row.notes.as_deref(), Some("March rent"));
    }

    #[test]
    fn set_budget_upserts() {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 3, 100_000, None)).unwrap();
        let row = set_budget(db.conn(), &budget("5000", 2026, 3, 200_000, Some("updated"))).unwrap();
        assert_eq!(row.amount, 200_000);

        let rows = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: Some("5000"), month: Some(3),
        }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].amount, 200_000);
    }

    #[test]
    fn set_annual_budget_distributes_evenly() {
        let db = setup();
        let rows = set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None)).unwrap();
        assert_eq!(rows.len(), 12);
        for row in &rows {
            assert_eq!(row.amount, 10_000);
        }
        let total: i64 = rows.iter().map(|r| r.amount).sum();
        assert_eq!(total, 120_000);
    }

    #[test]
    fn set_annual_budget_distributes_remainder() {
        let db = setup();
        let rows = set_annual_budget(db.conn(), &annual("5000", 2026, 100_000, None)).unwrap();
        assert_eq!(rows.len(), 12);
        let total: i64 = rows.iter().map(|r| r.amount).sum();
        assert_eq!(total, 100_000);
        for row in &rows[..4] { assert_eq!(row.amount, 8334); }
        for row in &rows[4..] { assert_eq!(row.amount, 8333); }
    }

    #[test]
    fn list_budgets_filters() {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None)).unwrap();
        set_budget(db.conn(), &budget("5100", 2026, 1, 5000, None)).unwrap();

        let all = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: None, month: None,
        }).unwrap();
        assert_eq!(all.len(), 13);

        let rent_only = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: Some("5000"), month: None,
        }).unwrap();
        assert_eq!(rent_only.len(), 12);

        let jan = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: None, month: Some(1),
        }).unwrap();
        assert_eq!(jan.len(), 2);
    }

    #[test]
    fn delete_budget_single_month() {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None)).unwrap();
        let deleted = delete_budget(db.conn(), "acme", "5000", "USD", 2026, Some(3)).unwrap();
        assert_eq!(deleted, 1);
        let remaining = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: Some("5000"), month: None,
        }).unwrap();
        assert_eq!(remaining.len(), 11);
    }

    #[test]
    fn delete_budget_all_months() {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None)).unwrap();
        let deleted = delete_budget(db.conn(), "acme", "5000", "USD", 2026, None).unwrap();
        assert_eq!(deleted, 12);
        let remaining = list_budgets(db.conn(), &ListBudgetParams {
            company_slug: "acme", year: 2026, account_code: Some("5000"), month: None,
        }).unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn variance_expense_under_budget() {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None)).unwrap();
        post_expense(&db, "2026-01-15", "5000", 80_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, false)).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.code, "5000");
        assert_eq!(r.budget_amount, 100_000);
        assert_eq!(r.actual_amount, 80_000);
        assert_eq!(r.variance_amount, 20_000);
        assert!(r.favorable);
    }

    #[test]
    fn variance_expense_over_budget() {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 50_000, None)).unwrap();
        post_expense(&db, "2026-01-15", "5000", 80_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, false)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].variance_amount, -30_000);
        assert!(!rows[0].favorable);
    }

    #[test]
    fn variance_revenue_exceeds_target() {
        let db = setup();
        set_budget(db.conn(), &budget("4000", 2026, 1, 100_000, None)).unwrap();
        post_revenue(&db, "2026-01-15", 150_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, false)).unwrap();
        let r = rows.iter().find(|r| r.code == "4000").unwrap();
        assert_eq!(r.budget_amount, 100_000);
        assert_eq!(r.actual_amount, 150_000);
        assert_eq!(r.variance_amount, 50_000);
        assert!(r.favorable);
    }

    #[test]
    fn variance_revenue_misses_target() {
        let db = setup();
        set_budget(db.conn(), &budget("4000", 2026, 1, 200_000, None)).unwrap();
        post_revenue(&db, "2026-01-15", 100_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, false)).unwrap();
        let r = rows.iter().find(|r| r.code == "4000").unwrap();
        assert_eq!(r.variance_amount, -100_000);
        assert!(!r.favorable);
    }

    #[test]
    fn variance_multi_month_aggregation() {
        let db = setup();
        for m in 1..=3 {
            set_budget(db.conn(), &budget("5000", 2026, m, 50_000, None)).unwrap();
        }
        post_expense(&db, "2026-01-15", "5000", 40_000);
        post_expense(&db, "2026-02-15", "5000", 60_000);
        post_expense(&db, "2026-03-15", "5000", 50_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 3, None, false)).unwrap();
        let r = rows.iter().find(|r| r.code == "5000").unwrap();
        assert_eq!(r.budget_amount, 150_000);
        assert_eq!(r.actual_amount, 150_000);
        assert_eq!(r.variance_amount, 0);
        assert!(r.favorable);
    }

    #[test]
    fn variance_include_unbudgeted() {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None)).unwrap();
        post_expense(&db, "2026-01-15", "5000", 80_000);
        post_expense(&db, "2026-01-20", "5100", 30_000);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, false)).unwrap();
        assert_eq!(rows.len(), 1);

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, None, true)).unwrap();
        assert!(rows.len() >= 2);
        let supplies = rows.iter().find(|r| r.code == "5100").unwrap();
        assert_eq!(supplies.budget_amount, 0);
        assert_eq!(supplies.actual_amount, 30_000);
    }

    #[test]
    fn variance_type_filter() {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None)).unwrap();
        set_budget(db.conn(), &budget("4000", 2026, 1, 200_000, None)).unwrap();

        let rows = compute_budget_variance(db.conn(), &variance(2026, 1, 1, Some("expense"), false)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "5000");
    }

    #[test]
    fn last_day_of_month_feb_leap() {
        assert_eq!(last_day_of_month(2024, 2), "2024-02-29");
        assert_eq!(last_day_of_month(2025, 2), "2025-02-28");
        assert_eq!(last_day_of_month(2026, 1), "2026-01-31");
        assert_eq!(last_day_of_month(2026, 4), "2026-04-30");
        assert_eq!(last_day_of_month(2026, 12), "2026-12-31");
    }
}
