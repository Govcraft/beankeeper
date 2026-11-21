//! JSON output rendering with `serde_json`.
//!
//! Each function converts database row types into serialisable wrappers and
//! produces pretty-printed JSON wrapped in a uniform envelope:
//!
//! ```json
//! { "ok": true, "meta": { "command": "...", "timestamp": "..." }, "data": ... }
//! ```
//!
//! Monetary fields retain raw integers (minor units) and gain decimal-string
//! display companions. Enum values are always lowercase.

use std::collections::HashMap;

use beankeeper::types::{Currency, Sha256Hex};
use chrono::{NaiveDate, SecondsFormat, Utc};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::db::clearings::Clearing;
use crate::db::ids::{AttachmentId, TransactionId};
use crate::db::{
    AccountRow, AccountWithBalanceRow, AttachmentRow, BalanceRow, ClearingState, CompanyRow,
    EntryRow, EntryStatus, PostResult, TransactionRow,
};
use crate::error::{CliError, ErrorCode};

// ---------------------------------------------------------------------------
// Envelope types
// ---------------------------------------------------------------------------

pub(crate) fn serialize_display<S: Serializer>(
    value: &impl std::fmt::Display,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

/// Response metadata included in every JSON envelope.
#[derive(Serialize, Clone)]
pub struct Meta {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<Pagination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

/// A filtered result page. Negative limits mean unlimited, as in SQLite.
#[derive(Debug, Clone, Serialize)]
pub struct Pagination {
    pub limit: i64,
    pub offset: i64,
    pub returned: usize,
    pub total: i64,
    pub has_more: bool,
}

impl Pagination {
    /// Describe a page using the same filters as its total count.
    #[must_use]
    pub fn new(limit: i64, offset: i64, returned: usize, total: i64) -> Self {
        let offset = offset.max(0);
        Self {
            limit,
            offset,
            returned,
            total,
            has_more: offset.saturating_add(i64::try_from(returned).unwrap_or(i64::MAX)) < total,
        }
    }
}

impl Meta {
    /// Attach pagination without changing the data payload shape.
    #[must_use]
    pub fn with_pagination(mut self, pagination: Pagination) -> Self {
        self.pagination = Some(pagination);
        self
    }

    /// Supply the currency for payloads that do not carry their own currency.
    #[must_use]
    pub fn with_currency(mut self, currency: Currency) -> Self {
        self.currency = Some(currency.code().to_owned());
        self
    }
}

/// Uniform JSON response envelope.
pub struct Envelope<T: Serialize> {
    pub ok: bool,
    pub meta: Meta,
    pub data: Option<T>,
    pub error: Option<EnvelopeError>,
}

impl<T: Serialize> Serialize for Envelope<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fields = 2 + usize::from(self.data.is_some()) + usize::from(self.error.is_some());
        let mut envelope = serializer.serialize_struct("Envelope", fields)?;
        envelope.serialize_field("ok", &self.ok)?;
        envelope.serialize_field("meta", &self.meta)?;
        if let Some(data) = &self.data {
            let payload = serialize_payload(data, &self.meta).map_err(serde::ser::Error::custom)?;
            envelope.serialize_field("data", &payload)?;
        }
        if let Some(error) = &self.error {
            envelope.serialize_field("error", error)?;
        }
        envelope.end()
    }
}

pub(crate) fn serialize_payload(
    data: &impl Serialize,
    meta: &Meta,
) -> Result<serde_json::Value, serde_json::Error> {
    let value = serde_json::to_value(data)?;
    let currency = meta
        .currency
        .as_deref()
        .and_then(|code| Currency::from_code(code).ok());
    Ok(with_amount_displays(value, currency))
}

pub use super::amounts::with_amount_displays;

/// Error payload within the envelope.
#[derive(Serialize)]
pub struct EnvelopeError {
    pub code: ErrorCode,
    pub message: String,
}

/// Construct a `Meta` with the current UTC timestamp.
#[must_use]
pub fn meta(command: &str, company: Option<&str>) -> Meta {
    Meta {
        command: command.to_string(),
        agent: None,
        company: company.map(str::to_string),
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        pagination: None,
        currency: None,
    }
}

