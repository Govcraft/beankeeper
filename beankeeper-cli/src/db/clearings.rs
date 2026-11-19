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
//! # Dates
//!
//! A clearing is never dated before its transaction. A reconciled clearing
//! also carries a `reconciled_date` (`>= cleared_date`): the day it was
//! locked in. A clearing created directly as reconciled has
//! `cleared_date == reconciled_date`; promoting a cleared clearing sets only
//! its `reconciled_date`. This lets reports answer "what was cleared, and
//! what was reconciled, as of a date".
//!
//! # Locking
//!
//! Reconciled clearings are locked: a `cleared` request never downgrades
//! them. Asking to clear a reconciled clearing (by its statement, or a fully
//! reconciled entry without a statement) is a validation error; the way back
//! is `--status uncleared`, then clearing again.
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
    /// Day the clearing was reconciled; `Some` exactly when `status` is
    /// [`ClearingStatus::Reconciled`].
    pub reconciled_date: Option<String>,
    pub statement_ref: Option<String>,
    pub status: ClearingStatus,
}

/// The `YYYY-MM-DD` day of a stored date (tolerates a trailing time).
fn day(s: &str) -> &str {
    s.get(..10).unwrap_or(s)
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
    /// `YYYY-MM-DD` date of the request: the `cleared_date` of a new clearing
    /// and, with status reconciled, the `reconciled_date` of every clearing it
    /// creates or promotes.
    pub date: &'a str,
    /// Date of the entry's transaction; no clearing may predate it.
    pub txn_date: &'a str,
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
/// 0. The request date must not be before the transaction date.
/// 1. With a statement that already cleared this entry (idempotency hit):
///    an explicit amount must equal the recorded one (else a validation
///    error); `--status reconciled` promotes that clearing if it is only
///    cleared; `--status cleared` on a reconciled clearing is an error
///    (reconciled clearings are locked); otherwise skip.
/// 2. Otherwise the amount defaults to the remaining amount. An explicit
///    amount must be positive and must not exceed the remaining amount
///    (over-clearing is a validation error).
/// 3. Reconciling in full (no amount, no statement, status reconciled) also
///    promotes every existing cleared clearing, so the entry ends up
///    reconciled -- the pre-#25 `txn clear --status reconciled` semantics.
/// 4. `--status cleared` on a fully cleared entry whose clearings are all
///    reconciled is an error (locked); with some still only cleared it is a
///    skip, like any other fully cleared entry.
/// 5. Nothing remaining and nothing to promote is a skip.
/// 6. A promoted clearing must not be cleared after the reconciled date.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for non-positive amounts, over-clearing,
/// a conflicting amount for an already-applied statement, a date before the
/// transaction, a reconciled date before a promoted clearing's cleared date,
/// or a `cleared` request on locked (reconciled) clearings.
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
    if day(spec.date) < day(spec.txn_date) {
        let what = if spec.status == ClearingStatus::Reconciled {
            "reconciled"
        } else {
            "clearing"
        };
        return Err(CliError::Validation(format!(
            "{what} date {} is before the transaction date {}",
            spec.date,
            day(spec.txn_date)
        )));
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
            return match (spec.status, prior.status) {
                (ClearingStatus::Reconciled, ClearingStatus::Cleared) => {
                    check_promotable(prior, spec.date)?;
                    Ok(ClearPlan::Promote {
                        ids: vec![prior.id],
                    })
                }
                (ClearingStatus::Cleared, ClearingStatus::Reconciled) => {
                    Err(locked_error(prior.entry_id, Some(stmt)))
                }
                _ => Ok(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement)),
            };
        }
    }

    let remaining = entry_amount - cleared_sum(existing);
    if spec.status == ClearingStatus::Cleared && remaining <= 0 {
        if let Some(first) = existing.first() {
            if existing
                .iter()
                .all(|c| c.status == ClearingStatus::Reconciled)
            {
                return Err(locked_error(first.entry_id, None));
            }
        }
    }
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
        let mut ids = Vec::new();
        for c in existing
            .iter()
            .filter(|c| c.status == ClearingStatus::Cleared)
        {
            check_promotable(c, spec.date)?;
            ids.push(c.id);
        }
        ids
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

/// A clearing can only be reconciled on or after its cleared date.
fn check_promotable(c: &Clearing, reconciled_date: &str) -> Result<(), CliError> {
    if day(reconciled_date) < day(&c.cleared_date) {
        return Err(CliError::Validation(format!(
            "reconciled date {reconciled_date} is before the cleared date {} of clearing {} \
             (entry {}); reconcile on or after the cleared date",
            c.cleared_date, c.id, c.entry_id
        )));
    }
    Ok(())
}

