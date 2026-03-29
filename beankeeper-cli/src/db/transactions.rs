use std::fmt::Write;

use rusqlite::{Connection, params};
use serde_json;

use super::{EntryRow, TransactionRow};
use crate::error::CliError;

/// A single entry to be posted as part of a transaction.
#[derive(Debug, Clone)]
pub struct PostEntryParams {
    pub account_code: String,
    pub direction: String,
    pub amount: i64,
    pub memo: Option<String>,
    pub tax_category: Option<String>,
}

/// Conflict resolution strategy for duplicate references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStrategy {
    /// Return an error on duplicate reference (default).
    #[default]
    Error,
    /// Skip the transaction silently on duplicate reference.
    Skip,
    /// Update the existing transaction (not yet implemented).
    Upsert,
}

/// Parameters for posting a new transaction.
pub struct PostTransactionParams<'a> {
    pub company_slug: &'a str,
    pub description: &'a str,
    pub metadata: Option<&'a str>,
    pub currency: &'a str,
    pub date: &'a str,
    pub entries: &'a [PostEntryParams],
    /// If set, correlate with this existing transaction ID (intercompany linking).
    pub correlate: Option<i64>,
    /// Idempotency reference -- rejects duplicate posts with the same reference per company.
    pub reference: Option<&'a str>,
    /// How to handle duplicate references.
    pub on_conflict: ConflictStrategy,
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

/// Outcome of a transaction post operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostResult {
    /// A new transaction was successfully created.
    Created(i64),
    /// A duplicate reference was found and the transaction was skipped.
    /// Contains the ID of the existing transaction.
    Skipped(i64),
}

/// Posts a new transaction with its entries inside a savepoint.
///
/// Each entry is a tuple of `(account_code, direction, amount)` where
/// direction is `"debit"` or `"credit"` and amount is in minor units.
///
/// Returns the [`PostResult`].
///
/// # Errors
///
/// Returns `CliError::Validation` if entries is empty or a direction is
/// invalid. Returns `CliError::Sqlite` on database errors (e.g. FK violations).
pub fn post_transaction(
    conn: &Connection,
    params: &PostTransactionParams<'_>,
) -> Result<PostResult, CliError> {
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
) -> Result<PostResult, CliError> {
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
            match p.on_conflict {
                ConflictStrategy::Error => {
                    return Err(CliError::Validation(format!(
                        "transaction with reference '{ref_str}' already exists (id: {existing_id})"
                    )));
                }
                ConflictStrategy::Skip => {
                    return Ok(PostResult::Skipped(existing_id));
                }
                ConflictStrategy::Upsert => {
                    return Err(CliError::General(
                        "on-conflict=upsert not yet implemented".to_string(),
                    ));
                }
            }
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

    for entry in p.entries {
        let dir_lower = entry.direction.to_lowercase();
        if dir_lower != "debit" && dir_lower != "credit" {
            return Err(CliError::Validation(format!(
                "invalid direction '{}'; expected 'debit' or 'credit'",
                entry.direction
            )));
        }

        conn.execute(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount, memo, tax_category, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'uncleared')",
            params![
                txn_id,
                entry.account_code,
                p.company_slug,
                dir_lower,
                entry.amount,
                entry.memo,
                entry.tax_category
            ],
        )?;
    }

    // If correlating, update the partner transaction bidirectionally.
    if let Some(partner_id) = p.correlate {
        link_partner(conn, p.company_slug, txn_id, partner_id)?;
    }

    Ok(PostResult::Created(txn_id))
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

/// Parameters for listing/searching transactions.
pub struct ListTransactionParams<'a> {
    /// Company slug to scope the query.
    pub company_slug: &'a str,
    /// Optional account code filter (matches entries).
    pub account_filter: Option<&'a str>,
    /// Optional start date (inclusive).
    pub from_date: Option<&'a str>,
    /// Optional end date (inclusive).
    pub to_date: Option<&'a str>,
    /// Maximum number of rows to return.
    pub limit: i64,
    /// Number of rows to skip.
    pub offset: i64,
    /// Substring search on transaction description (case-insensitive).
    pub description_like: Option<&'a str>,
    /// Minimum entry amount in minor units (exclusive).
    pub amount_gt: Option<i64>,
    /// Maximum entry amount in minor units (exclusive).
    pub amount_lt: Option<i64>,
    /// Exact entry amount in minor units.
    pub amount_eq: Option<i64>,
    /// Exact match on transaction currency code.
    pub currency_filter: Option<&'a str>,
    /// Exact match on transaction reference (idempotency key).
    pub reference_filter: Option<&'a str>,
    /// Substring search on transaction metadata (case-insensitive).
    pub metadata_like: Option<&'a str>,
    /// Exact match on entry tax category.
    pub tax_category_filter: Option<&'a str>,
    /// Filter entries by direction ("debit" or "credit").
    pub direction_filter: Option<&'a str>,
}