/// Construct a `Meta` with an explicit timestamp (for deterministic tests).
#[must_use]
pub fn meta_with_timestamp(command: &str, company: Option<&str>, timestamp: String) -> Meta {
    Meta {
        command: command.to_string(),
        agent: None,
        company: company.map(str::to_string),
        timestamp,
        pagination: None,
        currency: None,
    }
}

/// Serialise a success envelope.
pub(crate) fn envelope_ok<T: Serialize>(meta: Meta, data: T) -> Result<String, CliError> {
    let env = Envelope {
        ok: true,
        meta,
        data: Some(data),
        error: None::<EnvelopeError>,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

// ---------------------------------------------------------------------------
// Serialisable wrapper types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct CompanyJson {
    slug: String,
    name: String,
    description: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
pub struct AccountJson {
    code: String,
    name: String,
    r#type: String,
    normal_balance: String,
}

#[derive(Serialize)]
pub struct TransactionJson {
    id: i64,
    description: String,
    metadata: Option<String>,
    reference: Option<String>,
    posted_by: Option<String>,
    agent_uri: Option<String>,
    token_jti: Option<String>,
    currency: String,
    date: String,
    entries: Vec<EntryJson>,
}

#[derive(Serialize)]
pub struct EntryJson {
    /// Entry id (the `--entry` of `txn clear`).
    id: i64,
    account_code: String,
    direction: String,
    amount: i64,
    memo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tax_category: Option<String>,
    status: EntryStatus,
    /// Total cleared amount (`txn show` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    cleared_amount: Option<i64>,
    /// `amount - cleared_amount` (`txn show` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_amount: Option<i64>,
    /// The entry's individual clearings, oldest first (`txn show` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    clearings: Option<Vec<ClearingJson>>,
}

/// One clearing of an entry (`txn show`).
#[derive(Serialize)]
pub struct ClearingJson {
    id: i64,
    amount: i64,
    cleared_date: NaiveDate,
    #[serde(rename = "statement")]
    statement_ref: Option<String>,
    #[serde(flatten)]
    state: ClearingState,
}

/// Builds an entry's JSON; `clearings` adds the clearing breakdown.
fn entry_json(e: &EntryRow, clearings: Option<&[Clearing]>) -> EntryJson {
    let cleared = clearings.map(crate::db::clearings::cleared_sum);
    EntryJson {
        id: e.id,
        account_code: e.account_code.clone(),
        direction: e.direction.clone(),
        amount: e.amount,
        memo: e.memo.clone(),
        tax_category: e.tax_category.clone(),
        status: e.status,
        cleared_amount: cleared,
        remaining_amount: cleared.map(|c| e.amount - c),
        clearings: clearings.map(|cs| {
            cs.iter()
                .map(|c| ClearingJson {
                    id: c.id,
                    amount: c.amount,
                    cleared_date: c.cleared_on(),
                    statement_ref: c.statement_ref.clone(),
                    state: c.state(),
                })
                .collect()
        }),
    }
}

#[derive(Serialize)]
pub struct AttachmentJson<'a> {
    id: AttachmentId,
    document_type: &'static str,
    uri: &'a str,
    hash: Option<&'a str>,
    original_filename: Option<&'a str>,
    attached_at: &'a str,
}

#[derive(Serialize)]
struct TransactionWithAttachmentsJson<'a> {
    id: i64,
    description: String,
    metadata: Option<String>,
    reference: Option<String>,
    posted_by: Option<String>,
    agent_uri: Option<String>,
    token_jti: Option<String>,
    currency: String,
    date: String,
    entries: Vec<EntryJson>,
    attachments: Vec<AttachmentJson<'a>>,
}

#[derive(Serialize)]
pub struct TrialBalanceJson {
    accounts: Vec<TrialBalanceAccountJson>,
    total_debits: i64,
    total_credits: i64,
    balanced: bool,
}

#[derive(Serialize)]
pub struct TrialBalanceAccountJson {
    code: String,
    name: String,
    r#type: String,
    debit_total: i64,
    credit_total: i64,
}

