//! Partial clearings of ledger entries (issue #25): the pure planning logic
//! and the read-only queries. Every write lives in [`super::audit`] so each
//! clearing change is recorded in the audit trail.
//!
//! # Model
//!
//! An entry carries zero or more rows in `entry_clearings`, each clearing
//! part of the entry's amount on a `cleared_date`, optionally on behalf of a
//! statement (`statement_ref`), with status `cleared` or `reconciled`. The sum
//! of an entry's clearings never exceeds the entry amount.
//!
//! `entries.status` is a derived summary kept in sync on every change
//! ([`derive_status`]):
//!
//! - `reconciled` when the clearings cover the full amount and all of them are
//!   reconciled;
//! - `cleared` when the clearings cover the full amount (at least one is only
//!   cleared);
//! - `uncleared` otherwise, including partially cleared entries (the
//!   reconciliation report shows the cleared and remaining amounts).
//!
//! # Idempotency
//!
//! The idempotency key is `(entry, statement)`: an entry has at most one
//! clearing per statement (a partial unique index enforces it). Clearing the
//! same entry for the same statement again is skipped, or rejected when an
//! explicit amount disagrees with the recorded one. Clearings without a
//! statement have no key: each call adds a clearing, but clearing the "full
//! remaining amount" of an already fully cleared entry is a skip.

use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::audit::EntryStatus;
use crate::error::CliError;

/// Status of a single clearing row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClearingStatus {
    /// Matched against a statement.
    Cleared,
    /// Locked in by a completed reconciliation.
    Reconciled,
}

impl ClearingStatus {
    /// Returns the lowercase string stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
        }
    }
}

impl FromStr for ClearingStatus {
    type Err = CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cleared" => Ok(Self::Cleared),
            "reconciled" => Ok(Self::Reconciled),
            other => Err(CliError::Validation(format!(
                "invalid clearing status '{other}'"
            ))),
        }
    }
}

/// One persisted clearing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clearing {
    pub id: i64,
    pub entry_id: i64,
    pub amount: i64,
    pub cleared_date: String,
    pub statement_ref: Option<String>,
    pub status: ClearingStatus,
}

/// Total cleared amount of a set of clearings.
#[must_use]
pub fn cleared_sum(clearings: &[Clearing]) -> i64 {
    clearings.iter().map(|c| c.amount).sum()
}

/// Derives the entry-level summary status from its clearings.
#[must_use]
pub fn derive_status(entry_amount: i64, clearings: &[Clearing]) -> EntryStatus {
    if clearings.is_empty() || cleared_sum(clearings) < entry_amount {
        EntryStatus::Uncleared
    } else if clearings
        .iter()
        .all(|c| c.status == ClearingStatus::Reconciled)
    {
        EntryStatus::Reconciled
    } else {
        EntryStatus::Cleared
    }
}

/// What a clear request asks for.
#[derive(Debug, Clone, Copy)]
pub struct ClearSpec<'a> {
    /// Amount to clear in minor units; `None` means "the full remaining
    /// amount".
    pub amount: Option<i64>,
    pub status: ClearingStatus,
    pub statement: Option<&'a str>,
}

/// Why a clear request changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The entry already has a clearing for this statement.
    AlreadyClearedByStatement,
    /// Nothing remains to clear.
    FullyCleared,
    /// There are no matching clearings to remove.
    NothingToUnclear,
}

impl SkipReason {
    /// Returns the snake-case string used in output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyClearedByStatement => "already_cleared_by_statement",
            Self::FullyCleared => "fully_cleared",
            Self::NothingToUnclear => "nothing_to_unclear",
        }
    }
}

/// The change a clear request resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearPlan {
    /// Insert a new clearing of `amount`, then promote `promote` (clearing
    /// ids) to reconciled.
    Insert { amount: i64, promote: Vec<i64> },
    /// Promote existing clearings to reconciled; insert nothing.
    Promote { ids: Vec<i64> },
    /// Change nothing.
    Skip(SkipReason),
}

