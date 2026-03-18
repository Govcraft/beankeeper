use std::fmt::Write;

use rusqlite::{Connection, params};
use serde_json;

use super::{EntryRow, TransactionRow};
use crate::error::CliError;

/// Parameters for posting a new transaction.
pub struct PostTransactionParams<'a> {
    pub company_slug: &'a str,
    pub description: &'a str,
    pub metadata: Option<&'a str>,
    pub currency: &'a str,
    pub date: &'a str,
    pub entries: &'a [(String, String, i64, Option<String>)],
    /// If set, correlate with this existing transaction ID (intercompany linking).
    pub correlate: Option<i64>,
    /// Idempotency reference -- rejects duplicate posts with the same reference per company.
    pub reference: Option<&'a str>,
}

/// An orphaned intercompany correlation found by reconcile.
#[derive(Debug, Clone)]
pub struct OrphanedCorrelation {
    /// The transaction that has a correlate reference.
    pub transaction_id: i64,
    /// The company this transaction belongs to.
    pub company_slug: String,
    /// Transaction description.
    pub description: String,
    /// Transaction date.
    pub date: String,
    /// The partner transaction ID referenced in metadata.
    pub partner_id: i64,
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
    // Check for duplicate reference before inserting.
    if let Some(ref_str) = p.reference {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
                params![p.company_slug, ref_str],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing {
            return Err(CliError::Validation(format!(
                "transaction with reference '{ref_str}' already exists (id: {existing_id})"
            )));
        }
    }

    // Build the metadata string, merging --correlate and --metadata if both present.
    let effective_metadata = build_metadata(p.metadata, p.correlate);

    conn.execute(
        "INSERT INTO transactions (company_slug, description, metadata, currency, date, reference) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            p.company_slug,
            p.description,
            effective_metadata,
            p.currency,
            p.date,
            p.reference
        ],
    )?;

    let txn_id = conn.last_insert_rowid();

    for (account_code, direction, amount, memo) in p.entries {
        let dir_lower = direction.to_lowercase();
        if dir_lower != "debit" && dir_lower != "credit" {
            return Err(CliError::Validation(format!(
                "invalid direction '{direction}'; expected 'debit' or 'credit'"
            )));
        }

        conn.execute(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount, memo) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                txn_id,
                account_code,
                p.company_slug,
                dir_lower,
                amount,
                memo
            ],
        )?;
    }

    // If correlating, update the partner transaction bidirectionally.
    if let Some(partner_id) = p.correlate {
        link_partner(conn, p.company_slug, txn_id, partner_id)?;
    }

    Ok(txn_id)
}