#[derive(Serialize)]
pub struct BalanceJson {
    code: String,
    name: String,
    r#type: String,
    debit_total: i64,
    credit_total: i64,
    currency: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a lowercase account-type string to its normal balance direction.
fn normal_balance_for(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "debit",
        "liability" | "equity" | "revenue" => "credit",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Render functions
// ---------------------------------------------------------------------------

/// Render a list of companies as an enveloped JSON response.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_companies(companies: &[CompanyRow], meta: Meta) -> Result<String, CliError> {
    let rows: Vec<CompanyJson> = companies
        .iter()
        .map(|c| CompanyJson {
            slug: c.slug.clone(),
            name: c.name.clone(),
            description: c.description.clone(),
            created_at: c.created_at.clone(),
        })
        .collect();

    envelope_ok(meta, rows)
}

/// Render a list of accounts as an enveloped JSON response.
///
/// Account types and normal-balance directions are always lowercase.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_accounts(accounts: &[AccountRow], meta: Meta) -> Result<String, CliError> {
    let rows: Vec<AccountJson> = accounts
        .iter()
        .map(|a| AccountJson {
            code: a.code.clone(),
            name: a.name.clone(),
            r#type: a.account_type.clone(),
            normal_balance: normal_balance_for(&a.account_type).to_string(),
        })
        .collect();

    envelope_ok(meta, rows)
}

/// Render transactions with their entries as an enveloped JSON response.
///
/// `entries_map` maps transaction IDs to their entry rows.  Transactions
/// without entries in the map are rendered with an empty `entries` array.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_transactions<S: ::std::hash::BuildHasher>(
    transactions: &[TransactionRow],
    entries_map: &HashMap<i64, Vec<EntryRow>, S>,
    meta: Meta,
) -> Result<String, CliError> {
    let rows: Vec<TransactionJson> = transactions
        .iter()
        .map(|txn| {
            let entries = entries_map
                .get(&txn.id)
                .map(|rows| rows.iter().map(|e| entry_json(e, None)).collect())
                .unwrap_or_default();

            TransactionJson {
                id: txn.id,
                description: txn.description.clone(),
                metadata: txn.metadata.clone(),
                reference: txn.reference.clone(),
                posted_by: txn.posted_by.clone(),
                agent_uri: txn.agent_uri.clone(),
                token_jti: txn.token_jti.clone(),
                currency: txn.currency.clone(),
                date: txn.date.clone(),
                entries,
            }
        })
        .collect();

    envelope_ok(meta, rows)
}

/// Render transactions with entries and attachments as an enveloped JSON response.
///
/// `clearings_map` maps entry ids to their clearings; every entry then also
/// carries `cleared_amount`, `remaining_amount` and `clearings` (entries
/// missing from the map have none).
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_transactions_with_attachments<
    S: ::std::hash::BuildHasher,
    T: ::std::hash::BuildHasher,
    U: ::std::hash::BuildHasher,
>(
    transactions: &[TransactionRow],
    entries_map: &HashMap<i64, Vec<EntryRow>, S>,
    attachments_map: &HashMap<i64, Vec<AttachmentRow>, T>,
    clearings_map: &HashMap<i64, Vec<Clearing>, U>,
    meta: Meta,
) -> Result<String, CliError> {
    let rows: Vec<_> = transactions
        .iter()
        .map(|txn| {
            let entries = entries_map
                .get(&txn.id)
                .map(|rows| {
                    rows.iter()
                        .map(|e| {
                            let clearings = clearings_map.get(&e.id).map_or(&[][..], Vec::as_slice);
                            entry_json(e, Some(clearings))
                        })
                        .collect()
                })
                .unwrap_or_default();

            let att = attachments_map
                .get(&txn.id)
                .map(|rows| {
                    rows.iter()
                        .map(|a| AttachmentJson {
                            id: a.id,
                            document_type: a.document_type.as_str(),
                            uri: &a.uri,
                            hash: a.hash.as_ref().map(Sha256Hex::as_str),
                            original_filename: a.original_filename.as_deref(),
                            attached_at: &a.attached_at,
                        })
                        .collect()
                })
                .unwrap_or_default();

            TransactionWithAttachmentsJson {
                id: txn.id,
                description: txn.description.clone(),
                metadata: txn.metadata.clone(),
                reference: txn.reference.clone(),
                posted_by: txn.posted_by.clone(),
                agent_uri: txn.agent_uri.clone(),
                token_jti: txn.token_jti.clone(),
                currency: txn.currency.clone(),
                date: txn.date.clone(),
                entries,
                attachments: att,
            }
        })
        .collect();

    envelope_ok(meta, rows)
}

