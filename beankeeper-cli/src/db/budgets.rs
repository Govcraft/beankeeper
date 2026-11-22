//! Budget CRUD operations and variance computation.

use std::fmt::Write;

use beankeeper::types::{AccountType, Amount, Currency, Money};
use chrono::{Month, NaiveDate};
use rusqlite::{Connection, types::Type};

use super::connection::ReadConn;
use crate::db::{BudgetRow, BudgetVarianceRow};
use crate::error::CliError;

/// Parameters for setting a budget.
pub struct SetBudgetParams<'a> {
    pub company_slug: &'a str,
    pub account_code: &'a str,
    pub currency: Currency,
    pub year: i32,
    pub month: i32,
    pub amount: i64,
    pub notes: Option<&'a str>,
}

/// Parameters for setting an annual budget.
pub struct SetAnnualBudgetParams<'a> {
    pub company_slug: &'a str,
    pub account_code: &'a str,
    pub currency: Currency,
    pub year: i32,
    pub annual_amount: i64,
    pub notes: Option<&'a str>,
}

/// Company, account, currency and calendar selection for a budget deletion.
pub struct DeleteBudgetParams<'a> {
    pub company_slug: &'a str,
    pub account_code: &'a str,
    pub currency: Currency,
    pub year: i32,
    pub month: Option<i32>,
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
    conn: ReadConn<'_>,
    params: &ListBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    let mut sql = String::from(
        "SELECT id, company_slug, account_code, currency, year, month, amount, notes, created_at FROM budgets WHERE company_slug = ?1 AND year = ?2",
    );

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

/// Inclusive month bounds in a single budget year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetMonths {
    year: i32,
    from: Month,
    to: Month,
    first_day: NaiveDate,
    last_day: NaiveDate,
}

impl BudgetMonths {
    /// Validate ordered months in a representable calendar year.
    ///
    /// # Errors
    /// Returns [`CliError::Validation`] for a reversed span or unrepresentable year.
    pub fn new(year: i32, from: Month, to: Month) -> Result<Self, CliError> {
        let from_number = from.number_from_month();
        let to_number = to.number_from_month();
        if from_number > to_number {
            return Err(CliError::Validation(format!(
                "from month ({from_number}) must be <= to month ({to_number})"
            )));
        }
        let invalid_year = || CliError::Validation(format!("unsupported budget year {year}"));
        let first_day = NaiveDate::from_ymd_opt(year, from_number, 1).ok_or_else(invalid_year)?;
        let last_day = to
            .num_days(year)
            .and_then(|day| NaiveDate::from_ymd_opt(year, to_number, u32::from(day)))
            .ok_or_else(invalid_year)?;
        Ok(Self {
            year,
            from,
            to,
            first_day,
            last_day,
        })
    }

    /// Year of both endpoints.
    #[must_use]
    pub const fn year(self) -> i32 {
        self.year
    }

    /// First included month.
    #[must_use]
    pub const fn from(self) -> Month {
        self.from
    }

    /// Last included month.
    #[must_use]
    pub const fn to(self) -> Month {
        self.to
    }

    /// First and last included calendar dates.
    #[must_use]
    pub const fn dates(self) -> (NaiveDate, NaiveDate) {
        (self.first_day, self.last_day)
    }
}

/// Choose which accounts the variance report includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCoverage {
    /// Accounts with budgets in the requested interval.
    Budgeted,
    /// Also include accounts with actual activity and no budget.
    IncludeUnbudgeted,
}

/// Parameters for computing budget variance.
pub struct BudgetVarianceParams<'a> {
    /// Company whose budgets and actuals are being compared.
    pub company_slug: &'a str,
    /// Currency of both budgets and actuals.
    pub currency: Currency,
    /// Budget month range.
    pub months: BudgetMonths,
    /// Optional account type filter.
    pub account_type: Option<AccountType>,
    /// Whether actuals without budgets are included.
    pub coverage: BudgetCoverage,
}

/// The budget and actual activity for a single currency and month range.
#[derive(Debug)]
pub struct BudgetVarianceReport {
    currency: Currency,
    months: BudgetMonths,
    rows: Vec<BudgetVarianceRow>,
    total_budget: Money,
    total_actual: Money,
}

