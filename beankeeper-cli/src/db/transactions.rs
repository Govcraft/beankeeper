use std::fmt::Write;

use beankeeper::core::{JournalEntry, Transaction, TransactionError};
use beankeeper::types::{Currency, DebitOrCredit, Entry, Money};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};

use super::amounts::PositiveMinorUnits;
use super::audit::{self, Actor, savepoint::AuditedTx};
use super::companies::{AllCompanies, Company, CompanySlug};
use super::connection::ReadConn;
use super::metadata;
use super::metadata_filter;
use super::{
    EntryRow, TransactionRow,
    ids::{PartnerTransactionId, TransactionId},
};
use crate::error::CliError;
use crate::identity::{Granted, Post};

/// Check source identity before applying mutable import categorization rules.
pub(crate) fn reference_exists(
    conn: ReadConn<'_>,
    company: &crate::db::companies::CompanySlug,
    reference: &str,
) -> Result<bool, CliError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM transactions WHERE company_slug = ?1 AND reference = ?2)",
        params![company, reference],
        |row| row.get(0),
    )?)
}

/// A single entry to be posted as part of a transaction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PostEntryParams {
    pub account_code: String,
    #[serde(deserialize_with = "deserialize_post_direction")]
    pub direction: DebitOrCredit,
    pub amount: PositiveMinorUnits,
    pub memo: Option<String>,
    pub tax_category: Option<String>,
}

// Historical proposals created through the Rust API may have mixed-case directions.
fn deserialize_post_direction<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<DebitOrCredit, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    DebitOrCredit::from_code(&value)
        .or_else(|_| DebitOrCredit::from_code(&value.to_ascii_lowercase()))
        .map_err(serde::de::Error::custom)
}

/// Conflict resolution strategy for duplicate references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ConflictStrategy {
    /// Return an error on duplicate reference (default).
    #[default]
    Error,
    /// Skip the transaction silently on duplicate reference.
    Skip,
}

/// Parameters for posting a new transaction.
pub struct PostTransactionParams<'a> {
    pub company_slug: &'a CompanySlug,
    pub description: &'a str,
    pub metadata: Option<&'a str>,
    pub currency: Currency,
    pub date: NaiveDate,
    pub entries: &'a [PostEntryParams],
    /// If set, correlate with this existing transaction ID (intercompany linking).
    pub correlate: Option<PartnerTransactionId>,
    /// Idempotency reference -- rejects duplicate posts with the same reference per company.
    pub reference: Option<&'a str>,
    /// How to handle duplicate references.
    pub on_conflict: ConflictStrategy,
    /// The principal recorded on creation and any audited side effects.
    pub actor: &'a Actor,
}

/// An orphaned intercompany correlation found by reconcile.
#[derive(Debug, Clone)]
pub struct OrphanedCorrelation {
    /// The transaction that has a correlate reference.
    pub transaction_id: TransactionId,
    /// The company this transaction belongs to.
    pub company_slug: crate::db::companies::CompanySlug,
    /// Transaction description.
    pub description: String,
    /// Transaction date.
    pub date: NaiveDate,
    /// The partner transaction ID referenced in metadata.
    pub partner_id: i64,
}

/// Outcome of a transaction post operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostResult {
    /// A new transaction was successfully created.
    Created(TransactionId),
    /// A duplicate reference was found and the transaction was skipped.
    /// Contains the ID of the existing transaction.
    Skipped(TransactionId),
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
/// Returns `CliError::Validation` for invalid dates, currency, accounts or
/// entries. Returns `CliError::Sqlite` for storage errors.
///
/// Posting requires authority for this operation and ledger.
/// ```compile_fail
/// use beankeeper_cli::db::transactions::{PostTransactionParams, post_transaction};
/// use beankeeper_cli::identity::{Granted, Reverse};
/// fn wrong_grant(grant: Granted<'_, Reverse>, params: &PostTransactionParams<'_>) {
///     let _ = post_transaction(grant, params);
/// }
/// ```
pub fn post_transaction(
    grant: Granted<'_, Post>,
    params: &PostTransactionParams<'_>,
) -> Result<PostResult, CliError> {
    post_from_source(grant, params, PostingSource::Storage)
}

#[cfg(test)]
pub(crate) fn post_fixture(
    db: &super::Db,
    params: &PostTransactionParams<'_>,
) -> Result<PostResult, CliError> {
    Granted::<Post>::with_db(db, params.actor, |grant| post_transaction(grant, params))
}

pub(super) fn post_approved_transaction(
    approval: super::proposals::ApprovedPosting<'_>,
) -> Result<PostResult, CliError> {
    post_with_source(
        approval.transaction(),
        &Posting::Approved(approval),
        PostingSource::Proposal,
    )
}

pub(in crate::db) fn post_approved_statement(
    approval: audit::setup::statement_batches::ApprovedStatementPosting<'_>,
) -> Result<PostResult, CliError> {
    post_with_source(
        approval.transaction(),
        &Posting::ApprovedStatement(approval),
        PostingSource::Proposal,
    )
}

/// The origin of the request determines its validation order and diagnostics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostingSource {
    Storage,
    Command,
    Import,
    Proposal,
}

#[derive(Clone, Copy)]
enum PostingAccounts {
    Active,
    Reversal,
}

pub(crate) fn post_from_source(
    grant: Granted<'_, Post>,
    params: &PostTransactionParams<'_>,
    source: PostingSource,
) -> Result<PostResult, CliError> {
    grant.mutate("sp_authorized_post", |tx, actor| {
        let params = PostTransactionParams { actor, ..*params };
        post_direct(tx, &params, source)
    })
}