/// Render a trial balance as an enveloped JSON response with accounts, totals,
/// and a `balanced` boolean.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails, or a validation error
/// when a total cannot be represented.
pub fn render_trial_balance(balances: &[BalanceRow], meta: Meta) -> Result<String, CliError> {
    let super::TrialBalanceTotals {
        debits: total_debits,
        credits: total_credits,
    } = super::TrialBalanceTotals::from_rows(balances)?;

    let accounts: Vec<TrialBalanceAccountJson> = balances
        .iter()
        .map(|balance| TrialBalanceAccountJson {
            code: balance.code.clone(),
            name: balance.name.clone(),
            r#type: balance.account_type.clone(),
            debit_total: balance.debit_total,
            credit_total: balance.credit_total,
        })
        .collect();

    let result = TrialBalanceJson {
        accounts,
        total_debits,
        total_credits,
        balanced: total_debits == total_credits,
    };

    envelope_ok(meta, result)
}

/// Render a single account balance as an enveloped JSON response.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_account_balance(
    balance: &BalanceRow,
    currency: &str,
    meta: Meta,
) -> Result<String, CliError> {
    let json = BalanceJson {
        code: balance.code.clone(),
        name: balance.name.clone(),
        r#type: balance.account_type.clone(),
        debit_total: balance.debit_total,
        credit_total: balance.credit_total,
        currency: currency.to_string(),
    };

    envelope_ok(meta, json)
}

// ---------------------------------------------------------------------------
// Orphaned correlations
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OrphanedCorrelationJson {
    transaction_id: i64,
    company: String,
    description: String,
    date: String,
    partner_id: i64,
}

/// Render orphaned intercompany correlations as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] if serialization fails.
pub fn render_orphaned_correlations(
    orphans: &[crate::db::OrphanedCorrelation],
    meta: Meta,
) -> Result<String, CliError> {
    let json_orphans: Vec<OrphanedCorrelationJson> = orphans
        .iter()
        .map(|o| OrphanedCorrelationJson {
            transaction_id: o.transaction_id,
            company: o.company_slug.clone(),
            description: o.description.clone(),
            date: o.date.clone(),
            partner_id: o.partner_id,
        })
        .collect();

    envelope_ok(meta, json_orphans)
}

// ---------------------------------------------------------------------------
// Tax summary
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TaxSummaryEntryJson {
    tax_category: String,
    debit_total: i64,
    credit_total: i64,
}

/// Render a tax summary as an enveloped JSON response.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_tax_summary(
    rows: &[crate::db::TaxSummaryRow],
    meta: Meta,
) -> Result<String, CliError> {
    let json_rows: Vec<TaxSummaryEntryJson> = rows
        .iter()
        .map(|r| TaxSummaryEntryJson {
            tax_category: r.tax_category.clone(),
            debit_total: r.debit_total,
            credit_total: r.credit_total,
        })
        .collect();

    envelope_ok(meta, json_rows)
}

// ---------------------------------------------------------------------------
// Account with balances
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AccountWithBalanceJson {
    code: String,
    name: String,
    r#type: String,
    normal_balance: String,
    default_tax_category: Option<String>,
    debit_total: i64,
    credit_total: i64,
}

/// Render accounts with balance totals as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_accounts_with_balances(
    rows: &[AccountWithBalanceRow],
    meta: Meta,
) -> Result<String, CliError> {
    let json_rows: Vec<AccountWithBalanceJson> = rows
        .iter()
        .map(|row| AccountWithBalanceJson {
            code: row.code.clone(),
            name: row.name.clone(),
            r#type: row.account_type.clone(),
            normal_balance: normal_balance_for(&row.account_type).to_string(),
            default_tax_category: row.default_tax_category.clone(),
            debit_total: row.debit_total,
            credit_total: row.credit_total,
        })
        .collect();

    envelope_ok(meta, json_rows)
}

// ---------------------------------------------------------------------------
// Simple data helpers for mutation commands
// ---------------------------------------------------------------------------

/// Render a count as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_count(count: i64, meta: Meta) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct CountJson {
        count: i64,
    }
    envelope_ok(meta, CountJson { count })
}

/// Render a deletion confirmation as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_deleted(slug: &str, meta: Meta) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct DeletedJson<'a> {
        deleted: &'a str,
    }
    envelope_ok(meta, DeletedJson { deleted: slug })
}

