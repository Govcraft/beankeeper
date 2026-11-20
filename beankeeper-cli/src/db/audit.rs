//! Append-only audit trail and the sole audited mutation API.
//!
//! Every in-place mutation or deletion in the ledger flows through this
//! module. Each mutating function records an immutable `audit_log` row in the
//! *same* savepoint as the change itself, so a clearance-status flip, an
//! intercompany correlation, a budget revision, or an account/company deletion
//! can never happen without leaving a trace (issue #16).
//!
//! The un-audited variants of these operations intentionally do not exist:
//! the only way to mutate is to call one of the functions here, and every one
//! of them requires an [`Actor`] and writes the trail. This is what makes
//! "mutate without recording" a compile error rather than a runtime hazard --
//! the wrong call simply has no symbol to bind to. All `UPDATE`, `DELETE`, and
//! `INSERT OR REPLACE` statements in the crate live in this file; a regression
//! test enforces that they appear nowhere else.

use std::str::FromStr;

use rusqlite::{Connection, params};
use serde::Serialize;

use super::clearings::{
    ClearPlan, ClearSpec, Clearing, ClearingStatus, SkipReason, cleared_sum, derive_status,
    get_entry_target, list_clearings, plan_clear, plan_unclear,
};
use super::metadata;
use super::{AccountRow, BudgetRow, CompanyRow, SetAnnualBudgetParams, SetBudgetParams};
use crate::error::CliError;

// ---------------------------------------------------------------------------
// Typed clearance status -- illegal states are unrepresentable
// ---------------------------------------------------------------------------

/// Clearance status of a ledger entry.
///
/// Replaces the previously stringly-typed `status` so that every transition is
/// total and the set of legal states is fixed by the type, not by a runtime
/// string check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    /// Not yet matched against a statement.
    Uncleared,
    /// Matched against a statement but not part of a closed reconciliation.
    Cleared,
    /// Locked in by a completed reconciliation.
    Reconciled,
}

impl EntryStatus {
    /// Returns the lowercase string representation stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncleared => "uncleared",
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
        }
    }
}

impl FromStr for EntryStatus {
    type Err = CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "uncleared" => Ok(Self::Uncleared),
            "cleared" => Ok(Self::Cleared),
            "reconciled" => Ok(Self::Reconciled),
            other => Err(CliError::Validation(format!(
                "invalid entry status '{other}'"
            ))),
        }
    }
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Actor -- the principal recorded against every change
// ---------------------------------------------------------------------------

/// The principal responsible for a mutation, recorded in every audit row.
///
/// Resolved from, in order: an explicit `--actor` flag, the `BK_ACTOR`
/// environment variable (set by the Emergent exec-handler), the operating
/// system user (`USER` / `USERNAME`), or the fallback `"cli"`.
#[derive(Debug, Clone)]
pub struct Actor(String);

impl Actor {
    /// Constructs an actor with an explicit name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Resolves the effective actor from an optional explicit name, then the
    /// environment, falling back to `"cli"`.
    #[must_use]
    pub fn resolve(explicit: Option<&str>) -> Self {
        if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return Self(name.to_string());
        }

        for var in ["BK_ACTOR", "USER", "USERNAME"] {
            if let Ok(val) = std::env::var(var) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Self(trimmed.to_string());
                }
            }
        }

        Self("cli".to_string())
    }

    /// Returns the actor's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Audit row classification
// ---------------------------------------------------------------------------

/// The kind of entity an audit row concerns. Maps to the `entity` CHECK
/// constraint on `audit_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditEntity {
    Entry,
    Transaction,
    Budget,
    Account,
    Company,
}

impl AuditEntity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Transaction => "transaction",
            Self::Budget => "budget",
            Self::Account => "account",
            Self::Company => "company",
        }
    }
}

/// The kind of change recorded. Maps to the `action` CHECK constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditAction {
    StatusChange,
    Correlate,
    Reverse,
    BudgetSet,
    BudgetDelete,
    AccountDelete,
    CompanyDelete,
}

impl AuditAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::StatusChange => "status_change",
            Self::Correlate => "correlate",
            Self::Reverse => "reverse",
            Self::BudgetSet => "budget_set",
            Self::BudgetDelete => "budget_delete",
            Self::AccountDelete => "account_delete",
            Self::CompanyDelete => "company_delete",
        }
    }
}