/// Plans a clear request against an entry's existing clearings.
///
/// Rules:
///
/// 1. With a statement that already cleared this entry (idempotency hit):
///    an explicit amount must equal the recorded one (else a validation
///    error); `--status reconciled` promotes that clearing if it is only
///    cleared; otherwise skip.
/// 2. Otherwise the amount defaults to the remaining amount. An explicit
///    amount must be positive and must not exceed the remaining amount
///    (over-clearing is a validation error).
/// 3. Reconciling in full (no amount, no statement, status reconciled) also
///    promotes every existing cleared clearing, so the entry ends up
///    reconciled -- the pre-#25 `txn clear --status reconciled` semantics.
/// 4. Nothing remaining and nothing to promote is a skip.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for non-positive amounts, over-clearing,
/// or a conflicting amount for an already-applied statement.
pub fn plan_clear(
    entry_amount: i64,
    existing: &[Clearing],
    spec: &ClearSpec<'_>,
) -> Result<ClearPlan, CliError> {
    if let Some(a) = spec.amount {
        if a <= 0 {
            return Err(CliError::Validation(
                "clearing amount must be positive".to_string(),
            ));
        }
    }

    if let Some(stmt) = spec.statement {
        if let Some(prior) = existing
            .iter()
            .find(|c| c.statement_ref.as_deref() == Some(stmt))
        {
            if let Some(a) = spec.amount {
                if a != prior.amount {
                    return Err(CliError::Validation(format!(
                        "entry {} was already cleared for {} minor units by statement '{stmt}'; \
                         refusing to record a different amount ({a})",
                        prior.entry_id, prior.amount
                    )));
                }
            }
            if spec.status == ClearingStatus::Reconciled && prior.status == ClearingStatus::Cleared
            {
                return Ok(ClearPlan::Promote {
                    ids: vec![prior.id],
                });
            }
            return Ok(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement));
        }
    }

    let remaining = entry_amount - cleared_sum(existing);
    let amount = spec.amount.unwrap_or(remaining);
    if amount > remaining {
        return Err(CliError::Validation(format!(
            "over-clearing: requested {amount} minor units but only {remaining} of {entry_amount} remain uncleared"
        )));
    }

    let promote: Vec<i64> = if spec.status == ClearingStatus::Reconciled
        && spec.amount.is_none()
        && spec.statement.is_none()
    {
        existing
            .iter()
            .filter(|c| c.status == ClearingStatus::Cleared)
            .map(|c| c.id)
            .collect()
    } else {
        Vec::new()
    };

    if amount > 0 {
        Ok(ClearPlan::Insert { amount, promote })
    } else if promote.is_empty() {
        Ok(ClearPlan::Skip(SkipReason::FullyCleared))
    } else {
        Ok(ClearPlan::Promote { ids: promote })
    }
}

/// Plans an "unclear": the ids of the clearings to remove (all of the
/// entry's clearings, or only the given statement's).
#[must_use]
pub fn plan_unclear(existing: &[Clearing], statement: Option<&str>) -> Vec<i64> {
    existing
        .iter()
        .filter(|c| statement.is_none_or(|s| c.statement_ref.as_deref() == Some(s)))
        .map(|c| c.id)
        .collect()
}

// ---------------------------------------------------------------------------
// Read-only queries
// ---------------------------------------------------------------------------