/// Render a transaction post result as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_post_result(result: PostResult, meta: Meta) -> Result<String, CliError> {
    envelope_ok(meta, post_result_data(result)?)
}

pub(crate) fn post_result_data(result: PostResult) -> Result<serde_json::Value, CliError> {
    #[derive(Serialize)]
    struct PostResultJson {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        existing_id: Option<i64>,
        created: bool,
        skipped: bool,
    }

    let json = match result {
        PostResult::Created(id) => PostResultJson {
            id: Some(id),
            existing_id: None,
            created: true,
            skipped: false,
        },
        PostResult::Skipped(id) => PostResultJson {
            id: None,
            existing_id: Some(id),
            created: false,
            skipped: true,
        },
    };

    serde_json::to_value(json).map_err(|error| CliError::General(error.to_string()))
}

/// Render a `txn attach` confirmation as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_attached(
    attachment_id: AttachmentId,
    transaction_id: TransactionId,
    meta: Meta,
) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct AttachedJson {
        id: AttachmentId,
        transaction_id: TransactionId,
    }
    envelope_ok(
        meta,
        AttachedJson {
            id: attachment_id,
            transaction_id,
        },
    )
}

/// Render an `init` confirmation as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_init(path: &str, meta: Meta) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct InitJson<'a> {
        path: &'a str,
    }
    envelope_ok(meta, InitJson { path })
}

/// Render a `verify` confirmation as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_verify(schema_version: i64, meta: Meta) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct VerifyJson {
        schema_version: i64,
        status: &'static str,
    }
    envelope_ok(
        meta,
        VerifyJson {
            schema_version,
            status: "healthy",
        },
    )
}

/// Render an import result as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_import_result(
    result: &crate::commands::import_ofx::ImportResult,
    dry_run: bool,
    meta: Meta,
) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct ImportResultJson {
        dry_run: bool,
        imported: usize,
        skipped: usize,
        errors: usize,
        transactions: Vec<ImportTransactionJson>,
    }

    #[derive(Serialize)]
    struct ImportTransactionJson {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<i64>,
        date: String,
        description: String,
        amount: i64,
        currency: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    }

    let mut transactions = Vec::new();

    for t in &result.imported {
        transactions.push(ImportTransactionJson {
            id: if dry_run { None } else { Some(t.id) },
            date: t.date.clone(),
            description: t.description.clone(),
            amount: t.amount_minor,
            currency: t.currency.clone(),
            status: "imported".to_string(),
            reason: None,
        });
    }
    for t in &result.skipped {
        transactions.push(ImportTransactionJson {
            id: None,
            date: t.date.clone(),
            description: t.description.clone(),
            amount: t.amount_minor,
            currency: t.currency.clone(),
            status: "skipped".to_string(),
            reason: Some("duplicate".to_string()),
        });
    }
    for t in &result.errors {
        transactions.push(ImportTransactionJson {
            id: None,
            date: t.date.clone(),
            description: t.description.clone(),
            amount: t.amount_minor,
            currency: t.currency.clone(),
            status: "error".to_string(),
            reason: Some(t.error.clone()),
        });
    }

    envelope_ok(
        meta,
        ImportResultJson {
            dry_run,
            imported: result.imported.len(),
            skipped: result.skipped.len(),
            errors: result.errors.len(),
            transactions,
        },
    )
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct BudgetJson {
    id: i64,
    account_code: String,
    currency: String,
    year: i32,
    month: i32,
    #[serde(flatten)]
    amount: super::amounts::MoneyField,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    created_at: String,
}