impl BudgetVarianceReport {
    /// Currency shared by all values in this report.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Inclusive covered months.
    #[must_use]
    pub const fn months(&self) -> BudgetMonths {
        self.months
    }

    /// Per-account results, ordered by account code.
    #[must_use]
    pub fn rows(&self) -> &[BudgetVarianceRow] {
        &self.rows
    }

    /// Total budget for all included accounts.
    #[must_use]
    pub const fn total_budget(&self) -> Money {
        self.total_budget
    }

    /// Total actual activity for all included accounts.
    #[must_use]
    pub const fn total_actual(&self) -> Money {
        self.total_actual
    }
}

fn report_overflow() -> CliError {
    CliError::Validation("budget variance total exceeds the supported minor-unit range".into())
}

/// Compute budget versus actual variance in one currency for the given months.
///
/// Positive variance means underspending for expenses and exceeding the target
/// for other account types.
///
/// # Errors
/// Returns [`CliError`] for an unknown company, invalid stored data, arithmetic
/// overflow, or database errors.
pub fn compute_budget_variance(
    conn: &Connection,
    params: &BudgetVarianceParams<'_>,
) -> Result<BudgetVarianceReport, CliError> {
    let rows = super::audit::atomically(conn, || {
        super::companies::get_company(ReadConn::new(conn), params.company_slug)?;
        load_variance_rows(conn, params)
    })?;
    let total = |select: fn(&BudgetVarianceRow) -> Money| {
        let amount = rows.iter().try_fold(Amount::new(0), |total, row| {
            total
                .checked_add(select(row).amount())
                .ok_or_else(report_overflow)
        })?;
        i64::try_from(amount.minor_units()).map_err(|_| report_overflow())?;
        Ok::<_, CliError>(Money::new(amount, params.currency))
    };
    Ok(BudgetVarianceReport {
        currency: params.currency,
        months: params.months,
        total_budget: total(|row| row.budget_amount)?,
        total_actual: total(|row| row.actual_amount)?,
        rows,
    })
}