pub(super) fn post_direct(
    tx: &AuditedTx<'_>,
    params: &PostTransactionParams<'_>,
    source: PostingSource,
) -> Result<PostResult, CliError> {
    if source == PostingSource::Storage && params.entries.is_empty() {
        return Err(CliError::Validation(
            "transaction must have at least one entry".to_string(),
        ));
    }
    post_with_source(tx, &Posting::Direct(params), source)
}

pub(super) fn post_reversal(reversal: audit::ReversalPosting<'_>) -> Result<PostResult, CliError> {
    post_with_source(
        reversal.transaction(),
        &Posting::Reversal(reversal),
        PostingSource::Storage,
    )
}

enum Posting<'a> {
    Direct(&'a PostTransactionParams<'a>),
    Approved(super::proposals::ApprovedPosting<'a>),
    ApprovedStatement(audit::setup::statement_batches::ApprovedStatementPosting<'a>),
    Reversal(audit::ReversalPosting<'a>),
}

fn post_with_source(
    tx: &AuditedTx<'_>,
    posting: &Posting<'_>,
    source: PostingSource,
) -> Result<PostResult, CliError> {
    let reader = tx.read();
    let approved;
    let p = match posting {
        Posting::Direct(params) => *params,
        Posting::Reversal(reversal) => reversal.params(),
        Posting::Approved(approval) => {
            approved = approval.params();
            &approved
        }
        Posting::ApprovedStatement(approval) => {
            approved = approval.params();
            &approved
        }
    };
    // Command and import diagnostics precede idempotency. Raw API retries
    // look up the existing reference before validating entries.
    let prepared = (source != PostingSource::Storage)
        .then(|| build_journal(reader, p, source))
        .transpose()?;
    // Check for duplicate reference before inserting.
    if let Some(ref_str) = p.reference {
        let existing: Option<TransactionId> = reader
            .query_row(
                "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
                params![p.company_slug, ref_str],
                |row| row.get(0),
            )
            .optional()?;

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
            }
        }
    }

    if !matches!(posting, Posting::Approved(_))
        && let Some(reference) = p.reference
    {
        super::proposals::check_reference(reader, p.company_slug, reference)?;
    }
    let owner_undo = matches!(posting, Posting::Reversal(reversal) if reversal.is_owner_undo());
    if !matches!(
        posting,
        Posting::Approved(_) | Posting::ApprovedStatement(_)
    ) && !owner_undo
    {
        super::spend_limits::check(reader, p)?;
    }

    let journal = if let Some(journal) = prepared {
        if journal.currency() != p.currency {
            return Err(CliError::Validation(
                "posting currency does not match the journal".into(),
            ));
        }
        journal
    } else {
        let accounts = match posting {
            // Only the sealed mirror of an existing posting can use archived accounts.
            Posting::Reversal(_) => PostingAccounts::Reversal,
            _ => PostingAccounts::Active,
        };
        build_journal_with_accounts(reader, p, source, accounts)?
    };
    let period =
        super::periods::check_posting(tx, p.company_slug, journal.date()).map_err(|error| {
            if owner_undo {
                error
            } else {
                super::periods::ClosedPostingPeriod::public_error(error)
            }
        })?;
    let validated = ValidatedPosting {
        journal,
        period,
        request: p,
    };
    let metadata = match posting {
        Posting::Direct(_) | Posting::ApprovedStatement(_) => {
            metadata::UserAnnotations::parse(p.metadata)?
                .map(metadata::UserAnnotations::into_normalized)
        }
        Posting::Approved(_) => p.metadata.and_then(metadata::normalize),
        Posting::Reversal(reversal) => Some(metadata::reversal_link(reversal.original_id())?),
    };
    let metadata = match p.actor.transaction_source() {
        Some(source) => Some(metadata::with_transaction_source(metadata, source)?),
        None => metadata,
    };
    let effective_metadata = match p.correlate {
        None => metadata.map(metadata::NormalizedMetadata::into_string),
        Some(partner_id) => Some(metadata::with_correlation(metadata, partner_id)?),
    };
    audit::persist_posting(validated, effective_metadata.as_deref())
}

/// The only input to the posting insert. The validated journal and open
/// period both come from this request and connection.
pub(super) struct ValidatedPosting<'a> {
    journal: Transaction,
    period: super::periods::OpenPeriod<'a>,
    request: &'a PostTransactionParams<'a>,
}

impl<'a> ValidatedPosting<'a> {
    pub(super) fn into_parts(
        self,
    ) -> (
        Transaction,
        super::periods::OpenPeriod<'a>,
        &'a PostTransactionParams<'a>,
    ) {
        (self.journal, self.period, self.request)
    }
}

/// Build a sealed journal for posting or for a preview without storage effects.
pub(crate) fn build_journal(
    conn: ReadConn<'_>,
    request: &PostTransactionParams<'_>,
    source: PostingSource,
) -> Result<Transaction, CliError> {
    build_journal_with_accounts(conn, request, source, PostingAccounts::Active)
}

fn build_journal_with_accounts(
    conn: ReadConn<'_>,
    request: &PostTransactionParams<'_>,
    source: PostingSource,
    accounts: PostingAccounts,
) -> Result<Transaction, CliError> {
    let currency = request.currency;
    let company = Company::bind(conn, request.company_slug.clone())?;
    let mut journal = JournalEntry::new(request.date, request.description, currency);
    for entry in request.entries {
        let account = match accounts {
            PostingAccounts::Active => {
                super::accounts::get_active_account(&company, &entry.account_code)
            }
            PostingAccounts::Reversal => {
                super::accounts::get_account(&company, &entry.account_code)
            }
        }
        .map_err(|error| match (source, error) {
            (PostingSource::Storage, CliError::NotFound(message)) => CliError::Validation(message),
            (_, error) => error,
        })?;
        let default_tax = if matches!(source, PostingSource::Command | PostingSource::Import) {
            account.default_tax_category.as_deref()
        } else {
            None
        };
        let tax = entry.tax_category.as_deref().or(default_tax);
        let line = posting_entry(
            entry,
            super::accounts::row_to_account(&account)?,
            currency,
            tax,
            source,
        )?;
        journal = journal
            .entry(line)
            .map_err(|error| entry_error(error, source))?;
    }
    journal.post().map_err(|error| match source {
        PostingSource::Storage => invalid(error),
        PostingSource::Command => command_posting_error(error, currency),
        PostingSource::Import | PostingSource::Proposal => error.into(),
    })
}