/// Render budget rows as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on invalid stored currency or serialisation failure.
pub fn render_budgets(rows: &[crate::db::BudgetRow], meta: Meta) -> Result<String, CliError> {
    let json_rows = rows
        .iter()
        .map(|row| {
            Ok(BudgetJson {
                id: row.id,
                account_code: row.account_code.clone(),
                currency: row.currency.clone(),
                year: row.year,
                month: row.month,
                amount: super::amounts::MoneyField::new("amount", row.money()?),
                notes: row.notes.clone(),
                created_at: row.created_at.clone(),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    envelope_ok(meta, json_rows)
}

#[derive(Serialize)]
struct BudgetVarianceJson<'a> {
    code: &'a str,
    name: &'a str,
    account_type: &'static str,
    #[serde(flatten)]
    budget: super::amounts::MoneyField,
    #[serde(flatten)]
    actual: super::amounts::MoneyField,
    #[serde(flatten)]
    variance: super::amounts::MoneyField,
    variance_percent: Option<f64>,
    favorable: bool,
}

#[derive(Serialize)]
struct BudgetVarianceTotals {
    #[serde(flatten)]
    budget: super::amounts::MoneyField,
    #[serde(flatten)]
    actual: super::amounts::MoneyField,
}

#[derive(Serialize)]
struct BudgetVarianceData<'a> {
    year: i32,
    from_month: u32,
    to_month: u32,
    #[serde(serialize_with = "serialize_display")]
    currency: Currency,
    lines: Vec<BudgetVarianceJson<'a>>,
    totals: BudgetVarianceTotals,
}

impl Serialize for crate::db::BudgetVarianceReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use super::amounts::MoneyField;

        let months = self.months();
        let lines = self
            .rows()
            .iter()
            .map(|row| BudgetVarianceJson {
                code: &row.code,
                name: &row.name,
                account_type: row.account_type.code(),
                budget: MoneyField::new("budget", row.budget_amount),
                actual: MoneyField::new("actual", row.actual_amount),
                variance: MoneyField::new("variance", row.variance_amount),
                variance_percent: row
                    .variance_percent
                    .and_then(|percent| percent.to_number(1)),
                favorable: row.favorable,
            })
            .collect();
        BudgetVarianceData {
            year: months.year(),
            from_month: months.from().number_from_month(),
            to_month: months.to().number_from_month(),
            currency: self.currency(),
            lines,
            totals: BudgetVarianceTotals {
                budget: MoneyField::new("budget", self.total_budget()),
                actual: MoneyField::new("actual", self.total_actual()),
            },
        }
        .serialize(serializer)
    }
}

