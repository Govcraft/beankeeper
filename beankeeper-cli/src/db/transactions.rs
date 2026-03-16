use std::fmt::Write;

use rusqlite::{Connection, params};

use super::{EntryRow, TransactionRow};
use crate::error::CliError;

/// Parameters for posting a new transaction.
pub struct PostTransactionParams<'a> {
    pub company_slug: &'a str,
    pub description: &'a str,
    pub metadata: Option<&'a str>,
    pub currency: &'a str,
    pub date: &'a str,
    pub entries: &'a [(String, String, i64)],
}

/// Posts a new transaction with its entries inside a savepoint.
///
/// Each entry is a tuple of `(account_code, direction, amount)` where
/// direction is `"debit"` or `"credit"` and amount is in minor units.
///
/// Returns the new transaction ID.
///
/// # Errors
///
/// Returns `CliError::Validation` if entries is empty or a direction is
/// invalid. Returns `CliError::Sqlite` on database errors (e.g. FK violations).
pub fn post_transaction(
    conn: &Connection,
    params: &PostTransactionParams<'_>,
) -> Result<i64, CliError> {
    if params.entries.is_empty() {
        return Err(CliError::Validation(
            "transaction must have at least one entry".to_string(),
        ));
    }

    // Use a savepoint so we can roll back on error without affecting
    // any outer transaction.
    conn.execute_batch("SAVEPOINT post_txn")?;

    let result = post_transaction_inner(conn, params);

    match &result {
        Ok(_) => conn.execute_batch("RELEASE post_txn")?,
        Err(_) => conn.execute_batch("ROLLBACK TO post_txn; RELEASE post_txn")?,
    }

    result
}

fn post_transaction_inner(
    conn: &Connection,
    p: &PostTransactionParams<'_>,
) -> Result<i64, CliError> {
    conn.execute(
        "INSERT INTO transactions (company_slug, description, metadata, currency, date) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![p.company_slug, p.description, p.metadata, p.currency, p.date],
    )?;

    let txn_id = conn.last_insert_rowid();

    for (account_code, direction, amount) in p.entries {
        let dir_lower = direction.to_lowercase();
        if dir_lower != "debit" && dir_lower != "credit" {
            return Err(CliError::Validation(format!(
                "invalid direction '{direction}'; expected 'debit' or 'credit'"
            )));
        }

        conn.execute(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![txn_id, account_code, p.company_slug, dir_lower, amount],
        )?;
    }

    Ok(txn_id)
}

/// Lists transactions for a company with optional filters.
///
/// - `account_filter`: if set, only returns transactions that have an entry
///   for this account code.
/// - `from_date` / `to_date`: inclusive date range filter on
///   `transactions.date`.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
/// Parameters for listing transactions.
pub struct ListTransactionParams<'a> {
    /// Company slug to scope the query.
    pub company_slug: &'a str,
    /// Optional account code filter.
    pub account_filter: Option<&'a str>,
    /// Optional start date (inclusive).
    pub from_date: Option<&'a str>,
    /// Optional end date (inclusive).
    pub to_date: Option<&'a str>,
    /// Maximum number of rows to return.
    pub limit: i64,
    /// Number of rows to skip.
    pub offset: i64,
}