fn entry_error(error: beankeeper::types::EntryError, source: PostingSource) -> CliError {
    if source == PostingSource::Storage {
        invalid(error)
    } else {
        error.into()
    }
}

pub(crate) fn command_posting_error(error: TransactionError, currency: Currency) -> CliError {
    let TransactionError::Unbalanced {
        total_debits,
        total_credits,
    } = error
    else {
        return error.into();
    };
    let difference = Money::new(total_debits, currency)
        .checked_sub(Money::new(total_credits, currency))
        .and_then(Money::checked_abs);
    let difference = match difference {
        Ok(value) => value.amount().format_decimal(currency.minor_units()),
        Err(error) => return invalid(error),
    };
    let debits = total_debits.format_decimal(currency.minor_units());
    let credits = total_credits.format_decimal(currency.minor_units());
    CliError::Validation(format!(
        "transaction is unbalanced: debits={debits} {currency}, credits={credits} {currency} (difference {difference} {currency})"
    ))
}

fn invalid(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}

fn posting_entry(
    raw: &PostEntryParams,
    account: beankeeper::types::Account,
    currency: Currency,
    tax: Option<&str>,
    source: PostingSource,
) -> Result<Entry, CliError> {
    let money = Money::from_minor(i128::from(raw.amount), currency);
    let entry = match &raw.memo {
        Some(memo) => Entry::with_memo(account, money, raw.direction, memo),
        None => Entry::new(account, money, raw.direction),
    }
    .map_err(|error| entry_error(error, source))?;
    Ok(match tax {
        Some(category) => entry.with_tax_category(category),
        None => entry,
    })
}

/// Currency and optional entry amount bounds for a transaction search.
///
/// The bounds share one currency by construction. Only the SQL binding
/// exposes their minor-unit integers.
#[derive(Debug)]
pub struct TransactionValueFilter {
    currency: Currency,
    gt: Option<i64>,
    lt: Option<i64>,
    eq: Option<i64>,
}

impl TransactionValueFilter {
    /// Filter transactions by currency alone.
    #[must_use]
    pub const fn by_currency(currency: Currency) -> Self {
        Self {
            currency,
            gt: None,
            lt: None,
            eq: None,
        }
    }

    /// Require entries to meet the supplied exclusive or equality bounds.
    ///
    /// Returns `None` when no bounds were supplied.
    ///
    /// # Errors
    /// Returns [`CliError::Validation`] if bounds use different currencies
    /// or cannot be represented by the ledger's signed SQLite integers.
    pub fn from_bounds(
        gt: Option<Money>,
        lt: Option<Money>,
        eq: Option<Money>,
    ) -> Result<Option<Self>, CliError> {
        let Some(first) = gt.or(lt).or(eq) else {
            return Ok(None);
        };
        let currency = first.currency();
        let to_minor = |value: Money| {
            if value.currency() != currency {
                return Err(CliError::Validation(
                    "transaction amount bounds must use the same currency".into(),
                ));
            }
            i64::try_from(value.amount().minor_units()).map_err(|_| {
                CliError::Validation("transaction amount bound is outside the ledger range".into())
            })
        };
        Ok(Some(Self {
            currency,
            gt: gt.map(to_minor).transpose()?,
            lt: lt.map(to_minor).transpose()?,
            eq: eq.map(to_minor).transpose()?,
        }))
    }

    fn has_bounds(&self) -> bool {
        self.gt.is_some() || self.lt.is_some() || self.eq.is_some()
    }
}

/// Parameters for listing/searching transactions.
pub struct ListTransactionParams<'a> {
    /// Optional account code filter (matches entries).
    pub account_filter: Option<&'a str>,
    /// Optional start date (inclusive).
    pub from_date: Option<NaiveDate>,
    /// Optional end date (inclusive).
    pub to_date: Option<NaiveDate>,
    /// Maximum number of rows to return.
    pub limit: i64,
    /// Number of rows to skip.
    pub offset: i64,
    /// Substring search on transaction description (case-insensitive).
    pub description_like: Option<&'a str>,
    /// Match a currency, with optional entry amount bounds in that currency.
    pub value_filter: Option<TransactionValueFilter>,
    /// Exact match on transaction reference (idempotency key).
    pub reference_filter: Option<&'a str>,
    /// Substring search on transaction metadata (case-insensitive).
    pub metadata_like: Option<&'a str>,
    /// Exact-match filters on top-level metadata keys, AND-ed together
    /// (the shared rule in [`metadata_filter`]).
    pub meta_eq: &'a [metadata_filter::MetadataFilter],
    /// Exact match on entry tax category.
    pub tax_category_filter: Option<&'a str>,
    /// Filter entries by direction ("debit" or "credit").
    pub direction_filter: Option<DebitOrCredit>,
}

impl Default for ListTransactionParams<'_> {
    fn default() -> Self {
        Self {
            account_filter: None,
            from_date: None,
            to_date: None,
            limit: 50,
            offset: 0,
            description_like: None,
            value_filter: None,
            reference_filter: None,
            metadata_like: None,
            meta_eq: &[],
            tax_category_filter: None,
            direction_filter: None,
        }
    }
}

impl ListTransactionParams<'_> {
    /// Returns `true` if any filter requires joining the entries table.
    fn needs_entries_join(&self) -> bool {
        self.account_filter.is_some()
            || self
                .value_filter
                .as_ref()
                .is_some_and(TransactionValueFilter::has_bounds)
            || self.tax_category_filter.is_some()
            || self.direction_filter.is_some()
    }
}