/// Render a budget deletion confirmation as an enveloped JSON response.
///
/// # Errors
///
/// Returns [`CliError`] on serialisation failure.
pub fn render_budget_deleted(count: usize, meta: Meta) -> Result<String, CliError> {
    #[derive(Serialize)]
    struct BudgetDeletedJson {
        deleted: usize,
    }
    envelope_ok(meta, BudgetDeletedJson { deleted: count })
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(command: &str, company: Option<&str>) -> Meta {
        meta_with_timestamp(command, company, "2025-01-01T00:00:00Z".to_string())
    }

    #[test]
    fn render_companies_empty() {
        let result = render_companies(&[], test_meta("company.list", None));
        assert!(result.is_ok());
        let json = result.unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""command": "company.list""#));
        assert!(json.contains(r#""data": []"#));
    }

    #[test]
    fn render_companies_single() {
        let rows = vec![CompanyRow {
            slug: "acme".into(),
            name: "Acme Corp".into(),
            description: None,
            created_at: "2025-01-01T00:00:00".into(),
        }];
        let json = render_companies(&rows, test_meta("company.show", None)).unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""slug": "acme""#));
        assert!(json.contains(r#""name": "Acme Corp""#));
    }

    #[test]
    fn render_accounts_normal_balance() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let json =
            render_accounts(&rows, test_meta("account.list", Some("acme"))).unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""company": "acme""#));
        assert!(json.contains(r#""type": "asset""#));
        assert!(json.contains(r#""normal_balance": "debit""#));
    }

    #[test]
    fn render_accounts_revenue_is_credit() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "4000".into(),
            name: "Revenue".into(),
            account_type: "revenue".into(),
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let json =
            render_accounts(&rows, test_meta("account.list", Some("acme"))).unwrap_or_default();
        assert!(json.contains(r#""normal_balance": "credit""#));
    }

    #[test]
    fn render_transactions_with_entries() {
        let txns = vec![TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Sale".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
            posted_by: None,
            agent_uri: None,
            token_jti: None,
            reference: None,
        }];
        let mut map = HashMap::new();
        map.insert(
            1,
            vec![
                EntryRow {
                    id: 1,
                    transaction_id: 1,
                    account_code: "1000".into(),
                    company_slug: "acme".into(),
                    direction: "debit".into(),
                    amount: 5000,
                    memo: None,
                    tax_category: None,
                    status: EntryStatus::Uncleared,
                },
                EntryRow {
                    id: 2,
                    transaction_id: 1,
                    account_code: "4000".into(),
                    company_slug: "acme".into(),
                    direction: "credit".into(),
                    amount: 5000,
                    memo: None,
                    tax_category: None,
                    status: EntryStatus::Uncleared,
                },
            ],
        );
        let json = render_transactions(&txns, &map, test_meta("txn.list", Some("acme")))
            .unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""description": "Sale""#));
        assert!(json.contains(r#""account_code": "1000""#));
        assert!(json.contains(r#""direction": "debit""#));
        assert!(json.contains(r#""amount": 5000"#));
    }

    #[test]
    fn render_transactions_missing_entries() {
        let txns = vec![TransactionRow {
            id: 99,
            company_slug: "acme".into(),
            description: "Orphan".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15T10:00:00".into(),
            posted_by: None,
            agent_uri: None,
            token_jti: None,
            reference: None,
        }];
        let json = render_transactions(&txns, &HashMap::new(), test_meta("txn.list", Some("acme")))
            .unwrap_or_default();
        assert!(json.contains(r#""entries": []"#));
    }

    #[test]
    fn render_trial_balance_balanced() {
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
        let json = render_trial_balance(&rows, test_meta("report.trial-balance", Some("acme")))
            .unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""balanced": true"#));
        assert!(json.contains(r#""total_debits": 10000"#));
        assert!(json.contains(r#""total_credits": 10000"#));
    }

    #[test]
    fn render_trial_balance_unbalanced() {
        let rows = vec![BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 10000,
            credit_total: 5000,
        }];
        let json = render_trial_balance(&rows, test_meta("report.trial-balance", Some("acme")))
            .unwrap_or_default();
        assert!(json.contains(r#""balanced": false"#));
    }

    #[test]
    fn render_account_balance_json() {
        let row = BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 15000,
            credit_total: 5000,
        };
        let json = render_account_balance(&row, "USD", test_meta("report.balance", Some("acme")))
            .unwrap_or_default();
        assert!(json.contains(r#""ok": true"#));
        assert!(json.contains(r#""code": "1000""#));
        assert!(json.contains(r#""currency": "USD""#));
        assert!(json.contains(r#""debit_total": 15000"#));
    }

    #[test]
    fn normal_balance_for_all_types() {
        assert_eq!(normal_balance_for("asset"), "debit");
        assert_eq!(normal_balance_for("expense"), "debit");
        assert_eq!(normal_balance_for("liability"), "credit");
        assert_eq!(normal_balance_for("equity"), "credit");
        assert_eq!(normal_balance_for("revenue"), "credit");
        assert_eq!(normal_balance_for("unknown_type"), "unknown");
    }

    #[test]
    fn envelope_structure_success() -> Result<(), Box<dyn std::error::Error>> {
        let json = render_companies(&[], test_meta("company.list", None))?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        assert_eq!(v["ok"], true);
        assert_eq!(v["meta"]["command"], "company.list");
        assert_eq!(v["meta"]["timestamp"], "2025-01-01T00:00:00Z");
        assert!(v["meta"]["company"].is_null());
        assert!(v["data"].is_array());
        assert!(v.get("error").is_none());
        Ok(())
    }

    #[test]
    fn envelope_structure_company_field() -> Result<(), Box<dyn std::error::Error>> {
        let json = render_accounts(&[], test_meta("account.list", Some("acme")))?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        assert_eq!(v["meta"]["company"], "acme");
        Ok(())
    }

    #[test]
    fn meta_with_timestamp_is_deterministic() {
        let m = meta_with_timestamp("test", None, "2025-06-15T12:00:00Z".to_string());
        assert_eq!(m.timestamp, "2025-06-15T12:00:00Z");
        assert_eq!(m.command, "test");
        assert!(m.company.is_none());
    }

    #[test]
    fn error_code_returns_correct_strings() {
        assert_eq!(CliError::Usage("x".into()).error_code(), "USAGE");
        assert_eq!(CliError::Validation("x".into()).error_code(), "VALIDATION");
        assert_eq!(CliError::Database("x".into()).error_code(), "DATABASE");
        assert_eq!(CliError::NotFound("x".into()).error_code(), "NOT_FOUND");
        assert_eq!(CliError::General("x".into()).error_code(), "GENERAL");
        assert_eq!(
            CliError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "x")).error_code(),
            "IO"
        );
    }
}