/// Lists an entry's clearings, oldest first.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_clearings(conn: &Connection, entry_id: i64) -> Result<Vec<Clearing>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, entry_id, amount, cleared_date, statement_ref, status \
         FROM entry_clearings WHERE entry_id = ?1 ORDER BY cleared_date, id",
    )?;
    let rows = stmt.query_map(params![entry_id], |row| {
        Ok((
            Clearing {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                amount: row.get(2)?,
                cleared_date: row.get(3)?,
                statement_ref: row.get(4)?,
                status: ClearingStatus::Cleared,
            },
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (mut c, status) = row?;
        c.status = ClearingStatus::from_str(&status)?;
        out.push(c);
    }
    Ok(out)
}

/// An entry resolved for clearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryTarget {
    pub transaction_id: i64,
    pub entry_id: i64,
    pub account_code: String,
    pub amount: i64,
    pub currency: String,
    pub reference: Option<String>,
    pub date: String,
}

const TARGET_SELECT: &str = "SELECT t.id, e.id, e.account_code, e.amount, t.currency, \
     t.reference, t.date \
     FROM entries e JOIN transactions t ON t.id = e.transaction_id";

fn map_target(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryTarget> {
    Ok(EntryTarget {
        transaction_id: row.get(0)?,
        entry_id: row.get(1)?,
        account_code: row.get(2)?,
        amount: row.get(3)?,
        currency: row.get(4)?,
        reference: row.get(5)?,
        date: row.get(6)?,
    })
}

/// Resolves an entry by transaction id and entry id.
///
/// # Errors
///
/// Returns `CliError::NotFound` when the entry does not belong to the
/// transaction in this company.
pub fn get_entry_target(
    conn: &Connection,
    company: &str,
    transaction_id: i64,
    entry_id: i64,
) -> Result<EntryTarget, CliError> {
    conn.query_row(
        &format!("{TARGET_SELECT} WHERE t.company_slug = ?1 AND t.id = ?2 AND e.id = ?3"),
        params![company, transaction_id, entry_id],
        map_target,
    )
    .optional()?
    .ok_or_else(|| {
        CliError::NotFound(format!(
            "entry {entry_id} not found in transaction {transaction_id} for company '{company}'"
        ))
    })
}

/// Resolves an entry by transaction reference and account code.
///
/// # Errors
///
/// Returns `CliError::NotFound` when no transaction has the reference or it
/// has no entry on the account, and `CliError::Validation` when it has more
/// than one entry on the account.
pub fn find_entry_by_reference(
    conn: &Connection,
    company: &str,
    reference: &str,
    account: &str,
) -> Result<EntryTarget, CliError> {
    let txn_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
            params![company, reference],
            |row| row.get(0),
        )
        .optional()?;
    let Some(txn_id) = txn_id else {
        return Err(CliError::NotFound(format!(
            "no transaction with reference '{reference}' in company '{company}'"
        )));
    };

    let mut stmt = conn.prepare(&format!(
        "{TARGET_SELECT} WHERE t.id = ?1 AND e.account_code = ?2 ORDER BY e.id"
    ))?;
    let mut found = stmt
        .query_map(params![txn_id, account], map_target)?
        .collect::<Result<Vec<_>, _>>()?;
    match found.len() {
        0 => Err(CliError::NotFound(format!(
            "transaction #{txn_id} (reference '{reference}') has no entry on account {account}"
        ))),
        1 => Ok(found.remove(0)),
        n => Err(CliError::Validation(format!(
            "ambiguous: transaction #{txn_id} (reference '{reference}') has {n} entries on account \
             {account}; clear one with `txn clear {txn_id} --entry <id>`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: i64, amount: i64, stmt: Option<&str>, status: ClearingStatus) -> Clearing {
        Clearing {
            id,
            entry_id: 1,
            amount,
            cleared_date: "2026-01-01".into(),
            statement_ref: stmt.map(str::to_string),
            status,
        }
    }

    fn spec(amount: Option<i64>, statement: Option<&str>) -> ClearSpec<'_> {
        ClearSpec {
            amount,
            status: ClearingStatus::Cleared,
            statement,
        }
    }

    use ClearingStatus::{Cleared, Reconciled};

    #[test]
    fn derive_status_rules() {
        assert_eq!(derive_status(100, &[]), EntryStatus::Uncleared);
        assert_eq!(
            derive_status(100, &[c(1, 40, None, Cleared)]),
            EntryStatus::Uncleared
        );
        assert_eq!(
            derive_status(100, &[c(1, 40, None, Cleared), c(2, 60, None, Reconciled)]),
            EntryStatus::Cleared
        );
        assert_eq!(
            derive_status(
                100,
                &[c(1, 40, None, Reconciled), c(2, 60, None, Reconciled)]
            ),
            EntryStatus::Reconciled
        );
    }

    #[test]
    fn default_clears_full_remaining() {
        assert_eq!(
            plan_clear(100, &[], &spec(None, None)).ok(),
            Some(ClearPlan::Insert {
                amount: 100,
                promote: vec![]
            })
        );
        assert_eq!(
            plan_clear(100, &[c(1, 30, Some("S1"), Cleared)], &spec(None, None)).ok(),
            Some(ClearPlan::Insert {
                amount: 70,
                promote: vec![]
            })
        );
    }

    #[test]
    fn partial_clears_accumulate_until_full() {
        let existing = [c(1, 30, Some("S1"), Cleared)];
        assert_eq!(
            plan_clear(100, &existing, &spec(Some(70), Some("S2"))).ok(),
            Some(ClearPlan::Insert {
                amount: 70,
                promote: vec![]
            })
        );
        let full = [c(1, 30, Some("S1"), Cleared), c(2, 70, Some("S2"), Cleared)];
        assert_eq!(
            plan_clear(100, &full, &spec(None, None)).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
    }

    #[test]
    fn over_clearing_is_rejected() {
        let existing = [c(1, 60, None, Cleared)];
        assert!(matches!(
            plan_clear(100, &existing, &spec(Some(41), None)),
            Err(CliError::Validation(_))
        ));
        assert!(plan_clear(100, &existing, &spec(Some(40), None)).is_ok());
        assert!(matches!(
            plan_clear(100, &[], &spec(Some(0), None)),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn same_statement_is_idempotent() {
        let existing = [c(7, 30, Some("S1"), Cleared)];
        assert_eq!(
            plan_clear(100, &existing, &spec(Some(30), Some("S1"))).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
        );
        assert_eq!(
            plan_clear(100, &existing, &spec(None, Some("S1"))).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
        );
        // A different amount for the same statement is a conflict.
        assert!(matches!(
            plan_clear(100, &existing, &spec(Some(31), Some("S1"))),
            Err(CliError::Validation(_))
        ));
        // Reconciling a statement promotes its clearing.
        let reconcile = ClearSpec {
            amount: None,
            status: Reconciled,
            statement: Some("S1"),
        };
        assert_eq!(
            plan_clear(100, &existing, &reconcile).ok(),
            Some(ClearPlan::Promote { ids: vec![7] })
        );
    }

    #[test]
    fn reconcile_in_full_promotes_existing() {
        let reconcile = ClearSpec {
            amount: None,
            status: Reconciled,
            statement: None,
        };
        let partial = [c(1, 40, None, Cleared)];
        assert_eq!(
            plan_clear(100, &partial, &reconcile).ok(),
            Some(ClearPlan::Insert {
                amount: 60,
                promote: vec![1]
            })
        );
        let full = [c(1, 100, None, Cleared)];
        assert_eq!(
            plan_clear(100, &full, &reconcile).ok(),
            Some(ClearPlan::Promote { ids: vec![1] })
        );
        let done = [c(1, 100, None, Reconciled)];
        assert_eq!(
            plan_clear(100, &done, &reconcile).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
    }

    #[test]
    fn unclear_selects_by_statement() {
        let existing = [c(1, 30, Some("S1"), Cleared), c(2, 20, None, Cleared)];
        assert_eq!(plan_unclear(&existing, None), vec![1, 2]);
        assert_eq!(plan_unclear(&existing, Some("S1")), vec![1]);
        assert!(plan_unclear(&existing, Some("S9")).is_empty());
    }
}