impl<'a> ListTransactionParams<'a> {
    /// Creates params with only the required company slug and sensible defaults.
    #[must_use]
    pub fn for_company(company_slug: &'a str) -> Self {
        Self {
            company_slug,
            account_filter: None,
            from_date: None,
            to_date: None,
            limit: 50,
            offset: 0,
            description_like: None,
            amount_gt: None,
            amount_lt: None,
            amount_eq: None,
            currency_filter: None,
            reference_filter: None,
            metadata_like: None,
            tax_category_filter: None,
            direction_filter: None,
        }
    }

    /// Returns `true` if any filter requires joining the entries table.
    fn needs_entries_join(&self) -> bool {
        self.account_filter.is_some()
            || self.amount_gt.is_some()
            || self.amount_lt.is_some()
            || self.amount_eq.is_some()
            || self.tax_category_filter.is_some()
            || self.direction_filter.is_some()
    }
}

/// Builds the shared FROM/JOIN/WHERE clause for transaction queries.
///
/// Returns `(sql, param_values, next_param_idx)` without ORDER BY, LIMIT, or OFFSET.
fn build_list_query(
    params: &ListTransactionParams<'_>,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>, u32) {
    let mut sql = String::from("FROM transactions t");

    if params.needs_entries_join() {
        sql.push_str(" JOIN entries e ON e.transaction_id = t.id");
    }

    sql.push_str(" WHERE t.company_slug = ?1");

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(params.company_slug.to_string()));

    // Entry-level filters
    if let Some(account) = params.account_filter {
        let _ = write!(sql, " AND e.account_code = ?{param_idx}");
        param_values.push(Box::new(account.to_string()));
        param_idx += 1;
    }

    if let Some(amount) = params.amount_gt {
        let _ = write!(sql, " AND e.amount > ?{param_idx}");
        param_values.push(Box::new(amount));
        param_idx += 1;
    }

    if let Some(amount) = params.amount_lt {
        let _ = write!(sql, " AND e.amount < ?{param_idx}");
        param_values.push(Box::new(amount));
        param_idx += 1;
    }

    if let Some(amount) = params.amount_eq {
        let _ = write!(sql, " AND e.amount = ?{param_idx}");
        param_values.push(Box::new(amount));
        param_idx += 1;
    }

    if let Some(tax_cat) = params.tax_category_filter {
        let _ = write!(sql, " AND e.tax_category = ?{param_idx}");
        param_values.push(Box::new(tax_cat.to_string()));
        param_idx += 1;
    }

    if let Some(direction) = params.direction_filter {
        let _ = write!(sql, " AND e.direction = ?{param_idx}");
        param_values.push(Box::new(direction.to_string()));
        param_idx += 1;
    }

    // Transaction-level filters
    if let Some(from) = params.from_date {
        let _ = write!(sql, " AND t.date >= ?{param_idx}");
        param_values.push(Box::new(from.to_string()));
        param_idx += 1;
    }

    if let Some(to) = params.to_date {
        let _ = write!(sql, " AND t.date <= ?{param_idx}");
        param_values.push(Box::new(to.to_string()));
        param_idx += 1;
    }

    if let Some(desc) = params.description_like {
        let _ = write!(sql, " AND t.description LIKE '%' || ?{param_idx} || '%'");
        param_values.push(Box::new(desc.to_string()));
        param_idx += 1;
    }

    if let Some(currency) = params.currency_filter {
        let _ = write!(sql, " AND t.currency = ?{param_idx}");
        param_values.push(Box::new(currency.to_string()));
        param_idx += 1;
    }

    if let Some(reference) = params.reference_filter {
        let _ = write!(sql, " AND t.reference = ?{param_idx}");
        param_values.push(Box::new(reference.to_string()));
        param_idx += 1;
    }

    if let Some(meta) = params.metadata_like {
        let _ = write!(sql, " AND t.metadata LIKE '%' || ?{param_idx} || '%'");
        param_values.push(Box::new(meta.to_string()));
        param_idx += 1;
    }

    (sql, param_values, param_idx)
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
    let (where_clause, mut param_values, mut param_idx) = build_list_query(params);

    let mut sql = format!(
        "SELECT DISTINCT t.id, t.company_slug, t.description, \
         t.metadata, t.currency, t.date, t.posted_at, t.reference \
         {where_clause}"
    );

    sql.push_str(" ORDER BY t.date, t.id");

    let _ = write!(sql, " LIMIT ?{param_idx}");
    param_values.push(Box::new(params.limit));
    param_idx += 1;

    let _ = write!(sql, " OFFSET ?{param_idx}");
    param_values.push(Box::new(params.offset));

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