/// A single recorded change, returned by [`list_audit`].
#[derive(Debug, Clone)]
pub struct AuditLogRow {
    pub id: i64,
    pub changed_at: String,
    pub actor: String,
    pub company_slug: Option<String>,
    pub entity: String,
    pub entity_id: String,
    pub action: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

// ---------------------------------------------------------------------------
// Atomic change-plus-record primitives
// ---------------------------------------------------------------------------

/// Runs `f` inside a named savepoint, releasing on success and rolling back on
/// error. The savepoint pairs the data change and its audit row into one
/// atomic unit: either both land or neither does.
fn in_savepoint<T>(
    conn: &Connection,
    name: &'static str,
    f: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(value)
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

/// Appends one immutable row to `audit_log`. Private: callers reach it only
/// through the audited mutation functions below, never directly.
#[allow(clippy::too_many_arguments)]
fn record(
    conn: &Connection,
    actor: &Actor,
    company_slug: Option<&str>,
    entity: AuditEntity,
    entity_id: &str,
    action: AuditAction,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), CliError> {
    conn.execute(
        "INSERT INTO audit_log \
         (actor, company_slug, entity, entity_id, action, before, after) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            actor.name(),
            company_slug,
            entity.as_str(),
            entity_id,
            action.as_str(),
            before,
            after
        ],
    )?;
    Ok(())
}

/// Serializes a snapshot value to a JSON string for the `before`/`after`
/// columns.
fn snapshot<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

// ---------------------------------------------------------------------------
// Audited mutations -- the only way to change persisted ledger state
// ---------------------------------------------------------------------------

/// Sets the clearance status of an entry in full, recording the transition.
///
/// A convenience over [`clear_entry`] / [`unclear_entry`]: `cleared` and
/// `reconciled` clear the full remaining amount as of today (reconciled also
/// promotes existing clearings), `uncleared` removes every clearing.
///
/// Returns the previous status.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the entry does not belong to the given
/// transaction/company, or `CliError::Sqlite` on database errors.
pub fn set_entry_status(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    transaction_id: i64,
    entry_id: i64,
    to: EntryStatus,
) -> Result<EntryStatus, CliError> {
    let status = match to {
        EntryStatus::Uncleared => {
            return unclear_entry(conn, actor, company_slug, transaction_id, entry_id, None)
                .map(|o| o.previous_status);
        }
        EntryStatus::Cleared => ClearingStatus::Cleared,
        EntryStatus::Reconciled => ClearingStatus::Reconciled,
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    clear_entry(
        conn,
        actor,
        &ClearRequest {
            company_slug,
            transaction_id,
            entry_id,
            amount: None,
            date: &today,
            statement: None,
            status,
        },
    )
    .map(|o| o.previous_status)
}

/// A request to clear (part of) one entry.
#[derive(Debug, Clone, Copy)]
pub struct ClearRequest<'a> {
    pub company_slug: &'a str,
    pub transaction_id: i64,
    pub entry_id: i64,
    /// Minor units; `None` clears the full remaining amount.
    pub amount: Option<i64>,
    /// `YYYY-MM-DD`: the cleared date of a new clearing and, with status
    /// reconciled, the reconciled date of every clearing created or promoted.
    /// Must not be before the transaction date.
    pub date: &'a str,
    pub statement: Option<&'a str>,
    pub status: ClearingStatus,
}

/// What a clear/unclear call did to an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClearAction {
    /// A new clearing was recorded.
    Cleared,
    /// Existing clearings were promoted to reconciled (possibly alongside a
    /// new reconciled clearing).
    Reconciled,
    /// Clearings were removed.
    Uncleared,
    /// Nothing changed.
    Skipped,
}

impl ClearAction {
    /// Returns the lowercase string used in output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
            Self::Uncleared => "uncleared",
            Self::Skipped => "skipped",
        }
    }
}

/// The result of [`clear_entry`] / [`unclear_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
    pub transaction_id: i64,
    pub entry_id: i64,
    pub action: ClearAction,
    /// Id of the clearing inserted by this call, if any.
    pub clearing_id: Option<i64>,
    /// Amount newly cleared (or removed, for `uncleared`) by this call.
    pub amount: i64,
    /// Entry amount.
    pub entry_amount: i64,
    /// Total cleared amount after the call.
    pub cleared_amount: i64,
    /// `entry_amount - cleared_amount`.
    pub remaining_amount: i64,
    pub previous_status: EntryStatus,
    pub entry_status: EntryStatus,
    pub skip_reason: Option<SkipReason>,
}

/// Runs `f` atomically: every audited mutation it performs lands, or none
/// does. Used by batch clearing (`txn clear --from-file`).
///
/// # Errors
///
/// Propagates the first error from `f` after rolling everything back.
pub fn atomically<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    in_savepoint(conn, "sp_atomic_batch", f)
}

/// Re-derives and stores `entries.status`, returning `(cleared, status)`.
fn sync_entry_status(
    conn: &Connection,
    entry_id: i64,
    entry_amount: i64,
) -> Result<(Vec<Clearing>, EntryStatus), CliError> {
    let after = list_clearings(conn, entry_id)?;
    let status = derive_status(entry_amount, &after);
    conn.execute(
        "UPDATE entries SET status = ?1 WHERE id = ?2",
        params![status.as_str(), entry_id],
    )?;
    Ok((after, status))
}

fn clearing_json(c: &Clearing) -> serde_json::Value {
    serde_json::json!({
        "id": c.id,
        "amount": c.amount,
        "cleared_date": c.cleared_date,
        "reconciled_date": c.reconciled_date,
        "statement": c.statement_ref,
        "status": c.status.as_str(),
    })
}

/// Records one `status_change` audit row for an entry's clearing change.
///
/// Payload: `before`/`after` carry `status` (the derived entry status, as
/// before #25) and `cleared_amount`; `after` adds `op` (`clear`,
/// `reconcile`, `unclear`) and `clearings` (the affected clearing rows).
#[allow(clippy::too_many_arguments)]
fn record_clearing_change(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    entry_id: i64,
    op: &str,
    before: (EntryStatus, i64),
    after: (EntryStatus, i64),
    affected: &[Clearing],
) -> Result<(), CliError> {
    record(
        conn,
        actor,
        Some(company_slug),
        AuditEntity::Entry,
        &entry_id.to_string(),
        AuditAction::StatusChange,
        Some(&snapshot(&serde_json::json!({
            "status": before.0.as_str(),
            "cleared_amount": before.1,
        }))),
        Some(&snapshot(&serde_json::json!({
            "status": after.0.as_str(),
            "cleared_amount": after.1,
            "op": op,
            "clearings": affected.iter().map(clearing_json).collect::<Vec<_>>(),
        }))),
    )
}