/// List transactions matching the given filters.
///
/// # Errors
///
/// Returns [`CliError`] on database query failure.
pub fn list_transactions(
    conn: &Connection,
    params: &ListTransactionParams<'_>,
) -> Result<Vec<TransactionRow>, CliError> {
    let ListTransactionParams {
        company_slug,
        account_filter,
        from_date,
        to_date,
        limit,
        offset,
    } = params;
    let mut sql = String::from(
        "SELECT DISTINCT t.id, t.company_slug, t.description, \
         t.metadata, t.currency, t.date, t.posted_at \
         FROM transactions t",
    );

    if account_filter.is_some() {
        sql.push_str(" JOIN entries e ON e.transaction_id = t.id");
    }

    sql.push_str(" WHERE t.company_slug = ?1");

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new((*company_slug).to_string()));

    if let Some(account) = account_filter {
        let _ = write!(sql, " AND e.account_code = ?{param_idx}");
        param_values.push(Box::new((*account).to_string()));
        param_idx += 1;
    }

    if let Some(from) = from_date {
        let _ = write!(sql, " AND t.date >= ?{param_idx}");
        param_values.push(Box::new((*from).to_string()));
        param_idx += 1;
    }

    if let Some(to) = to_date {
        let _ = write!(sql, " AND t.date <= ?{param_idx}");
        param_values.push(Box::new((*to).to_string()));
        param_idx += 1;
    }

    sql.push_str(" ORDER BY t.date, t.id");

    let _ = write!(sql, " LIMIT ?{param_idx}");
    param_values.push(Box::new(*limit));
    param_idx += 1;

    let _ = write!(sql, " OFFSET ?{param_idx}");
    param_values.push(Box::new(*offset));
    let _ = param_idx;

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(TransactionRow {
            id: row.get(0)?,
            company_slug: row.get(1)?,
            description: row.get(2)?,
            metadata: row.get(3)?,
            currency: row.get(4)?,
            date: row.get(5)?,
            posted_at: row.get(6)?,
        })
    })?;

    let mut transactions = Vec::new();
    for row in rows {
        transactions.push(row?);
    }
    Ok(transactions)
}

/// Fetches a single transaction and all its entries.
///
/// Returns `CliError::NotFound` if the transaction does not exist.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the transaction does not exist.
pub fn get_transaction(
    conn: &Connection,
    company_slug: &str,
    id: i64,
) -> Result<(TransactionRow, Vec<EntryRow>), CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, company_slug, description, metadata, currency, date, posted_at \
         FROM transactions \
         WHERE company_slug = ?1 AND id = ?2",
    )?;

    let mut rows = stmt.query_map(params![company_slug, id], |row| {
        Ok(TransactionRow {
            id: row.get(0)?,
            company_slug: row.get(1)?,
            description: row.get(2)?,
            metadata: row.get(3)?,
            currency: row.get(4)?,
            date: row.get(5)?,
            posted_at: row.get(6)?,
        })
    })?;

    let txn = match rows.next() {
        Some(row) => row?,
        None => {
            return Err(CliError::NotFound(format!(
                "transaction {id} not found in company '{company_slug}'"
            )));
        }
    };

    let entries = get_entries_for_transaction(conn, id)?;

    Ok((txn, entries))
}