/// The error for a `cleared` request on reconciled (locked) clearings.
fn locked_error(entry_id: i64, statement: Option<&str>) -> CliError {
    let (what, flag) = match statement {
        Some(s) => (
            format!("entry {entry_id} is reconciled by statement '{s}'"),
            format!(" --statement {s}"),
        ),
        None => (format!("entry {entry_id} is reconciled"), String::new()),
    };
    CliError::Validation(format!(
        "{what}; reconciled clearings are locked and cannot be downgraded to cleared. \
         To redo it, remove the clearing with `--status uncleared{flag}`, then clear it again"
    ))
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
        "SELECT id, entry_id, amount, cleared_date, statement_ref, status, reconciled_date \
         FROM entry_clearings WHERE entry_id = ?1 ORDER BY cleared_date, id",
    )?;
    let rows = stmt.query_map(params![entry_id], |row| {
        Ok((
            Clearing {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                amount: row.get(2)?,
                cleared_date: row.get(3)?,
                reconciled_date: row.get(6)?,
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
            reconciled_date: (status == ClearingStatus::Reconciled).then(|| "2026-01-01".into()),
            statement_ref: stmt.map(str::to_string),
            status,
        }
    }

    const TXN_DATE: &str = "2026-01-01";
    const DATE: &str = "2026-01-31";

    fn spec(amount: Option<i64>, statement: Option<&str>) -> ClearSpec<'_> {
        ClearSpec {
            amount,
            status: ClearingStatus::Cleared,
            statement,
            date: DATE,
            txn_date: TXN_DATE,
        }
    }

    fn reconcile<'a>(statement: Option<&'a str>, date: &'a str) -> ClearSpec<'a> {
        ClearSpec {
            amount: None,
            status: ClearingStatus::Reconciled,
            statement,
            date,
            txn_date: TXN_DATE,
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
        assert_eq!(
            plan_clear(100, &existing, &reconcile(Some("S1"), DATE)).ok(),
            Some(ClearPlan::Promote { ids: vec![7] })
        );
    }

    #[test]
    fn reconcile_in_full_promotes_existing() {
        let reconcile = reconcile(None, DATE);
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
    fn date_before_transaction_is_rejected() {
        let mut early = spec(None, None);
        early.date = "2025-12-31";
        let err = plan_clear(100, &[], &early);
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("before the transaction date")),
            "{err:?}"
        );
        // Same day is fine; a stored date with a time is compared by day.
        let mut same = spec(None, None);
        same.date = TXN_DATE;
        same.txn_date = "2026-01-01 09:00:00";
        assert!(plan_clear(100, &[], &same).is_ok());
        // The rule applies to the reconciled date too.
        let err = plan_clear(100, &[], &reconcile(None, "2025-12-31"));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.starts_with("reconciled date")),
            "{err:?}"
        );
    }

    #[test]
    fn reconciled_date_must_not_precede_cleared_date() {
        let mut feb = c(7, 100, Some("S1"), Cleared);
        feb.cleared_date = "2026-02-15".into();
        let existing = [feb];
        // Promote by statement and promote in full both check the date.
        for spec in [
            reconcile(Some("S1"), "2026-02-14"),
            reconcile(None, "2026-02-14"),
        ] {
            let err = plan_clear(100, &existing, &spec);
            assert!(
                matches!(&err, Err(CliError::Validation(m)) if m.contains("before the cleared date")),
                "{err:?}"
            );
        }
        assert_eq!(
            plan_clear(100, &existing, &reconcile(Some("S1"), "2026-02-15")).ok(),
            Some(ClearPlan::Promote { ids: vec![7] })
        );
    }

    #[test]
    fn cleared_request_on_reconciled_clearings_is_locked() {
        // By statement.
        let existing = [c(7, 30, Some("S1"), Reconciled)];
        let err = plan_clear(100, &existing, &spec(None, Some("S1")));
        assert!(
            matches!(&err, Err(CliError::Validation(m))
                if m.contains("locked") && m.contains("--status uncleared --statement S1")),
            "{err:?}"
        );
        // Fully reconciled entry, no statement.
        let full = [c(1, 40, None, Reconciled), c(2, 60, Some("S2"), Reconciled)];
        let err = plan_clear(100, &full, &spec(None, None));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("locked")),
            "{err:?}"
        );
        let err = plan_clear(100, &full, &spec(Some(1), Some("S3")));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("locked")),
            "{err:?}"
        );
        // A partially reconciled entry can still clear its remainder.
        assert_eq!(
            plan_clear(100, &existing, &spec(None, None)).ok(),
            Some(ClearPlan::Insert {
                amount: 70,
                promote: vec![]
            })
        );
        // Mixed, fully covered: still a plain skip.
        let mixed = [c(1, 40, None, Reconciled), c(2, 60, None, Cleared)];
        assert_eq!(
            plan_clear(100, &mixed, &spec(None, None)).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
        // Reconciling an already reconciled statement is still a skip.
        assert_eq!(
            plan_clear(100, &existing, &reconcile(Some("S1"), DATE)).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
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