/// Clears (part of) an entry, keeping `entries.status` in sync and recording
/// the change. See [`super::clearings::plan_clear`] for the rules; a skip
/// changes nothing and records nothing.
///
/// # Errors
///
/// Returns `CliError::NotFound` for an unknown entry, `CliError::Validation`
/// for over-clearing, a conflicting statement amount, a date before the
/// transaction or before a promoted clearing's cleared date, or a `cleared`
/// request on reconciled (locked) clearings, or `CliError::Sqlite` on
/// database errors.
pub fn clear_entry(
    conn: &Connection,
    actor: &Actor,
    req: &ClearRequest<'_>,
) -> Result<ClearOutcome, CliError> {
    in_savepoint(conn, "sp_entry_clear", || {
        let target = get_entry_target(conn, req.company_slug, req.transaction_id, req.entry_id)?;
        let existing = list_clearings(conn, req.entry_id)?;
        let before_sum = cleared_sum(&existing);
        let before_status = derive_status(target.amount, &existing);
        let plan = plan_clear(
            target.amount,
            &existing,
            &ClearSpec {
                amount: req.amount,
                status: req.status,
                statement: req.statement,
                date: req.date,
                txn_date: &target.date,
            },
        )?;

        let (action, inserted, amount, promote) = match plan {
            ClearPlan::Skip(reason) => {
                return Ok(ClearOutcome {
                    transaction_id: req.transaction_id,
                    entry_id: req.entry_id,
                    action: ClearAction::Skipped,
                    clearing_id: None,
                    amount: 0,
                    entry_amount: target.amount,
                    cleared_amount: before_sum,
                    remaining_amount: target.amount - before_sum,
                    previous_status: before_status,
                    entry_status: before_status,
                    skip_reason: Some(reason),
                });
            }
            ClearPlan::Insert { amount, promote } => {
                conn.execute(
                    "INSERT INTO entry_clearings \
                     (entry_id, company_slug, amount, cleared_date, statement_ref, status, \
                      reconciled_date) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        req.entry_id,
                        req.company_slug,
                        amount,
                        req.date,
                        req.statement,
                        req.status.as_str(),
                        (req.status == ClearingStatus::Reconciled).then_some(req.date),
                    ],
                )?;
                let action = if promote.is_empty() {
                    ClearAction::Cleared
                } else {
                    ClearAction::Reconciled
                };
                (action, Some(conn.last_insert_rowid()), amount, promote)
            }
            ClearPlan::Promote { ids } => (ClearAction::Reconciled, None, 0, ids),
        };

        for id in &promote {
            conn.execute(
                "UPDATE entry_clearings SET status = 'reconciled', reconciled_date = ?2 \
                 WHERE id = ?1",
                params![id, req.date],
            )?;
        }

        let (after, status) = sync_entry_status(conn, req.entry_id, target.amount)?;
        let after_sum = cleared_sum(&after);
        let affected: Vec<Clearing> = after
            .into_iter()
            .filter(|c| Some(c.id) == inserted || promote.contains(&c.id))
            .collect();
        let op = if action == ClearAction::Reconciled {
            "reconcile"
        } else {
            "clear"
        };
        record_clearing_change(
            conn,
            actor,
            req.company_slug,
            req.entry_id,
            op,
            (before_status, before_sum),
            (status, after_sum),
            &affected,
        )?;

        Ok(ClearOutcome {
            transaction_id: req.transaction_id,
            entry_id: req.entry_id,
            action,
            clearing_id: inserted,
            amount,
            entry_amount: target.amount,
            cleared_amount: after_sum,
            remaining_amount: target.amount - after_sum,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    })
}

/// Removes an entry's clearings (all, or only `statement`'s), keeping
/// `entries.status` in sync and recording the removed rows.
///
/// # Errors
///
/// Returns `CliError::NotFound` for an unknown entry or `CliError::Sqlite`
/// on database errors.
pub fn unclear_entry(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    transaction_id: i64,
    entry_id: i64,
    statement: Option<&str>,
) -> Result<ClearOutcome, CliError> {
    in_savepoint(conn, "sp_entry_unclear", || {
        let target = get_entry_target(conn, company_slug, transaction_id, entry_id)?;
        let existing = list_clearings(conn, entry_id)?;
        let before_sum = cleared_sum(&existing);
        let before_status = derive_status(target.amount, &existing);
        let ids = plan_unclear(&existing, statement);

        if ids.is_empty() {
            return Ok(ClearOutcome {
                transaction_id,
                entry_id,
                action: ClearAction::Skipped,
                clearing_id: None,
                amount: 0,
                entry_amount: target.amount,
                cleared_amount: before_sum,
                remaining_amount: target.amount - before_sum,
                previous_status: before_status,
                entry_status: before_status,
                skip_reason: Some(SkipReason::NothingToUnclear),
            });
        }

        let removed: Vec<Clearing> = existing
            .into_iter()
            .filter(|c| ids.contains(&c.id))
            .collect();
        for id in &ids {
            conn.execute("DELETE FROM entry_clearings WHERE id = ?1", params![id])?;
        }
        let (after, status) = sync_entry_status(conn, entry_id, target.amount)?;
        let after_sum = cleared_sum(&after);
        record_clearing_change(
            conn,
            actor,
            company_slug,
            entry_id,
            "unclear",
            (before_status, before_sum),
            (status, after_sum),
            &removed,
        )?;

        Ok(ClearOutcome {
            transaction_id,
            entry_id,
            action: ClearAction::Uncleared,
            clearing_id: None,
            amount: cleared_sum(&removed),
            entry_amount: target.amount,
            cleared_amount: after_sum,
            remaining_amount: target.amount - after_sum,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    })
}

/// Correlates a newly posted transaction with an existing partner transaction
/// in another company, updating the partner's metadata and recording the link.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the partner does not exist,
/// `CliError::Validation` if it belongs to the same company or is already
/// correlated, or `CliError::Sqlite` on database errors.
pub fn correlate_partner(
    conn: &Connection,
    actor: &Actor,
    new_company: &str,
    new_txn_id: i64,
    partner_id: i64,
) -> Result<(), CliError> {
    in_savepoint(conn, "sp_correlate", || {
        let (partner_company, partner_metadata): (String, Option<String>) = conn
            .query_row(
                "SELECT company_slug, metadata FROM transactions WHERE id = ?1",
                params![partner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| CliError::NotFound(format!("transaction #{partner_id} not found")))?;

        if partner_company == new_company {
            return Err(CliError::Validation(format!(
                "cannot correlate with transaction #{partner_id}: it belongs to the same company '{new_company}'"
            )));
        }

        // Same rule as the reconcile query: only a valid JSON object with a
        // non-null `correlate` key counts. A memo that merely mentions the
        // word "correlate" does not block linking (issue #17).
        if metadata::has_correlate(partner_metadata.as_deref()) {
            return Err(CliError::Validation(format!(
                "transaction #{partner_id} is already correlated"
            )));
        }

        // Normalizes legacy free-text metadata to `{"memo": ...}` and splices
        // in the correlate key, so the partner's metadata stays a JSON object.
        let updated = metadata::with_correlate(partner_metadata.as_deref(), new_txn_id)?;

        conn.execute(
            "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
            params![updated, partner_id],
        )?;

        record(
            conn,
            actor,
            Some(partner_company.as_str()),
            AuditEntity::Transaction,
            &partner_id.to_string(),
            AuditAction::Correlate,
            Some(&snapshot(
                &serde_json::json!({ "metadata": partner_metadata }),
            )),
            Some(&snapshot(&serde_json::json!({
                "metadata": updated,
                "correlated_with": new_txn_id,
            }))),
        )?;

        Ok(())
    })
}

/// Inputs for an atomic, audited reversal of a transaction in one company.
pub struct ReverseTransactionParams<'a> {
    pub company_slug: &'a str,
    pub id: i64,
    pub date: &'a str,
    pub description: Option<&'a str>,
    /// An explicit reference also permits retrying the same reversal.
    pub reference: Option<&'a str>,
    pub actor: &'a Actor,
}

/// Creates an opposite posting, links both transactions, and records the action.
///
/// Existing entries and clearings stay intact; new entries start uncleared.
/// Only the reversal link is copied into the new transaction's metadata.
///
/// # Errors
///
/// Rejects missing/already reversed originals, reconciled clearings, invalid
/// dates or postings, duplicate references, and database failures. Every write
/// is rolled back together on failure.
pub fn reverse_transaction(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
) -> Result<super::transactions::PostResult, CliError> {
    use super::transactions::{
        PostResult, PostTransactionParams, get_transaction, post_transaction,
    };

    in_savepoint(conn, "sp_reverse", || {
        let (original, entries) = get_transaction(conn, p.company_slug, p.id)?;
        let mut original_metadata: serde_json::Value = original
            .metadata
            .as_deref()
            .and_then(metadata::normalize)
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(result) = existing_reversal(conn, p, &original_metadata)? {
            return Ok(result);
        }
        let date = validate_reversal(conn, p, &original.date)?;
        let description = p.description.map_or_else(
            || format!("Reversal of #{}: {}", p.id, original.description),
            str::to_string,
        );
        let entries = reversal_entries(conn, &original, &entries, date)?;
        let reference = p
            .reference
            .map_or_else(|| format!("reversal-of-{}", p.id), str::to_string);
        let reversal_metadata = serde_json::json!({"reverses": p.id}).to_string();
        let result = post_transaction(
            conn,
            &PostTransactionParams {
                company_slug: p.company_slug,
                description: &description,
                metadata: Some(&reversal_metadata),
                currency: &original.currency,
                date: &date.to_string(),
                entries: &entries,
                correlate: None,
                reference: Some(&reference),
                on_conflict: super::transactions::ConflictStrategy::Error,
                actor: p.actor,
            },
        )?;
        let PostResult::Created(reversal_id) = result else {
            return Err(CliError::General(
                "reversal was unexpectedly skipped".into(),
            ));
        };
        original_metadata["reversed_by"] = serde_json::json!(reversal_id);
        let updated = original_metadata.to_string();
        conn.execute(
            "UPDATE transactions SET metadata = ?1 WHERE id = ?2 AND company_slug = ?3",
            params![updated, p.id, p.company_slug],
        )?;
        record(
            conn,
            p.actor,
            Some(p.company_slug),
            AuditEntity::Transaction,
            &p.id.to_string(),
            AuditAction::Reverse,
            Some(&snapshot(
                &serde_json::json!({"metadata": original.metadata}),
            )),
            Some(&snapshot(
                &serde_json::json!({"metadata": updated, "reversed_by": reversal_id, "reference": reference}),
            )),
        )?;
        Ok(result)
    })
}

/// Only a matching explicit reference can reuse the existing linked reversal.
fn existing_reversal(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
    original_metadata: &serde_json::Value,
) -> Result<Option<super::transactions::PostResult>, CliError> {
    use super::transactions::{PostResult, get_transaction};
    if let Some(reversal_id) = original_metadata
        .get("reversed_by")
        .filter(|v| !v.is_null())
    {
        if let (Some(id), Some(reference)) = (reversal_id.as_i64(), p.reference) {
            let (reversal, _) = get_transaction(conn, p.company_slug, id)?;
            let reverse_meta: Option<serde_json::Value> = reversal
                .metadata
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            if reversal.reference.as_deref() == Some(reference)
                && reverse_meta
                    .as_ref()
                    .and_then(|v| v.get("reverses"))
                    .and_then(serde_json::Value::as_i64)
                    == Some(p.id)
            {
                return Ok(Some(PostResult::Skipped(id)));
            }
        }
        return Err(CliError::Validation(format!(
            "transaction #{} is already reversed by #{reversal_id}",
            p.id
        )));
    }
    Ok(None)
}

/// Check dates and individual clearings, including partial reconciliations.
fn validate_reversal(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
    original_date: &str,
) -> Result<chrono::NaiveDate, CliError> {
    let date = chrono::NaiveDate::parse_from_str(p.date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid date '{}': {e}", p.date)))?;
    let original_date = chrono::NaiveDate::parse_from_str(original_date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid original date: {e}")))?;
    if date < original_date {
        return Err(CliError::Validation(
            "reversal date must be on or after the original transaction date".into(),
        ));
    }
    let reconciled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries e LEFT JOIN entry_clearings c ON c.entry_id = e.id \
             WHERE e.transaction_id = ?1 AND (e.status = 'reconciled' OR c.status = 'reconciled'))",
        params![p.id],
        |row| row.get(0),
    )?;
    if reconciled {
        return Err(CliError::Validation(format!(
            "transaction #{} has reconciled clearings and cannot be reversed",
            p.id
        )));
    }
    Ok(date)
}

/// Build mirror entries and validate the entire posting with the library.
fn reversal_entries(
    conn: &Connection,
    original: &super::TransactionRow,
    entries: &[super::EntryRow],
    date: chrono::NaiveDate,
) -> Result<Vec<super::transactions::PostEntryParams>, CliError> {
    use beankeeper::types::{Currency, DebitOrCredit, Entry, Money};
    let currency = Currency::from_code(&original.currency)
        .map_err(|e| CliError::Validation(format!("invalid currency: {e}")))?;
    let mut journal = beankeeper::core::JournalEntry::new(date, "Reversal");
    let mut reversed = Vec::with_capacity(entries.len());
    for entry in entries {
        let (direction, direction_name) = match entry.direction.as_str() {
            "debit" => (DebitOrCredit::Credit, "credit"),
            "credit" => (DebitOrCredit::Debit, "debit"),
            other => {
                return Err(CliError::Validation(format!(
                    "invalid entry direction '{other}'"
                )));
            }
        };
        let row = super::accounts::get_account(conn, &original.company_slug, &entry.account_code)?;
        journal = journal.entry(Entry::new(
            super::accounts::row_to_account(&row)?,
            Money::from_minor(i128::from(entry.amount), currency),
            direction,
        )?);
        reversed.push(super::transactions::PostEntryParams {
            account_code: entry.account_code.clone(),
            direction: direction_name.into(),
            amount: entry.amount,
            memo: entry.memo.clone(),
            tax_category: entry.tax_category.clone(),
        });
    }
    journal.post()?;
    Ok(reversed)
}

/// Upserts a single month's budget, recording the prior amount (if any).
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_budget(
    conn: &Connection,
    actor: &Actor,
    p: &SetBudgetParams<'_>,
) -> Result<BudgetRow, CliError> {
    in_savepoint(conn, "sp_budget_set", || {
        let prior_amount: Option<i64> = conn
            .query_row(
                "SELECT amount FROM budgets \
                 WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 \
                 AND year = ?4 AND month = ?5",
                params![p.company_slug, p.account_code, p.currency, p.year, p.month],
                |row| row.get(0),
            )
            .ok();

        conn.execute(
            "INSERT OR REPLACE INTO budgets \
             (company_slug, account_code, currency, year, month, amount, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.company_slug,
                p.account_code,
                p.currency,
                p.year,
                p.month,
                p.amount,
                p.notes
            ],
        )?;

        let id = conn.last_insert_rowid();
        let created_at: String = conn.query_row(
            "SELECT created_at FROM budgets WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;

        let before = prior_amount.map(|amount| snapshot(&serde_json::json!({ "amount": amount })));
        record(
            conn,
            actor,
            Some(p.company_slug),
            AuditEntity::Budget,
            &id.to_string(),
            AuditAction::BudgetSet,
            before.as_deref(),
            Some(&snapshot(&serde_json::json!({
                "amount": p.amount,
                "notes": p.notes,
                "year": p.year,
                "month": p.month,
            }))),
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
    })
}

/// Distributes an annual budget evenly across 12 months and upserts each,
/// recording every month's change. The whole distribution is atomic.
///
/// Each month gets `annual_amount / 12` minor units, with the first
/// `annual_amount % 12` months receiving an extra 1 unit so the total is exact.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_annual_budget(
    conn: &Connection,
    actor: &Actor,
    params: &SetAnnualBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    in_savepoint(conn, "sp_annual_budget", || {
        let base = params.annual_amount / 12;
        let remainder = params.annual_amount % 12;
        let mut rows = Vec::with_capacity(12);

        for m in 1..=12 {
            let extra = i64::from(i64::from(m) <= remainder);
            let row = set_budget(
                conn,
                actor,
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
    })
}

/// Deletes budget rows for an account, recording a snapshot of each removed
/// row. If `month` is `None`, deletes all months for that account/year/currency.
///
/// Returns the number of rows deleted.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
#[allow(clippy::too_many_arguments)]
pub fn delete_budget(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<i32>,
) -> Result<usize, CliError> {
    in_savepoint(conn, "sp_budget_delete", || {
        // Snapshot the rows about to be removed.
        let victims =
            collect_budget_victims(conn, company_slug, account_code, currency, year, month)?;

        let count = if let Some(m) = month {
            conn.execute(
                "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 \
                 AND currency = ?3 AND year = ?4 AND month = ?5",
                params![company_slug, account_code, currency, year, m],
            )?
        } else {
            conn.execute(
                "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 \
                 AND currency = ?3 AND year = ?4",
                params![company_slug, account_code, currency, year],
            )?
        };

        for v in &victims {
            record(
                conn,
                actor,
                Some(company_slug),
                AuditEntity::Budget,
                &v.id.to_string(),
                AuditAction::BudgetDelete,
                Some(&snapshot(&serde_json::json!({
                    "year": year,
                    "month": v.month,
                    "amount": v.amount,
                    "notes": v.notes,
                }))),
                None,
            )?;
        }

        Ok(count)
    })
}

/// A budget row captured before deletion.
struct BudgetVictim {
    id: i64,
    month: i32,
    amount: i64,
    notes: Option<String>,
}

fn collect_budget_victims(
    conn: &Connection,
    company_slug: &str,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<i32>,
) -> Result<Vec<BudgetVictim>, CliError> {
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(BudgetVictim {
            id: row.get(0)?,
            month: row.get(1)?,
            amount: row.get(2)?,
            notes: row.get(3)?,
        })
    };

    let rows: Vec<BudgetVictim> = if let Some(m) = month {
        let mut stmt = conn.prepare(
            "SELECT id, month, amount, notes FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 \
             AND year = ?4 AND month = ?5",
        )?;
        let mapped = stmt.query_map(
            params![company_slug, account_code, currency, year, m],
            map_row,
        )?;
        mapped.collect::<Result<_, _>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, month, amount, notes FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 AND year = ?4",
        )?;
        let mapped =
            stmt.query_map(params![company_slug, account_code, currency, year], map_row)?;
        mapped.collect::<Result<_, _>>()?
    };

    Ok(rows)
}

/// Deletes an account by company slug and code, recording a snapshot.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the account does not exist,
/// `CliError::Validation` if ledger data still references it, or
/// `CliError::Sqlite` on database errors.
pub fn delete_account(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    code: &str,
) -> Result<(), CliError> {
    in_savepoint(conn, "sp_account_delete", || {
        // Reading the row first both yields the snapshot and produces the
        // `NotFound` error when the account is absent.
        let row: AccountRow = super::accounts::get_account(conn, company_slug, code)?;

        let (entries, budgets, attachments): (i64, i64, i64) = conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM entries WHERE company_slug = ?1 AND account_code = ?2), \
                (SELECT COUNT(*) FROM budgets WHERE company_slug = ?1 AND account_code = ?2), \
                (SELECT COUNT(*) FROM attachments a JOIN entries e ON e.id = a.entry_id \
                    WHERE e.company_slug = ?1 AND e.account_code = ?2)",
            params![company_slug, code],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if entries > 0 || budgets > 0 || attachments > 0 {
            return Err(CliError::Validation(format!(
                "account {code} cannot be deleted: posted entries={entries}, \
                 budget entries={budgets}, attachments={attachments} (ledger is append-only)"
            )));
        }

        conn.execute(
            "DELETE FROM accounts WHERE company_slug = ?1 AND code = ?2",
            params![company_slug, code],
        )?;

        record(
            conn,
            actor,
            Some(company_slug),
            AuditEntity::Account,
            code,
            AuditAction::AccountDelete,
            Some(&snapshot(&serde_json::json!({
                "code": row.code,
                "name": row.name,
                "type": row.account_type,
                "created_at": row.created_at,
                "default_tax_category": row.default_tax_category,
            }))),
            None,
        )?;

        Ok(())
    })
}

/// Deletes a company by slug, recording a snapshot.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the company does not exist,
/// `CliError::Validation` if ledger data still references it, or
/// `CliError::Sqlite` on database errors.
pub fn delete_company(conn: &Connection, actor: &Actor, slug: &str) -> Result<(), CliError> {
    in_savepoint(conn, "sp_company_delete", || {
        let row: CompanyRow = super::companies::get_company(conn, slug)?;

        let dependencies: (i64, i64, i64, i64, i64) = conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM accounts WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM transactions WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM entries WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM budgets WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM attachments WHERE company_slug = ?1)",
            params![slug],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let (accounts, transactions, entries, budgets, attachments) = dependencies;
        if accounts > 0 || transactions > 0 || entries > 0 || budgets > 0 || attachments > 0 {
            return Err(CliError::Validation(format!(
                "company '{slug}' cannot be deleted: accounts={accounts}, \
                 transactions={transactions}, posted entries={entries}, \
                 budget entries={budgets}, attachments={attachments} (ledger is append-only)"
            )));
        }

        conn.execute("DELETE FROM companies WHERE slug = ?1", params![slug])?;

        record(
            conn,
            actor,
            Some(slug),
            AuditEntity::Company,
            slug,
            AuditAction::CompanyDelete,
            Some(&snapshot(&serde_json::json!({
                "slug": row.slug,
                "name": row.name,
                "description": row.description,
                "created_at": row.created_at,
            }))),
            None,
        )?;

        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Reading the trail
// ---------------------------------------------------------------------------

/// Filters for [`list_audit`].
pub struct ListAuditParams<'a> {
    pub company_slug: Option<&'a str>,
    pub entity: Option<&'a str>,
    pub action: Option<&'a str>,
    pub limit: i64,
}

/// Lists audit-log rows most-recent-first, with optional filters.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_audit(
    conn: &Connection,
    params: &ListAuditParams<'_>,
) -> Result<Vec<AuditLogRow>, CliError> {
    use std::fmt::Write as _;

    let mut sql = String::from(
        "SELECT id, changed_at, actor, company_slug, entity, entity_id, action, before, after \
         FROM audit_log WHERE 1 = 1",
    );

    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1u32;

    if let Some(c) = params.company_slug {
        let _ = write!(sql, " AND company_slug = ?{idx}");
        values.push(Box::new(c.to_string()));
        idx += 1;
    }
    if let Some(e) = params.entity {
        let _ = write!(sql, " AND entity = ?{idx}");
        values.push(Box::new(e.to_string()));
        idx += 1;
    }
    if let Some(a) = params.action {
        let _ = write!(sql, " AND action = ?{idx}");
        values.push(Box::new(a.to_string()));
        idx += 1;
    }

    let _ = write!(sql, " ORDER BY id DESC LIMIT ?{idx}");
    values.push(Box::new(params.limit));

    let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(AuditLogRow {
            id: row.get(0)?,
            changed_at: row.get(1)?,
            actor: row.get(2)?,
            company_slug: row.get(3)?,
            entity: row.get(4)?,
            entity_id: row.get(5)?,
            action: row.get(6)?,
            before: row.get(7)?,
            after: row.get(8)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn seed() -> Db {
        let db = Db::open_in_memory().expect("open in-memory db");
        let conn = db.conn();
        conn.execute(
            "INSERT INTO companies (slug, name) VALUES ('acme', 'Acme')",
            [],
        )
        .expect("insert company");
        conn.execute(
            "INSERT INTO accounts (company_slug, code, name, type) \
             VALUES ('acme', '1000', 'Cash', 'asset')",
            [],
        )
        .expect("insert account");
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date) \
             VALUES ('acme', 'Opening', 'USD', '2026-01-01')",
            [],
        )
        .expect("insert txn");
        conn.execute(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount, status) \
             VALUES (1, '1000', 'acme', 'debit', 100, 'uncleared')",
            [],
        )
        .expect("insert entry");
        db
    }

    #[test]
    fn entry_status_parses_round_trip() {
        for s in [
            EntryStatus::Uncleared,
            EntryStatus::Cleared,
            EntryStatus::Reconciled,
        ] {
            assert_eq!(EntryStatus::from_str(s.as_str()).ok(), Some(s));
        }
        assert!(EntryStatus::from_str("bogus").is_err());
    }

    #[test]
    fn actor_resolves_explicit_over_env() {
        let actor = Actor::resolve(Some("emergent"));
        assert_eq!(actor.name(), "emergent");
    }

    #[test]
    fn actor_falls_back_to_cli() {
        // An empty explicit name is ignored; env may or may not be set, but the
        // resolved name is never empty.
        let actor = Actor::resolve(Some("   "));
        assert!(!actor.name().is_empty());
    }

    #[test]
    fn set_entry_status_records_transition() {
        let db = seed();
        let actor = Actor::new("tester");
        let prev = set_entry_status(db.conn(), &actor, "acme", 1, 1, EntryStatus::Cleared)
            .expect("clear entry");
        assert_eq!(prev, EntryStatus::Uncleared);

        let status: String = db
            .conn()
            .query_row("SELECT status FROM entries WHERE id = 1", [], |r| r.get(0))
            .expect("read status");
        assert_eq!(status, "cleared");

        let (actor_col, action, before, after): (String, String, Option<String>, Option<String>) =
            db.conn()
                .query_row(
                    "SELECT actor, action, before, after FROM audit_log \
                 WHERE entity = 'entry' AND entity_id = '1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .expect("read audit row");
        assert_eq!(actor_col, "tester");
        assert_eq!(action, "status_change");
        assert!(before.unwrap_or_default().contains("uncleared"));
        assert!(after.unwrap_or_default().contains("cleared"));
    }

    #[test]
    fn set_entry_status_unknown_entry_errors_and_records_nothing() {
        let db = seed();
        let actor = Actor::new("tester");
        let result = set_entry_status(db.conn(), &actor, "acme", 1, 999, EntryStatus::Cleared);
        assert!(result.is_err());

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .expect("count audit");
        assert_eq!(count, 0, "failed mutation must leave no audit row");
    }

    #[test]
    fn delete_account_snapshots_before_removal() {
        let db = seed();
        let actor = Actor::new("tester");
        // Remove the dependent entry first so the FK delete succeeds.
        db.conn()
            .execute("DELETE FROM entries WHERE company_slug = 'acme'", [])
            .expect("clear entries");

        delete_account(db.conn(), &actor, "acme", "1000").expect("delete account");

        let (action, before): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT action, before FROM audit_log WHERE entity = 'account'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read audit");
        assert_eq!(action, "account_delete");
        assert!(before.unwrap_or_default().contains("Cash"));
    }

    fn req<'a>(amount: Option<i64>, date: &'a str, stmt: Option<&'a str>) -> ClearRequest<'a> {
        ClearRequest {
            company_slug: "acme",
            transaction_id: 1,
            entry_id: 1,
            amount,
            date,
            statement: stmt,
            status: ClearingStatus::Cleared,
        }
    }

    fn entry_status(db: &Db) -> String {
        db.conn()
            .query_row("SELECT status FROM entries WHERE id = 1", [], |r| r.get(0))
            .expect("read status")
    }

    fn audit_count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .expect("count audit")
    }

    #[test]
    fn partial_clears_keep_status_in_sync_and_audit() {
        let db = seed();
        let actor = Actor::new("tester");

        let o = clear_entry(db.conn(), &actor, &req(Some(30), "2026-01-31", Some("S1")))
            .expect("partial clear");
        assert_eq!(o.action, ClearAction::Cleared);
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (30, 30, 70)
        );
        assert_eq!(o.entry_status, EntryStatus::Uncleared);
        assert_eq!(entry_status(&db), "uncleared");

        let o = clear_entry(db.conn(), &actor, &req(None, "2026-02-28", Some("S2")))
            .expect("clear remainder");
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (70, 100, 0)
        );
        assert_eq!(entry_status(&db), "cleared");

        // Re-running S1 is a skip and leaves no audit row.
        let o = clear_entry(db.conn(), &actor, &req(Some(30), "2026-01-31", Some("S1")))
            .expect("rerun");
        assert_eq!(o.action, ClearAction::Skipped);
        assert_eq!(audit_count(&db), 2);

        let after: String = db
            .conn()
            .query_row(
                "SELECT after FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("read audit");
        let v: serde_json::Value = serde_json::from_str(&after).expect("json");
        assert_eq!(v["status"], "cleared");
        assert_eq!(v["cleared_amount"], 100);
        assert_eq!(v["op"], "clear");
        assert_eq!(v["clearings"][0]["statement"], "S2");
        assert_eq!(v["clearings"][0]["amount"], 70);

        // Reconcile in full promotes both clearings.
        let mut r = req(None, "2026-03-01", None);
        r.status = ClearingStatus::Reconciled;
        let o = clear_entry(db.conn(), &actor, &r).expect("reconcile");
        assert_eq!(o.action, ClearAction::Reconciled);
        assert_eq!(entry_status(&db), "reconciled");

        // Unclear one statement drops back to uncleared.
        let o = unclear_entry(db.conn(), &actor, "acme", 1, 1, Some("S2")).expect("unclear");
        assert_eq!((o.amount, o.cleared_amount), (70, 30));
        assert_eq!(entry_status(&db), "uncleared");
    }

    #[test]
    fn over_clear_fails_without_side_effects() {
        let db = seed();
        let actor = Actor::new("tester");
        clear_entry(db.conn(), &actor, &req(Some(60), "2026-01-31", None)).expect("clear 60");
        let err = clear_entry(db.conn(), &actor, &req(Some(41), "2026-01-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))));
        assert_eq!(audit_count(&db), 1);
    }

    #[test]
    fn atomically_rolls_back_the_whole_batch() {
        let db = seed();
        let actor = Actor::new("tester");
        let result = atomically(db.conn(), || {
            clear_entry(db.conn(), &actor, &req(Some(50), "2026-01-31", Some("S1")))?;
            clear_entry(db.conn(), &actor, &req(Some(51), "2026-01-31", Some("S2")))
        });
        assert!(result.is_err());
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM entry_clearings", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0);
        assert_eq!(audit_count(&db), 0);
    }

    #[test]
    fn reconciled_dates_are_stored_and_audited() {
        let db = seed();
        let actor = Actor::new("tester");
        clear_entry(db.conn(), &actor, &req(Some(40), "2026-01-31", Some("S1"))).expect("clear S1");

        // Promote S1 in February: cleared_date stays, reconciled_date is set.
        let mut r = req(None, "2026-02-10", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let o = clear_entry(db.conn(), &actor, &r).expect("promote S1");
        assert_eq!(o.action, ClearAction::Reconciled);
        let rows = list_clearings(db.conn(), 1).expect("list");
        assert_eq!(rows[0].cleared_date, "2026-01-31");
        assert_eq!(rows[0].reconciled_date.as_deref(), Some("2026-02-10"));

        let after: String = db
            .conn()
            .query_row(
                "SELECT after FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("read audit");
        let v: serde_json::Value = serde_json::from_str(&after).expect("json");
        assert_eq!(v["op"], "reconcile");
        assert_eq!(v["clearings"][0]["reconciled_date"], "2026-02-10");

        // A clearing created directly as reconciled gets both dates.
        let mut r = req(Some(60), "2026-03-01", Some("S2"));
        r.status = ClearingStatus::Reconciled;
        clear_entry(db.conn(), &actor, &r).expect("reconcile S2");
        let rows = list_clearings(db.conn(), 1).expect("list");
        assert_eq!(rows[1].cleared_date, "2026-03-01");
        assert_eq!(rows[1].reconciled_date.as_deref(), Some("2026-03-01"));
        assert_eq!(entry_status(&db), "reconciled");

        // Locked: a cleared request on the reconciled entry fails, no audit.
        let n = audit_count(&db);
        let err = clear_entry(db.conn(), &actor, &req(None, "2026-03-02", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");
        assert_eq!(audit_count(&db), n);
    }

    #[test]
    fn invalid_dates_fail_without_side_effects() {
        let db = seed();
        let actor = Actor::new("tester");
        // Transaction is dated 2026-01-01.
        let err = clear_entry(db.conn(), &actor, &req(None, "2025-12-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");

        clear_entry(db.conn(), &actor, &req(None, "2026-01-31", Some("S1"))).expect("clear");
        let mut r = req(None, "2026-01-30", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let err = clear_entry(db.conn(), &actor, &r);
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");
        assert_eq!(audit_count(&db), 1);
        assert_eq!(entry_status(&db), "cleared");
    }

    #[test]
    fn schema_rejects_inconsistent_reconciled_dates() {
        let db = seed();
        let insert = |status: &str, cleared: &str, reconciled: Option<&str>| {
            db.conn().execute(
                "INSERT INTO entry_clearings \
                 (entry_id, company_slug, amount, cleared_date, status, reconciled_date) \
                 VALUES (1, 'acme', 10, ?1, ?2, ?3)",
                params![cleared, status, reconciled],
            )
        };
        assert!(insert("reconciled", "2026-01-31", None).is_err());
        assert!(insert("reconciled", "2026-01-31", Some("2026-01-30")).is_err());
        assert!(insert("cleared", "2026-01-31", Some("2026-02-01")).is_err());
        assert!(insert("reconciled", "2026-01-31", Some("2026-01-31")).is_ok());
        assert!(insert("cleared", "2026-01-31", None).is_ok());
    }

    #[test]
    fn correlate_partner_ignores_word_correlate_in_memo() {
        let db = seed();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO companies (slug, name) VALUES ('beta', 'Beta')",
            [],
        )
        .expect("insert company");
        // Legacy free-text metadata that mentions the word "correlate".
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date, metadata) \
             VALUES ('beta', 'Partner', 'USD', '2026-01-01', 'please \"correlate\" later')",
            [],
        )
        .expect("insert partner");
        let partner_id = conn.last_insert_rowid();

        let actor = Actor::new("tester");
        correlate_partner(conn, &actor, "acme", 1, partner_id).expect("correlate");

        let meta: String = conn
            .query_row(
                "SELECT metadata FROM transactions WHERE id = ?1",
                params![partner_id],
                |r| r.get(0),
            )
            .expect("read metadata");
        let value: serde_json::Value = serde_json::from_str(&meta).expect("metadata is JSON");
        assert_eq!(value["correlate"], serde_json::json!(1));
        assert_eq!(
            value["memo"],
            serde_json::json!("please \"correlate\" later")
        );

        // Now it really is correlated, so a second link is refused.
        assert!(correlate_partner(conn, &actor, "acme", 1, partner_id).is_err());
    }

    #[test]
    fn delete_account_missing_errors() {
        let db = seed();
        let actor = Actor::new("tester");
        assert!(delete_account(db.conn(), &actor, "acme", "9999").is_err());
    }
}