/// Builds the shared FROM/JOIN/WHERE clause for transaction queries.
///
/// Returns `(sql, param_values, next_param_idx)` without ORDER BY, LIMIT, or OFFSET.
fn build_list_query(
    company: &Company<'_>,
    params: &ListTransactionParams<'_>,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>, u32) {
    let mut sql = String::from("FROM transactions t");

    if params.needs_entries_join() {
        sql.push_str(
            " JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug",
        );
    }

    sql.push_str(" WHERE t.company_slug = ?1");

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(company.slug().clone()));

    // Entry-level filters
    if let Some(account) = params.account_filter {
        let _ = write!(sql, " AND e.account_code = ?{param_idx}");
        param_values.push(Box::new(account.to_string()));
        param_idx += 1;
    }

    if let Some(filter) = &params.value_filter {
        let _ = write!(sql, " AND t.currency = ?{param_idx}");
        param_values.push(Box::new(filter.currency.code().to_owned()));
        param_idx += 1;

        for (operator, bound) in [(">", filter.gt), ("<", filter.lt), ("=", filter.eq)] {
            if let Some(amount) = bound {
                let _ = write!(sql, " AND e.amount {operator} ?{param_idx}");
                param_values.push(Box::new(amount));
                param_idx += 1;
            }
        }
    }

    if let Some(tax_cat) = params.tax_category_filter {
        let _ = write!(sql, " AND e.tax_category = ?{param_idx}");
        param_values.push(Box::new(tax_cat.to_string()));
        param_idx += 1;
    }

    if let Some(direction) = params.direction_filter {
        let _ = write!(sql, " AND e.direction = ?{param_idx}");
        param_values.push(Box::new(direction));
        param_idx += 1;
    }

    // Transaction-level filters
    if let Some(from) = params.from_date {
        let _ = write!(sql, " AND t.date >= ?{param_idx}");
        param_values.push(Box::new(from));
        param_idx += 1;
    }

    if let Some(to) = params.to_date {
        let _ = write!(sql, " AND t.date <= ?{param_idx}");
        param_values.push(Box::new(to));
        param_idx += 1;
    }

    if let Some(desc) = params.description_like {
        let _ = write!(sql, " AND t.description LIKE '%' || ?{param_idx} || '%'");
        param_values.push(Box::new(desc.to_string()));
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

    // Exact match on top-level metadata keys: the shared `--filter-metadata`
    // rule (see `metadata_filter`).
    if !params.meta_eq.is_empty() {
        let pred = metadata_filter::sql_predicate("t.metadata", params.meta_eq, param_idx);
        let _ = write!(sql, " AND {}", pred.sql);
        param_idx = pred.next_param;
        for v in pred.params {
            param_values.push(Box::new(v));
        }
    }

    (sql, param_values, param_idx)
}

/// List transactions matching the given filters.
///
/// # Errors
///
/// Returns [`CliError`] on database query failure.
pub fn list_transactions(
    company: &Company<'_>,
    params: &ListTransactionParams<'_>,
) -> Result<Vec<TransactionRow>, CliError> {
    let (where_clause, mut param_values, mut param_idx) = build_list_query(company, params);

    let mut sql = format!(
        "SELECT DISTINCT t.id, t.company_slug, t.description, \
         t.metadata, t.currency, t.date, t.posted_at, t.reference, t.posted_by, t.agent_uri, t.token_jti \
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

    let mut stmt = company.read().prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), decode_transaction)?;

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
    company: &Company<'_>,
    params: &ListTransactionParams<'_>,
) -> Result<i64, CliError> {
    let (where_clause, param_values, _) = build_list_query(company, params);

    let sql = format!("SELECT COUNT(DISTINCT t.id) {where_clause}");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let count: i64 = company
        .read()
        .query_row(&sql, params_refs.as_slice(), |row| row.get(0))?;
    Ok(count)
}

/// Fetches a transaction and its entries in the given company.
///
/// # Errors
/// Returns `CliError::NotFound` if the company or transaction does not exist.
pub fn get_transaction(
    company: &Company<'_>,
    id: TransactionId,
) -> Result<(TransactionRow, Vec<EntryRow>), CliError> {
    find_transaction(company, id)?.ok_or_else(|| missing_transaction(company, id))
}

pub(super) fn find_transaction(
    company: &Company<'_>,
    id: TransactionId,
) -> Result<Option<(TransactionRow, Vec<EntryRow>)>, CliError> {
    read_transaction_by(company.read(), company.slug(), id, decode_transaction)
}

fn missing_transaction(company: &Company<'_>, id: TransactionId) -> CliError {
    CliError::NotFound(format!(
        "transaction {id} not found in company '{}'",
        company.slug()
    ))
}

/// Read the exact stored date alongside the typed row when checking audit bytes.
pub(super) fn get_transaction_for_audit(
    company: &Company<'_>,
    id: TransactionId,
) -> Result<(TransactionRow, Vec<EntryRow>, String), CliError> {
    let ((txn, date_text), entries) =
        read_transaction_by(company.read(), company.slug(), id, |row| {
            Ok((decode_transaction(row)?, row.get(5)?))
        })?
        .ok_or_else(|| missing_transaction(company, id))?;
    Ok((txn, entries, date_text))
}

fn decode_transaction(row: &rusqlite::Row<'_>) -> rusqlite::Result<TransactionRow> {
    Ok(TransactionRow {
        id: row.get(0)?,
        company_slug: row.get(1)?,
        description: row.get(2)?,
        metadata: row.get(3)?,
        currency: row.get(4)?,
        date: super::calendar_date::read(row, 5)?,
        posted_at: row.get(6)?,
        reference: row.get(7)?,
        posted_by: row.get(8)?,
        agent_uri: row.get(9)?,
        token_jti: row.get(10)?,
    })
}