/// Build effective metadata JSON from optional user metadata and correlate ID.
fn build_metadata(user_metadata: Option<&str>, correlate: Option<i64>) -> Option<String> {
    match (user_metadata, correlate) {
        (None, None) => None,
        (Some(m), None) => Some(m.to_string()),
        (None, Some(cid)) => Some(format!(r#"{{"correlate":{cid}}}"#)),
        (Some(m), Some(cid)) => Some(format!(
            r#"{{"correlate":{cid},"ref":{}}}"#,
            serde_json::json!(m)
        )),
    }
}

/// Validate and link a partner transaction bidirectionally.
///
/// - Verifies the partner exists and belongs to a different company.
/// - Verifies the partner is not already correlated.
/// - Updates the partner's metadata to include `{"correlate": new_txn_id}`.
fn link_partner(
    conn: &Connection,
    new_company: &str,
    new_txn_id: i64,
    partner_id: i64,
) -> Result<(), CliError> {
    // Look up the partner transaction (any company).
    let partner: (String, Option<String>) = conn
        .query_row(
            "SELECT company_slug, metadata FROM transactions WHERE id = ?1",
            params![partner_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| CliError::NotFound(format!("transaction #{partner_id} not found")))?;

    let (partner_company, partner_metadata) = partner;

    // Must belong to a different company.
    if partner_company == new_company {
        return Err(CliError::Validation(format!(
            "cannot correlate with transaction #{partner_id}: it belongs to the same company '{new_company}'"
        )));
    }

    // Must not already be correlated.
    if let Some(ref meta) = partner_metadata {
        if meta.contains("\"correlate\"") {
            return Err(CliError::Validation(format!(
                "transaction #{partner_id} is already correlated"
            )));
        }
    }

    // Build the updated metadata for the partner.
    let updated_partner_meta =
        merge_correlate_into_metadata(partner_metadata.as_deref(), new_txn_id);

    conn.execute(
        "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
        params![updated_partner_meta, partner_id],
    )?;

    Ok(())
}

/// Merge a `correlate` key into existing metadata.
///
/// - No existing metadata → `{"correlate": id}`
/// - Existing JSON object → insert `correlate` key
/// - Existing plain string → `{"correlate": id, "ref": "old_string"}`
fn merge_correlate_into_metadata(existing: Option<&str>, correlate_id: i64) -> String {
    match existing {
        None => format!(r#"{{"correlate":{correlate_id}}}"#),
        Some(s) if s.starts_with('{') => {
            // Insert "correlate":N at the beginning of the JSON object.
            let rest = &s[1..]; // skip opening brace
            if rest.trim_start().starts_with('}') {
                // Empty object
                format!(r#"{{"correlate":{correlate_id}}}"#)
            } else {
                format!(r#"{{"correlate":{correlate_id},{rest}"#)
            }
        }
        Some(s) => {
            // Plain string — wrap into JSON object with ref key.
            format!(
                r#"{{"correlate":{correlate_id},"ref":{}}}"#,
                serde_json::json!(s)
            )
        }
    }
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
         t.metadata, t.currency, t.date, t.posted_at, t.reference \
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
            reference: row.get(7)?,
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
        "SELECT id, company_slug, description, metadata, currency, date, posted_at, reference \
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
            reference: row.get(7)?,
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
        "SELECT id, transaction_id, account_code, company_slug, direction, amount, memo \
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
            memo: row.get(6)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// Find orphaned intercompany correlations.
///
/// Returns transactions that reference a partner via `json_extract(metadata, '$.correlate')`
/// where the partner either doesn't exist or doesn't reference back.
///
/// # Errors
///
/// Returns [`CliError`] on database query failure.
pub fn find_orphaned_correlations(conn: &Connection) -> Result<Vec<OrphanedCorrelation>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.company_slug, t.description, t.date, \
                CAST(json_extract(t.metadata, '$.correlate') AS INTEGER) AS partner_id \
         FROM transactions t \
         WHERE json_extract(t.metadata, '$.correlate') IS NOT NULL \
           AND NOT EXISTS ( \
               SELECT 1 FROM transactions t2 \
               WHERE t2.id = CAST(json_extract(t.metadata, '$.correlate') AS INTEGER) \
                 AND CAST(json_extract(t2.metadata, '$.correlate') AS INTEGER) = t.id \
           ) \
         ORDER BY t.id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(OrphanedCorrelation {
            transaction_id: row.get(0)?,
            company_slug: row.get(1)?,
            description: row.get(2)?,
            date: row.get(3)?,
            partner_id: row.get(4)?,
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
    use crate::db::accounts::create_account;
    use crate::db::companies::create_company;
    use crate::db::connection::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(db.conn(), "acme", "Acme Corp", None)
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        create_account(db.conn(), "acme", "1000", "Cash", "asset")
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "4000", "Revenue", "revenue")
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn sample_entries() -> Vec<(String, String, i64, Option<String>)> {
        vec![
            ("1000".to_string(), "debit".to_string(), 5000, None),
            ("4000".to_string(), "credit".to_string(), 5000, None),
        ]
    }

    fn make_params<'a>(
        entries: &'a [(String, String, i64, Option<String>)],
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
            correlate: None,
            reference: None,
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
        let entries = vec![("1000".to_string(), "INVALID".to_string(), 5000, None)];
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
            company_slug: "acme",
            account_filter: None,
            from_date: None,
            to_date: None,
            limit: 100,
            offset: 0,
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
            ("5000".to_string(), "debit".to_string(), 1000, None),
            ("1000".to_string(), "credit".to_string(), 1000, None),
        ];
        let p2 = make_params(&entries2, "Expense", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let lp = ListTransactionParams {
            company_slug: "acme",
            account_filter: Some("5000"),
            from_date: None,
            to_date: None,
            limit: 100,
            offset: 0,
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
            company_slug: "acme",
            account_filter: None,
            from_date: Some("2024-02-01"),
            to_date: Some("2024-02-28"),
            limit: 100,
            offset: 0,
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
        let id = post_transaction(db.conn(), &p).unwrap_or_else(|e| panic!("post failed: {e}"));

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
            ("1000".to_string(), "debit".to_string(), 5000, None),
            ("9999".to_string(), "credit".to_string(), 5000, None),
        ];
        let p = make_params(&entries, "Bad", None, "2024-01-15");
        let result = post_transaction(db.conn(), &p);
        // The foreign key violation should cause an error
        assert!(result.is_err());

        // Verify no transaction was partially committed
        let lp = ListTransactionParams {
            company_slug: "acme",
            account_filter: None,
            from_date: None,
            to_date: None,
            limit: 100,
            offset: 0,
        };
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 0);
    }
}