/// Fetches all entries for a given transaction ID.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn get_entries_for_transaction(
    conn: &Connection,
    txn_id: i64,
) -> Result<Vec<EntryRow>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, transaction_id, account_code, company_slug, direction, amount \
         FROM entries \
         WHERE transaction_id = ?1 \
         ORDER BY id",
    )?;

    let rows = stmt.query_map(params![txn_id], |row| {
        Ok(EntryRow {
            id: row.get(0)?,
            transaction_id: row.get(1)?,
            account_code: row.get(2)?,
            company_slug: row.get(3)?,
            direction: row.get(4)?,
            amount: row.get(5)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::accounts::create_account;
    use crate::db::companies::create_company;
    use crate::db::connection::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(db.conn(), "acme", "Acme Corp")
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        create_account(db.conn(), "acme", "1000", "Cash", "asset")
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "4000", "Revenue", "revenue")
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn sample_entries() -> Vec<(String, String, i64)> {
        vec![
            ("1000".to_string(), "debit".to_string(), 5000),
            ("4000".to_string(), "credit".to_string(), 5000),
        ]
    }

    fn make_params<'a>(
        entries: &'a [(String, String, i64)],
        description: &'a str,
        metadata: Option<&'a str>,
        date: &'a str,
    ) -> PostTransactionParams<'a> {
        PostTransactionParams {
            company_slug: "acme",
            description,
            metadata,
            currency: "USD",
            date,
            entries,
        }
    }

    #[test]
    fn post_and_get_transaction() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test sale", None, "2024-01-15");
        let id = post_transaction(db.conn(), &p);
        assert!(id.is_ok());
        let id = id.unwrap_or_else(|e| panic!("post failed: {e}"));

        let result = get_transaction(db.conn(), "acme", id);
        assert!(result.is_ok());
        let (txn, entry_rows) = result.unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(txn.description, "Test sale");
        assert_eq!(entry_rows.len(), 2);
    }

    #[test]
    fn post_transaction_with_metadata() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test", Some(r#"{"ref":"INV-001"}"#), "2024-01-15");
        let id = post_transaction(db.conn(), &p);
        assert!(id.is_ok());
        let id = id.unwrap_or_else(|e| panic!("post failed: {e}"));
        let (txn, _) =
            get_transaction(db.conn(), "acme", id).unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(txn.metadata.as_deref(), Some(r#"{"ref":"INV-001"}"#));
    }

    #[test]
    fn post_empty_entries_is_validation_error() {
        let db = setup();
        let p = make_params(&[], "Empty", None, "2024-01-15");
        let result = post_transaction(db.conn(), &p);
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn post_invalid_direction_is_validation_error() {
        let db = setup();
        let entries = vec![("1000".to_string(), "INVALID".to_string(), 5000)];
        let p = make_params(&entries, "Bad", None, "2024-01-15");
        let result = post_transaction(db.conn(), &p);
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn get_missing_transaction_is_not_found() {
        let db = setup();
        let result = get_transaction(db.conn(), "acme", 999);
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_transactions_returns_all() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "First", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "Second", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let lp = ListTransactionParams {
            company_slug: "acme", account_filter: None,
            from_date: None, to_date: None, limit: 100, offset: 0,
        };
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_transactions_with_account_filter() {
        let db = setup();
        create_account(db.conn(), "acme", "5000", "Expenses", "expense")
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));

        let entries1 = sample_entries();
        let p1 = make_params(&entries1, "Sale", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());

        let entries2 = vec![
            ("5000".to_string(), "debit".to_string(), 1000),
            ("1000".to_string(), "credit".to_string(), 1000),
        ];
        let p2 = make_params(&entries2, "Expense", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let lp = ListTransactionParams {
            company_slug: "acme", account_filter: Some("5000"),
            from_date: None, to_date: None, limit: 100, offset: 0,
        };
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Expense");
    }

    #[test]
    fn list_transactions_with_date_range() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "Jan", None, "2024-01-15");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "Feb", None, "2024-02-15");
        assert!(post_transaction(db.conn(), &p2).is_ok());
        let p3 = make_params(&entries, "Mar", None, "2024-03-15");
        assert!(post_transaction(db.conn(), &p3).is_ok());

        let lp = ListTransactionParams {
            company_slug: "acme", account_filter: None,
            from_date: Some("2024-02-01"), to_date: Some("2024-02-28"),
            limit: 100, offset: 0,
        };
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Feb");
    }

    #[test]
    fn get_entries_for_transaction_returns_entries() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test", None, "2024-01-15");
        let id = post_transaction(db.conn(), &p)
            .unwrap_or_else(|e| panic!("post failed: {e}"));

        let entry_rows = get_entries_for_transaction(db.conn(), id).unwrap_or_default();
        assert_eq!(entry_rows.len(), 2);
        assert_eq!(entry_rows[0].direction, "debit");
        assert_eq!(entry_rows[1].direction, "credit");
    }

    #[test]
    fn savepoint_rolls_back_on_error() {
        let db = setup();
        // This should fail because account "9999" doesn't exist (FK constraint)
        let entries = vec![
            ("1000".to_string(), "debit".to_string(), 5000),
            ("9999".to_string(), "credit".to_string(), 5000),
        ];
        let p = make_params(&entries, "Bad", None, "2024-01-15");
        let result = post_transaction(db.conn(), &p);
        // The foreign key violation should cause an error
        assert!(result.is_err());

        // Verify no transaction was partially committed
        let lp = ListTransactionParams {
            company_slug: "acme", account_filter: None,
            from_date: None, to_date: None, limit: 100, offset: 0,
        };
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 0);
    }
}