fn read_transaction_by<T>(
    conn: ReadConn<'_>,
    company: &CompanySlug,
    id: TransactionId,
    decode: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Option<(T, Vec<EntryRow>)>, CliError> {
    let company_slug = company.as_str();
    let mut stmt = conn.prepare(
        "SELECT id, company_slug, description, metadata, currency, date, posted_at, reference, posted_by, agent_uri, token_jti \
         FROM transactions \
         WHERE company_slug = ?1 AND id = ?2",
    )?;

    let mut rows = stmt.query_map(params![company_slug, id], decode)?;

    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let txn = row?;
    let entries = read_entries(conn, company, id)?;
    Ok(Some((txn, entries)))
}

/// Fetches the entries for a transaction in this company.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn get_entries_for_transaction(
    company: &Company<'_>,
    txn_id: TransactionId,
) -> Result<Vec<EntryRow>, CliError> {
    read_entries(company.read(), company.slug(), txn_id)
}

fn read_entries(
    conn: ReadConn<'_>,
    company: &CompanySlug,
    txn_id: TransactionId,
) -> Result<Vec<EntryRow>, CliError> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.transaction_id, e.account_code, e.company_slug, e.direction, \
         e.amount, e.memo, e.tax_category, e.status \
         FROM transactions t \
         JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug \
         WHERE t.id = ?1 AND t.company_slug = ?2 \
         ORDER BY e.id",
    )?;

    let rows = stmt.query_map(params![txn_id, company.as_str()], |row| {
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

/// Find orphaned intercompany correlations.
///
/// Returns transactions that reference a partner via `json_extract(metadata, '$.correlate')`
/// where the partner either doesn't exist or doesn't reference back.
///
/// # Errors
///
/// Returns [`CliError`] on database query failure.
pub fn find_orphaned_correlations(
    conn: ReadConn<'_>,
    _scope: AllCompanies,
) -> Result<Vec<OrphanedCorrelation>, CliError> {
    // NULL or non-JSON metadata means "no correlate" (issue #17); the guarded
    // expression is shared with the Rust-side rule in `metadata::has_correlate`.
    let own = metadata::sql_correlate("t.metadata");
    let partner = metadata::sql_correlate("t2.metadata");
    let sql = format!(
        "SELECT t.id, t.company_slug, t.description, t.date, \
                CAST({own} AS INTEGER) AS partner_id \
         FROM transactions t \
         WHERE {own} IS NOT NULL \
           AND NOT EXISTS ( \
               SELECT 1 FROM transactions t2 \
               WHERE t2.id = CAST({own} AS INTEGER) \
                 AND CAST({partner} AS INTEGER) = t.id \
           ) \
         ORDER BY t.id"
    );
    let mut stmt = conn.prepare(&sql)?;

    let rows = stmt.query_map([], |row| {
        Ok(OrphanedCorrelation {
            transaction_id: row.get(0)?,
            company_slug: row.get(1)?,
            description: row.get(2)?,
            date: super::calendar_date::read(row, 3)?,
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
    use crate::db::CreateAccountParams;
    use crate::db::test_support::fixture_actor;
    use crate::identity::{Admin, Granted};

    use super::*;
    use crate::db::accounts::create_account;
    use crate::db::companies::create_company;
    use crate::db::connection::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            create_company(
                grant,
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                &"Acme Corp".parse()?,
                None,
            )
        })
        .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "4000",
                    name: "Revenue",
                    account_type: beankeeper::types::AccountType::Revenue,
                    default_tax_category: None,
                },
            )
        })
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn sample_entries() -> Vec<PostEntryParams> {
        vec![
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ]
    }

    fn test_actor() -> &'static Actor {
        use std::sync::OnceLock;
        static A: OnceLock<Actor> = OnceLock::new();
        A.get_or_init(|| fixture_actor("test"))
    }

    fn make_params<'a>(
        entries: &'a [PostEntryParams],
        description: &'a str,
        metadata: Option<&'a str>,
        date: &'a str,
    ) -> PostTransactionParams<'a> {
        static COMPANY: std::sync::LazyLock<CompanySlug> =
            std::sync::LazyLock::new(|| "acme".parse().expect("fixture company"));
        PostTransactionParams {
            company_slug: &COMPANY,
            description,
            metadata,
            currency: Currency::USD,
            date: date
                .parse()
                .unwrap_or_else(|error| panic!("fixture date: {error}")),
            entries,
            correlate: None,
            reference: None,
            on_conflict: ConflictStrategy::Error,
            actor: test_actor(),
        }
    }

    #[test]
    fn post_and_get_transaction() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test sale", None, "2024-01-15");
        let id = post_fixture(&db, &p);
        assert!(id.is_ok());
        let PostResult::Created(id) = id.unwrap_or_else(|e| panic!("post failed: {e}")) else {
            panic!("expected Created");
        };

        let result = get_transaction(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme".parse().expect("fixture company"),
            )
            .expect("bind fixture company"),
            id,
        );
        assert!(result.is_ok());
        let (txn, entry_rows) = result.unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(txn.description, "Test sale");
        assert_eq!(entry_rows.len(), 2);
    }

    #[test]
    fn calendar_date_codec_preserves_ledger_text_and_reads_historical_dates() -> Result<(), CliError>
    {
        let db = setup();
        let date =
            NaiveDate::from_ymd_opt(2026, 1, 5).unwrap_or_else(|| panic!("invalid fixture date"));
        let (encoded, historical): (String, NaiveDate) =
            db.conn()
                .query_row("SELECT ?1, '2026-1-5'", [date], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(encoded, "2026-01-05");
        assert_eq!(historical, date);
        Ok(())
    }

    #[test]
    fn post_transaction_with_metadata() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test", Some(r#"{"ref":"INV-001"}"#), "2024-01-15");
        let id = post_fixture(&db, &p);
        assert!(id.is_ok());
        let PostResult::Created(id) = id.unwrap_or_else(|e| panic!("post failed: {e}")) else {
            panic!("expected Created");
        };
        let (txn, _) = get_transaction(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme".parse().expect("fixture company"),
            )
            .expect("bind fixture company"),
            id,
        )
        .unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(txn.metadata.as_deref(), Some(r#"{"ref":"INV-001"}"#));
    }

    #[test]
    fn storage_rejects_invalid_journals_before_writing() -> Result<(), CliError> {
        let db = setup();
        let count = || -> rusqlite::Result<i64> {
            db.conn().query_row(
            "SELECT (SELECT COUNT(*) FROM transactions) + (SELECT COUNT(*) FROM entries) + (SELECT COUNT(*) FROM audit_log)",
            [],
            |row| row.get(0),
        )
        };
        let before = count()?;
        let mut unbalanced = sample_entries();
        unbalanced[0].amount = (unbalanced[0].amount.minor_units() + 1).try_into()?;
        let mut two_debits = sample_entries();
        two_debits[1].direction = DebitOrCredit::Debit;
        let balanced = sample_entries();
        for entries in [unbalanced.as_slice(), two_debits.as_slice(), &balanced[..1]] {
            let request = make_params(entries, "Invalid", None, "2026-01-05");
            assert!(matches!(
                post_fixture(&db, &request),
                Err(CliError::Validation(_))
            ));
        }
        assert_eq!(count()?, before);
        Ok(())
    }

    #[test]
    fn direct_post_rejects_reserved_links_before_writing() -> Result<(), CliError> {
        let db = setup();
        let count = || -> rusqlite::Result<i64> {
            db.conn().query_row(
            "SELECT (SELECT COUNT(*) FROM transactions) + (SELECT COUNT(*) FROM entries) + (SELECT COUNT(*) FROM audit_log)",
            [],
            |row| row.get(0),
        )
        };
        let before = count()?;
        let entries = sample_entries();
        for key in ["correlate", "reverses", "reversed_by", "proposal_id"] {
            let metadata = format!(r#"{{"{key}":999}}"#);
            let request = make_params(&entries, "Forged link", Some(&metadata), "2024-01-15");
            assert!(matches!(
                post_fixture(&db, &request),
                Err(CliError::Validation(_))
            ));
        }
        assert_eq!(count()?, before);
        Ok(())
    }

    #[test]
    fn post_empty_entries_is_validation_error() {
        let db = setup();
        let p = make_params(&[], "Empty", None, "2024-01-15");
        let result = post_fixture(&db, &p);
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn posting_entry_decodes_historical_direction_spelling() -> Result<(), serde_json::Error> {
        let mut input = serde_json::json!({
            "account_code": "1000",
            "direction": "DEBIT",
            "amount": 5000
        });
        let entry: PostEntryParams = serde_json::from_value(input.clone())?;
        assert_eq!(entry.direction, DebitOrCredit::Debit);
        assert_eq!(serde_json::to_value(entry)?["direction"], "debit");
        input["direction"] = serde_json::json!("INVALID");
        assert!(serde_json::from_value::<PostEntryParams>(input).is_err());
        Ok(())
    }

    #[test]
    fn get_missing_transaction_is_not_found() {
        let db = setup();
        let result = get_transaction(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme".parse().expect("fixture company"),
            )
            .expect("bind fixture company"),
            crate::db::ids::fixture_id(999),
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_transactions_returns_all() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "First", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "Second", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_transactions_with_account_filter() {
        let db = setup();
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "5000",
                    name: "Expenses",
                    account_type: beankeeper::types::AccountType::Expense,
                    default_tax_category: None,
                },
            )
        })
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));

        let entries1 = sample_entries();
        let p1 = make_params(&entries1, "Sale", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());

        let entries2 = vec![
            PostEntryParams {
                account_code: "5000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 1000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 1000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ];
        let p2 = make_params(&entries2, "Expense", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            account_filter: Some("5000"),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Expense");
    }

    #[test]
    fn list_transactions_with_date_range() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "Jan", None, "2024-01-15");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "Feb", None, "2024-02-15");
        assert!(post_fixture(&db, &p2).is_ok());
        let p3 = make_params(&entries, "Mar", None, "2024-03-15");
        assert!(post_fixture(&db, &p3).is_ok());

        let lp = ListTransactionParams {
            from_date: NaiveDate::from_ymd_opt(2024, 2, 1),
            to_date: NaiveDate::from_ymd_opt(2024, 2, 28),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Feb");
    }

    #[test]
    fn get_entries_for_transaction_respects_company_scope() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Test", None, "2024-01-15");
        let id = post_fixture(&db, &p).unwrap_or_else(|e| panic!("post failed: {e}"));
        let PostResult::Created(id) = id else {
            panic!("expected Created");
        };

        let actor = fixture_actor("fixture");
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            create_company(
                grant,
                &"other"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                &"Other".parse()?,
                None,
            )
        })
        .expect("create company");
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"other"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Other cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })
        .expect("create account");
        db.conn()
            .execute(
                "INSERT INTO entries (transaction_id, account_code, company_slug, direction, amount)
                 VALUES (?1, '1000', 'other', 'debit', 100)",
                params![id],
            )
            .expect("seed a mismatched transaction and entry company");

        let company = Company::bind(db.read(), "acme".parse().expect("fixture company"))
            .expect("bind transaction company");
        let entry_rows = get_entries_for_transaction(&company, id).expect("read entries");
        assert_eq!(entry_rows.len(), 2);
        assert_eq!(
            entry_rows[0].direction,
            beankeeper::types::DebitOrCredit::Debit
        );
        assert_eq!(
            entry_rows[1].direction,
            beankeeper::types::DebitOrCredit::Credit
        );

        let only_foreign_amount = ListTransactionParams {
            value_filter: TransactionValueFilter::from_bounds(None, None, Some(Money::usd(100)))
                .expect("one currency"),
            ..Default::default()
        };
        assert!(
            list_transactions(&company, &only_foreign_amount)
                .expect("list matching transactions")
                .is_empty()
        );
        assert_eq!(
            count_transactions(&company, &only_foreign_amount)
                .expect("count matching transactions"),
            0
        );

        let other = Company::bind(db.read(), "other".parse().expect("fixture company"))
            .expect("bind other company");
        assert!(
            get_entries_for_transaction(&other, id)
                .expect("read entries for other company")
                .is_empty()
        );
    }

    #[test]
    fn transaction_reads_reject_unsupported_stored_currency() -> Result<(), CliError> {
        let db = setup();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date)
             VALUES ('acme', 'Legacy transaction', 'BAD', '2024-01-15')",
            [],
        )?;
        let result = get_transaction(
            &crate::db::companies::Company::bind(ReadConn::new(conn), "acme".parse()?)?,
            crate::db::ids::fixture_id(conn.last_insert_rowid()),
        );
        assert!(matches!(
            result,
            Err(CliError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                ..
            )))
        ));
        Ok(())
    }

    #[test]
    fn savepoint_rolls_back_on_error() {
        let db = setup();
        // This should fail because account "9999" doesn't exist (FK constraint)
        let entries = vec![
            PostEntryParams {
                account_code: "1000".to_string(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "9999".to_string(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ];
        let p = make_params(&entries, "Bad", None, "2024-01-15");
        let result = post_fixture(&db, &p);
        // The foreign key violation should cause an error
        assert!(result.is_err());

        // Verify no transaction was partially committed
        let lp = ListTransactionParams {
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn list_transactions_with_description_filter() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "AWS monthly bill", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "Office supplies", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            description_like: Some("AWS"),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "AWS monthly bill");
    }

    #[test]
    fn transaction_amount_filter_rejects_mixed_currencies_and_unbindable_amounts() {
        for (gt, lt) in [
            (Money::usd(100), Money::jpy(200)),
            (Money::usd(100), Money::usd(i128::from(i64::MAX) + 1)),
        ] {
            assert!(matches!(
                TransactionValueFilter::from_bounds(Some(gt), Some(lt), None),
                Err(CliError::Validation(_))
            ));
        }
    }

    #[test]
    fn list_transactions_with_amount_gt() {
        let db = setup();
        let small_entries = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 1000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 1000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ];
        let big_entries = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 50000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 50000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ];
        let p1 = make_params(&small_entries, "Small", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&big_entries, "Big", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            value_filter: TransactionValueFilter::from_bounds(Some(Money::usd(10000)), None, None)
                .unwrap_or_else(|error| panic!("{error}")),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Big");
    }

    #[test]
    fn list_transactions_with_amount_eq() {
        let db = setup();
        let entries = sample_entries(); // amount 5000
        let p1 = make_params(&entries, "Exact", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());

        let lp = ListTransactionParams {
            value_filter: TransactionValueFilter::from_bounds(None, None, Some(Money::usd(5000)))
                .unwrap_or_else(|error| panic!("{error}")),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);

        let lp2 = ListTransactionParams {
            value_filter: TransactionValueFilter::from_bounds(None, None, Some(Money::usd(9999)))
                .unwrap_or_else(|error| panic!("{error}")),
            limit: 100,
            ..Default::default()
        };
        let list2 = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp2,
        )
        .unwrap_or_default();
        assert_eq!(list2.len(), 0);
    }

    #[test]
    fn list_transactions_with_reference_filter() {
        let db = setup();
        let entries = sample_entries();
        let mut p1 = make_params(&entries, "With ref", None, "2024-01-01");
        p1.reference = Some("INV-001");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "No ref", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            reference_filter: Some("INV-001"),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
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
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "Untagged", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            metadata_like: Some("AWS"),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Tagged");
    }

    #[test]
    fn list_transactions_with_direction_filter() {
        let db = setup();
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "5000",
                    name: "Expenses",
                    account_type: beankeeper::types::AccountType::Expense,
                    default_tax_category: None,
                },
            )
        })
        .unwrap_or_else(|e| panic!("account setup failed: {e}"));

        let entries = sample_entries();
        let p1 = make_params(&entries, "Sale", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());

        // Filter for transactions where account 1000 has a debit entry
        let lp = ListTransactionParams {
            account_filter: Some("1000"),
            direction_filter: Some(DebitOrCredit::Debit),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);

        // Filter for transactions where account 1000 has a credit entry (none)
        let lp2 = ListTransactionParams {
            account_filter: Some("1000"),
            direction_filter: Some(DebitOrCredit::Credit),
            limit: 100,
            ..Default::default()
        };
        let list2 = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp2,
        )
        .unwrap_or_default();
        assert_eq!(list2.len(), 0);
    }

    #[test]
    fn count_transactions_returns_count() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "First", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "Second", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());
        let p3 = make_params(&entries, "Third", None, "2024-01-03");
        assert!(post_fixture(&db, &p3).is_ok());

        let lp = ListTransactionParams::default();
        let count = count_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or(0);
        assert_eq!(count, 3);

        let lp2 = ListTransactionParams {
            from_date: NaiveDate::from_ymd_opt(2024, 1, 2),
            ..Default::default()
        };
        let count2 = count_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp2,
        )
        .unwrap_or(0);
        assert_eq!(count2, 2);
    }

    #[test]
    fn list_transactions_combined_filters() {
        let db = setup();
        let entries = sample_entries();
        let p1 = make_params(&entries, "AWS January", None, "2024-01-15");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries, "AWS February", None, "2024-02-15");
        assert!(post_fixture(&db, &p2).is_ok());
        let p3 = make_params(&entries, "Office supplies", None, "2024-01-20");
        assert!(post_fixture(&db, &p3).is_ok());

        // Combine description + date range
        let lp = ListTransactionParams {
            description_like: Some("AWS"),
            from_date: NaiveDate::from_ymd_opt(2024, 2, 1),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "AWS February");
    }

    #[test]
    fn list_transactions_with_tax_category_filter() {
        let db = setup();
        let entries_with_tax = vec![
            PostEntryParams {
                account_code: "1000".into(),
                direction: beankeeper::types::DebitOrCredit::Debit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: Some("sched-c:24b".into()),
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: beankeeper::types::DebitOrCredit::Credit,
                amount: 5000_i64
                    .try_into()
                    .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                memo: None,
                tax_category: None,
            },
        ];
        let entries_no_tax = sample_entries();

        let p1 = make_params(&entries_with_tax, "Taxed", None, "2024-01-01");
        assert!(post_fixture(&db, &p1).is_ok());
        let p2 = make_params(&entries_no_tax, "Untaxed", None, "2024-01-02");
        assert!(post_fixture(&db, &p2).is_ok());

        let lp = ListTransactionParams {
            tax_category_filter: Some("sched-c:24b"),
            limit: 100,
            ..Default::default()
        };
        let list = list_transactions(
            &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                .expect("bind fixture company"),
            &lp,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].description, "Taxed");
    }

    #[test]
    fn post_transaction_wraps_plain_text_metadata() {
        let db = setup();
        let entries = sample_entries();
        let p = make_params(&entries, "Memo", Some("open:1000"), "2024-01-15");
        let PostResult::Created(id) =
            post_fixture(&db, &p).unwrap_or_else(|e| panic!("post failed: {e}"))
        else {
            panic!("expected Created");
        };
        let (txn, _) = get_transaction(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme".parse().expect("fixture company"),
            )
            .expect("bind fixture company"),
            id,
        )
        .unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(txn.metadata.as_deref(), Some(r#"{"memo":"open:1000"}"#));
    }

    #[test]
    fn find_orphaned_correlations_tolerates_non_json_metadata() {
        let db = setup();
        // Bypass normalization to simulate rows written before schema v9.
        for meta in ["open:1000", "[1,2]", "", r#"{"correlate":42}"#] {
            db.conn()
                .execute(
                    "INSERT INTO transactions (company_slug, description, metadata, date) \
                     VALUES ('acme', 'raw', ?1, '2024-01-01')",
                    params![meta],
                )
                .unwrap_or_else(|e| panic!("insert failed: {e}"));
        }
        let orphans = find_orphaned_correlations(db.read(), AllCompanies)
            .unwrap_or_else(|e| panic!("reconcile: {e}"));
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].partner_id, 42);
    }

    #[test]
    fn list_transactions_with_meta_eq_filter() {
        let db = setup();
        let entries = sample_entries();
        let metas = [
            r#"{"vendor":"AWS","qty":3}"#,
            r#"{"vendor":"aws"}"#,
            r#"{"memo":"vendor=AWS"}"#,
        ];
        for m in metas {
            let p = make_params(&entries, m, Some(m), "2024-01-01");
            assert!(post_fixture(&db, &p).is_ok());
        }
        // A pre-v9 free-text row must not break the json_extract filter.
        db.conn()
            .execute(
                "INSERT INTO transactions (company_slug, description, metadata, date) \
                 VALUES ('acme', 'raw', 'vendor AWS', '2024-01-01')",
                [],
            )
            .unwrap_or_else(|e| panic!("insert failed: {e}"));

        let check = |pairs: &[(&str, &str)], expected: usize| {
            let owned: Vec<metadata_filter::MetadataFilter> = pairs
                .iter()
                .map(|(k, v)| metadata_filter::MetadataFilter {
                    key: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect();
            let lp = ListTransactionParams {
                meta_eq: &owned,
                ..Default::default()
            };
            let list = list_transactions(
                &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                    .expect("bind fixture company"),
                &lp,
            )
            .unwrap_or_else(|e| panic!("list: {e}"));
            assert_eq!(list.len(), expected, "filters {pairs:?}");
            assert_eq!(
                count_transactions(
                    &Company::bind(db.read(), "acme".parse().expect("fixture company"))
                        .expect("bind fixture company"),
                    &lp
                )
                .unwrap_or(-1),
                i64::try_from(expected).unwrap_or(-1)
            );
        };
        check(&[("vendor", "AWS")], 1);
        check(&[("vendor", "AWS"), ("qty", "3")], 1);
        check(&[("vendor", "aws"), ("qty", "3")], 0);
        check(&[("qty", "3.0")], 0);
        check(&[("memo", "vendor=AWS")], 1);
        check(&[("missing", "")], 0);
    }

    #[test]
    fn post_transaction_skip_conflict() -> Result<(), Box<dyn std::error::Error>> {
        let db = setup();
        let entries = sample_entries();
        let mut p1 = make_params(&entries, "First", None, "2024-01-01");
        p1.reference = Some("REF-1");
        let res1 = post_fixture(&db, &p1)?;
        let PostResult::Created(id1) = res1 else {
            panic!("expected Created");
        };

        let mut p2 = make_params(&entries, "Duplicate", None, "2024-01-01");
        p2.reference = Some("REF-1");
        p2.on_conflict = ConflictStrategy::Skip;
        let res2 = post_fixture(&db, &p2)?;
        assert_eq!(res2, PostResult::Skipped(id1));

        // Verify only one transaction exists
        let lp = ListTransactionParams::default();
        let count = count_transactions(
            &Company::bind(db.read(), "acme".parse()?).expect("bind fixture company"),
            &lp,
        )?;
        assert_eq!(count, 1);
        Ok(())
    }
}