/// Count transactions matching the given filters (ignores limit/offset).
///
/// # Errors
///
/// Returns [`CliError`] on database query failure.
pub fn count_transactions(
    conn: &Connection,
    params: &ListTransactionParams<'_>,
) -> Result<i64, CliError> {
    let (where_clause, param_values, _) = build_list_query(params);

    let sql = format!("SELECT COUNT(DISTINCT t.id) {where_clause}");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let count: i64 = conn.query_row(&sql, params_refs.as_slice(), |row| row.get(0))?;
    Ok(count)
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
        "SELECT id, transaction_id, account_code, company_slug, direction, amount, memo, \
         tax_category, status \
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
            tax_category: row.get(7)?,
            status: row.get(8)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// Updates the clearance status of a specific entry.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the entry does not exist or doesn't belong to the given transaction/company.
/// Returns `CliError::Sqlite` on database errors.
pub fn update_entry_status(
    conn: &Connection,
    company_slug: &str,
    txn_id: i64,
    entry_id: i64,
    status: &str,
) -> Result<(), CliError> {
    let rows_affected = conn.execute(
        "UPDATE entries \
         SET status = ?1 \
         WHERE id = ?2 AND transaction_id = ?3 AND company_slug = ?4",
        params![status, entry_id, txn_id, company_slug],
    )?;

    if rows_affected == 0 {
        return Err(CliError::NotFound(format!(
            "entry {entry_id} not found in transaction {txn_id} for company '{company_slug}'"
        )));
    }

    Ok(())
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
        create_account(db.conn(), "acme", "1000", "Cash", "asset", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        create_account(db.conn(), "acme", "4000", "Revenue", "revenue", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn sample_entries() -> Vec<PostEntryParams> {
        vec![
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: "debit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".to_string(),
                direction: "credit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
        ]
    }

    fn make_params<'a>(
        entries: &'a [PostEntryParams],
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
            on_conflict: ConflictStrategy::Error,
        }
    }

    #[test]
    fn post_and_get_transaction() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test sale", None, "2024-01-15");
        let id = post_transaction(db.conn(), &p);
        assert!(id.is_ok());
        let PostResult::Created(id) = id.unwrap_or_else(|e| panic!("post failed: {e}")) else {
            panic!("expected Created");
        };

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
        let PostResult::Created(id) = id.unwrap_or_else(|e| panic!("post failed: {e}")) else {
            panic!("expected Created");
        };
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
        let entries = vec![PostEntryParams {
            account_code: "1000".to_string(),
            direction: "INVALID".to_string(),
            amount: 5000,
            memo: None,
            tax_category: None,
        }];
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

        let mut lp = ListTransactionParams::for_company("acme");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_transactions_with_account_filter() {
        let db = setup();
        create_account(db.conn(), "acme", "5000", "Expenses", "expense", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));

        let entries1 = sample_entries();
        let p1 = make_params(&entries1, "Sale", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());

        let entries2 = vec![
            PostEntryParams {
                account_code: "5000".to_string(),
                direction: "debit".to_string(),
                amount: 1000,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: "credit".to_string(),
                amount: 1000,
                memo: None,
                tax_category: None,
            },
        ];
        let p2 = make_params(&entries2, "Expense", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.account_filter = Some("5000");
        lp.limit = 100;
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

        let mut lp = ListTransactionParams::for_company("acme");
        lp.from_date = Some("2024-02-01");
        lp.to_date = Some("2024-02-28");
        lp.limit = 100;
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
        let PostResult::Created(id) = id else {
            panic!("expected Created");
        };

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
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: "debit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "9999".to_string(),
                direction: "credit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
        ];
        let p = make_params(&entries, "Bad", None, "2024-01-15");
        let result = post_transaction(db.conn(), &p);
        // The foreign key violation should cause an error
        assert!(result.is_err());

        // Verify no transaction was partially committed
        let mut lp = ListTransactionParams::for_company("acme");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn list_transactions_with_description_filter() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "AWS monthly bill", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "Office supplies", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.description_like = Some("AWS");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "AWS monthly bill");
    }

    #[test]
    fn list_transactions_with_amount_gt() {
        let db = setup();
        let small_entries = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount: 1000,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount: 1000,
                memo: None,
                tax_category: None,
            },
        ];
        let big_entries = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount: 50000,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount: 50000,
                memo: None,
                tax_category: None,
            },
        ];
        let p1 = make_params(&small_entries, "Small", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&big_entries, "Big", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.amount_gt = Some(10000);
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Big");
    }

    #[test]
    fn list_transactions_with_amount_eq() {
        let db = setup();
        let entries = sample_entries(); // amount 5000
        let p1 = make_params(&entries, "Exact", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.amount_eq = Some(5000);
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);

        let mut lp2 = ListTransactionParams::for_company("acme");
        lp2.amount_eq = Some(9999);
        lp2.limit = 100;
        let list2 = list_transactions(db.conn(), &lp2).unwrap_or_default();
        assert_eq!(list2.len(), 0);
    }

    #[test]
    fn list_transactions_with_reference_filter() {
        let db = setup();
        let entries = sample_entries();
        let mut p1 = make_params(&entries, "With ref", None, "2024-01-01");
        p1.reference = Some("INV-001");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "No ref", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.reference_filter = Some("INV-001");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "With ref");
    }

    #[test]
    fn list_transactions_with_metadata_filter() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(
            &entries,
            "Tagged",
            Some(r#"{"vendor":"AWS"}"#),
            "2024-01-01",
        );
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "Untagged", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.metadata_like = Some("AWS");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Tagged");
    }

    #[test]
    fn list_transactions_with_direction_filter() {
        let db = setup();
        create_account(db.conn(), "acme", "5000", "Expenses", "expense", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));

        let entries = sample_entries();
        let p1 = make_params(&entries, "Sale", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());

        // Filter for transactions where account 1000 has a debit entry
        let mut lp = ListTransactionParams::for_company("acme");
        lp.account_filter = Some("1000");
        lp.direction_filter = Some("debit");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);

        // Filter for transactions where account 1000 has a credit entry (none)
        let mut lp2 = ListTransactionParams::for_company("acme");
        lp2.account_filter = Some("1000");
        lp2.direction_filter = Some("credit");
        lp2.limit = 100;
        let list2 = list_transactions(db.conn(), &lp2).unwrap_or_default();
        assert_eq!(list2.len(), 0);
    }

    #[test]
    fn count_transactions_returns_count() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "First", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "Second", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());
        let p3 = make_params(&entries, "Third", None, "2024-01-03");
        assert!(post_transaction(db.conn(), &p3).is_ok());

        let lp = ListTransactionParams::for_company("acme");
        let count = count_transactions(db.conn(), &lp).unwrap_or(0);
        assert_eq!(count, 3);

        let mut lp2 = ListTransactionParams::for_company("acme");
        lp2.from_date = Some("2024-01-02");
        let count2 = count_transactions(db.conn(), &lp2).unwrap_or(0);
        assert_eq!(count2, 2);
    }

    #[test]
    fn list_transactions_combined_filters() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "AWS January", None, "2024-01-15");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries, "AWS February", None, "2024-02-15");
        assert!(post_transaction(db.conn(), &p2).is_ok());
        let p3 = make_params(&entries, "Office supplies", None, "2024-01-20");
        assert!(post_transaction(db.conn(), &p3).is_ok());

        // Combine description + date range
        let mut lp = ListTransactionParams::for_company("acme");
        lp.description_like = Some("AWS");
        lp.from_date = Some("2024-02-01");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "AWS February");
    }

    #[test]
    fn list_transactions_with_tax_category_filter() {
        let db = setup();
        let entries_with_tax = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount: 5000,
                memo: None,
                tax_category: Some("sched-c:24b".into()),
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
        ];
        let entries_no_tax = sample_entries();

        let p1 = make_params(&entries_with_tax, "Taxed", None, "2024-01-01");
        assert!(post_transaction(db.conn(), &p1).is_ok());
        let p2 = make_params(&entries_no_tax, "Untaxed", None, "2024-01-02");
        assert!(post_transaction(db.conn(), &p2).is_ok());

        let mut lp = ListTransactionParams::for_company("acme");
        lp.tax_category_filter = Some("sched-c:24b");
        lp.limit = 100;
        let list = list_transactions(db.conn(), &lp).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Taxed");
    }

    #[test]
    fn post_transaction_skip_conflict() {
        let db = setup();
        let entries = sample_entries();
        let mut p1 = make_params(&entries, "First", None, "2024-01-01");
        p1.reference = Some("REF-1");
        let res1 = post_transaction(db.conn(), &p1).unwrap();
        let PostResult::Created(id1) = res1 else {
            panic!("expected Created");
        };

        let mut p2 = make_params(&entries, "Duplicate", None, "2024-01-01");
        p2.reference = Some("REF-1");
        p2.on_conflict = ConflictStrategy::Skip;
        let res2 = post_transaction(db.conn(), &p2).unwrap();
        assert_eq!(res2, PostResult::Skipped(id1));

        // Verify only one transaction exists
        let lp = ListTransactionParams::for_company("acme");
        let count = count_transactions(db.conn(), &lp).unwrap();
        assert_eq!(count, 1);
    }
}