fn load_variance_rows(
    conn: &Connection,
    p: &BudgetVarianceParams<'_>,
) -> Result<Vec<BudgetVarianceRow>, CliError> {
    let (from_date, to_date) = p.months.dates();
    let sql = match p.coverage {
        BudgetCoverage::Budgeted => build_variance_query_budget_only(p.account_type),
        BudgetCoverage::IncludeUnbudgeted => build_variance_query_with_unbudgeted(p.account_type),
    };

    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(p.company_slug.to_string()),
        Box::new(p.currency.code().to_owned()),
        Box::new(p.months.year()),
        Box::new(p.months.from().number_from_month()),
        Box::new(p.months.to().number_from_month()),
        Box::new(from_date.to_string()),
        Box::new(to_date.to_string()),
    ];

    if let Some(at) = p.account_type {
        param_values.push(Box::new(at.code()));
    }

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        let account_type: String = row.get(2)?;
        let account_type = account_type.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
        })?;
        let budget_amount: i64 = row.get(3)?;
        let actual_amount: i64 = row.get(4)?;
        let variance_amount: i64 = row.get(5)?;
        let money = |minor| Money::from_minor(i128::from(minor), p.currency);
        Ok(BudgetVarianceRow {
            code: row.get(0)?,
            name: row.get(1)?,
            account_type,
            budget_amount: money(budget_amount),
            actual_amount: money(actual_amount),
            variance_amount: money(variance_amount),
            variance_percent: crate::percent::Percent::of(variance_amount, budget_amount),
            favorable: row.get(6)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Build the variance query that only includes accounts with a budget.
fn build_variance_query_budget_only(account_type: Option<AccountType>) -> String {
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
fn build_variance_query_with_unbudgeted(account_type: Option<AccountType>) -> String {
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

#[cfg(test)]
mod tests {
    use crate::db::CreateAccountParams;

    use super::*;
    use crate::db::transactions::{ConflictStrategy, PostEntryParams, PostTransactionParams};
    use crate::db::{Actor, Db, audit, create_account, create_company, post_transaction};

    fn test_actor() -> Actor {
        Actor::new("test")
    }

    // Test-local shims onto the audited mutation API, so the audited path is
    // exercised here without restating the actor at every call site.
    fn set_budget(conn: &Connection, p: &SetBudgetParams<'_>) -> Result<BudgetRow, CliError> {
        audit::set_budget(conn, &test_actor(), p)
    }

    fn set_annual_budget(
        conn: &Connection,
        p: &SetAnnualBudgetParams<'_>,
    ) -> Result<Vec<BudgetRow>, CliError> {
        audit::set_annual_budget(conn, &test_actor(), p)
    }

    fn delete_budget(
        conn: &Connection,
        company_slug: &str,
        account_code: &str,
        currency: Currency,
        year: i32,
        month: Option<i32>,
    ) -> Result<usize, CliError> {
        audit::delete_budget(
            conn,
            &test_actor(),
            &DeleteBudgetParams {
                company_slug,
                account_code,
                currency,
                year,
                month,
            },
            crate::confirmation::Confirmed::forced(true)?,
        )
    }

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
                account_type: beankeeper::types::AccountType::Asset,
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
                account_type: beankeeper::types::AccountType::Revenue,
                default_tax_category: None,
            },
        )
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "5000",
                name: "Rent Expense",
                account_type: beankeeper::types::AccountType::Expense,
                default_tax_category: None,
            },
        )
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "5100",
                name: "Supplies",
                account_type: beankeeper::types::AccountType::Expense,
                default_tax_category: None,
            },
        )
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn post_expense(db: &Db, date: &str, expense_code: &str, amount: i64) {
        let entries = vec![
            PostEntryParams {
                account_code: expense_code.to_string(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount,
                memo: None,
                tax_category: None,
            },
        ];
        let actor = test_actor();
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
            actor: &actor,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    fn post_revenue(db: &Db, date: &str, amount: i64) {
        let entries = vec![
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount,
                memo: None,
                tax_category: None,
            },
        ];
        let actor = test_actor();
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
            actor: &actor,
        };
        post_transaction(db.conn(), &params).unwrap_or_else(|e| panic!("post failed: {e}"));
    }

    fn budget<'a>(
        account: &'a str,
        year: i32,
        month: i32,
        amount: i64,
        notes: Option<&'a str>,
    ) -> SetBudgetParams<'a> {
        SetBudgetParams {
            company_slug: "acme",
            account_code: account,
            currency: Currency::USD,
            year,
            month,
            amount,
            notes,
        }
    }

    fn annual<'a>(
        account: &'a str,
        year: i32,
        amount: i64,
        notes: Option<&'a str>,
    ) -> SetAnnualBudgetParams<'a> {
        SetAnnualBudgetParams {
            company_slug: "acme",
            account_code: account,
            currency: Currency::USD,
            year,
            annual_amount: amount,
            notes,
        }
    }

    fn variance(
        year: i32,
        from: Month,
        to: Month,
        account_type: Option<AccountType>,
        unbudgeted: bool,
    ) -> BudgetVarianceParams<'static> {
        BudgetVarianceParams {
            company_slug: "acme",
            currency: Currency::USD,
            months: BudgetMonths::new(year, from, to).unwrap_or_else(|error| panic!("{error}")),
            account_type,
            coverage: if unbudgeted {
                BudgetCoverage::IncludeUnbudgeted
            } else {
                BudgetCoverage::Budgeted
            },
        }
    }

    #[test]
    fn set_budget_creates_row() -> Result<(), CliError> {
        let db = setup();
        let row = set_budget(
            db.conn(),
            &budget("5000", 2026, 3, 250_000, Some("March rent")),
        )?;
        assert_eq!(row.account_code, "5000");
        assert_eq!(row.year, 2026);
        assert_eq!(row.month, 3);
        assert_eq!(row.amount, 250_000);
        assert_eq!(row.notes.as_deref(), Some("March rent"));
        Ok(())
    }

    #[test]
    fn set_budget_upserts() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 3, 100_000, None))?;
        let row = set_budget(
            db.conn(),
            &budget("5000", 2026, 3, 200_000, Some("updated")),
        )?;
        assert_eq!(row.amount, 200_000);

        let rows = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: Some("5000"),
                month: Some(3),
            },
        )?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].amount, 200_000);
        Ok(())
    }

    #[test]
    fn set_annual_budget_distributes_evenly() -> Result<(), CliError> {
        let db = setup();
        let rows = set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None))?;
        assert_eq!(rows.len(), 12);
        for row in &rows {
            assert_eq!(row.amount, 10_000);
        }
        let total: i64 = rows.iter().map(|r| r.amount).sum();
        assert_eq!(total, 120_000);
        Ok(())
    }

    #[test]
    fn set_annual_budget_distributes_remainder() -> Result<(), CliError> {
        let db = setup();
        let rows = set_annual_budget(db.conn(), &annual("5000", 2026, 100_000, None))?;
        assert_eq!(rows.len(), 12);
        let total: i64 = rows.iter().map(|r| r.amount).sum();
        assert_eq!(total, 100_000);
        for row in &rows[..4] {
            assert_eq!(row.amount, 8334);
        }
        for row in &rows[4..] {
            assert_eq!(row.amount, 8333);
        }
        Ok(())
    }

    #[test]
    fn list_budgets_filters() -> Result<(), CliError> {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None))?;
        set_budget(db.conn(), &budget("5100", 2026, 1, 5000, None))?;

        let all = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: None,
                month: None,
            },
        )?;
        assert_eq!(all.len(), 13);

        let rent_only = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: Some("5000"),
                month: None,
            },
        )?;
        assert_eq!(rent_only.len(), 12);

        let jan = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: None,
                month: Some(1),
            },
        )?;
        assert_eq!(jan.len(), 2);
        Ok(())
    }

    #[test]
    fn delete_budget_single_month() -> Result<(), CliError> {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None))?;
        let deleted = delete_budget(db.conn(), "acme", "5000", Currency::USD, 2026, Some(3))?;
        assert_eq!(deleted, 1);
        let remaining = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: Some("5000"),
                month: None,
            },
        )?;
        assert_eq!(remaining.len(), 11);
        Ok(())
    }

    #[test]
    fn delete_budget_all_months() -> Result<(), CliError> {
        let db = setup();
        set_annual_budget(db.conn(), &annual("5000", 2026, 120_000, None))?;
        let deleted = delete_budget(db.conn(), "acme", "5000", Currency::USD, 2026, None)?;
        assert_eq!(deleted, 12);
        let remaining = list_budgets(
            db.read(),
            &ListBudgetParams {
                company_slug: "acme",
                year: 2026,
                account_code: Some("5000"),
                month: None,
            },
        )?;
        assert!(remaining.is_empty());
        Ok(())
    }

    #[test]
    fn variance_expense_under_budget() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None))?;
        post_expense(&db, "2026-01-15", "5000", 80_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, false),
        )?;
        let rows = report.rows();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.code, "5000");
        assert_eq!(r.budget_amount, Money::usd(100_000));
        assert_eq!(r.actual_amount, Money::usd(80_000));
        assert_eq!(r.variance_amount, Money::usd(20_000));
        assert!(r.favorable);
        Ok(())
    }

    #[test]
    fn variance_retains_exact_ratio_until_rendering() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 16, None))?;
        post_expense(&db, "2026-01-15", "5000", 15);

        for include_unbudgeted in [false, true] {
            let report = compute_budget_variance(
                db.conn(),
                &variance(
                    2026,
                    Month::January,
                    Month::January,
                    None,
                    include_unbudgeted,
                ),
            )?;
            let rows = report.rows();
            let percent = rows
                .iter()
                .find(|row| row.code == "5000")
                .and_then(|row| row.variance_percent);
            assert_eq!(
                percent.map(|value| value.to_fixed(2)).as_deref(),
                Some("6.25")
            );
            assert_eq!(
                percent.map(|value| value.to_fixed(1)).as_deref(),
                Some("6.3")
            );
        }
        Ok(())
    }

    #[test]
    fn variance_expense_over_budget() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 50_000, None))?;
        post_expense(&db, "2026-01-15", "5000", 80_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, false),
        )?;
        let rows = report.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].variance_amount, Money::usd(-30_000));
        assert!(!rows[0].favorable);
        Ok(())
    }

    #[test]
    fn variance_revenue_exceeds_target() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("4000", 2026, 1, 100_000, None))?;
        post_revenue(&db, "2026-01-15", 150_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, false),
        )?;
        let rows = report.rows();
        let r = rows
            .iter()
            .find(|r| r.code == "4000")
            .ok_or_else(|| CliError::General("expected matching row".into()))?;
        assert_eq!(r.budget_amount, Money::usd(100_000));
        assert_eq!(r.actual_amount, Money::usd(150_000));
        assert_eq!(r.variance_amount, Money::usd(50_000));
        assert!(r.favorable);
        Ok(())
    }

    #[test]
    fn variance_revenue_misses_target() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("4000", 2026, 1, 200_000, None))?;
        post_revenue(&db, "2026-01-15", 100_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, false),
        )?;
        let rows = report.rows();
        let r = rows
            .iter()
            .find(|r| r.code == "4000")
            .ok_or_else(|| CliError::General("expected matching row".into()))?;
        assert_eq!(r.variance_amount, Money::usd(-100_000));
        assert!(!r.favorable);
        Ok(())
    }

    #[test]
    fn variance_multi_month_aggregation() -> Result<(), CliError> {
        let db = setup();
        for m in 1..=3 {
            set_budget(db.conn(), &budget("5000", 2026, m, 50_000, None))?;
        }
        post_expense(&db, "2026-01-15", "5000", 40_000);
        post_expense(&db, "2026-02-15", "5000", 60_000);
        post_expense(&db, "2026-03-15", "5000", 50_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::March, None, false),
        )?;
        let rows = report.rows();
        let r = rows
            .iter()
            .find(|r| r.code == "5000")
            .ok_or_else(|| CliError::General("expected matching row".into()))?;
        assert_eq!(r.budget_amount, Money::usd(150_000));
        assert_eq!(r.actual_amount, Money::usd(150_000));
        assert_eq!(r.variance_amount, Money::usd(0));
        assert!(r.favorable);
        Ok(())
    }

    #[test]
    fn variance_include_unbudgeted() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None))?;
        post_expense(&db, "2026-01-15", "5000", 80_000);
        post_expense(&db, "2026-01-20", "5100", 30_000);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, false),
        )?;
        let rows = report.rows();
        assert_eq!(rows.len(), 1);

        let report = compute_budget_variance(
            db.conn(),
            &variance(2026, Month::January, Month::January, None, true),
        )?;
        let rows = report.rows();
        assert!(rows.len() >= 2);
        let supplies = rows
            .iter()
            .find(|r| r.code == "5100")
            .ok_or_else(|| CliError::General("expected matching row".into()))?;
        assert_eq!(supplies.budget_amount, Money::usd(0));
        assert_eq!(supplies.actual_amount, Money::usd(30_000));
        assert_eq!(supplies.variance_percent, None);
        Ok(())
    }

    #[test]
    fn variance_type_filter() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, 100_000, None))?;
        set_budget(db.conn(), &budget("4000", 2026, 1, 200_000, None))?;

        let report = compute_budget_variance(
            db.conn(),
            &variance(
                2026,
                Month::January,
                Month::January,
                Some(AccountType::Expense),
                false,
            ),
        )?;
        let rows = report.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "5000");
        Ok(())
    }

    #[test]
    fn budget_months_validates_order_and_leap_month_end() -> Result<(), CliError> {
        let february = BudgetMonths::new(2024, Month::January, Month::February)?;
        let (first, last) = february.dates();
        assert_eq!(first.to_string(), "2024-01-01");
        assert_eq!(last.to_string(), "2024-02-29");
        assert!(BudgetMonths::new(2026, Month::December, Month::January).is_err());
        assert!(BudgetMonths::new(i32::MAX, Month::January, Month::January).is_err());
        Ok(())
    }

    #[test]
    fn budget_variance_rejects_total_overflow() -> Result<(), CliError> {
        let db = setup();
        set_budget(db.conn(), &budget("5000", 2026, 1, i64::MAX, None))?;
        set_budget(db.conn(), &budget("5100", 2026, 1, 1, None))?;
        let params = variance(2026, Month::January, Month::January, None, false);
        assert!(matches!(
            compute_budget_variance(db.conn(), &params),
            Err(CliError::Validation(_))
        ));
        Ok(())
    }
}
