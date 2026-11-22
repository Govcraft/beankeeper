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

use beankeeper::types::{Amount, Currency};
use chrono::NaiveDate;
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use super::amounts::PositiveMinorUnits;
use super::audit::EntryStatus;
use super::companies::Company;
use crate::db::ids::{ClearingId, EntryId, TransactionId};
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

    /// Status of a new clearing recorded on this day.
    #[must_use]
    pub const fn on(self, date: NaiveDate) -> ClearingState {
        match self {
            Self::Cleared => ClearingState::Cleared,
            Self::Reconciled => ClearingState::Reconciled { on: date },
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

/// A clearing's state. Only a reconciled clearing has a reconciliation date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearingState {
    /// Matched against a statement.
    Cleared,
    /// Locked in by a completed reconciliation.
    Reconciled {
        /// The date the clearing was locked.
        on: NaiveDate,
    },
}

impl ClearingState {
    /// The status label, separate from its associated date.
    #[must_use]
    pub const fn status(self) -> ClearingStatus {
        match self {
            Self::Cleared => ClearingStatus::Cleared,
            Self::Reconciled { .. } => ClearingStatus::Reconciled,
        }
    }

    /// The reconciliation date, when one exists.
    #[must_use]
    pub const fn reconciled_on(self) -> Option<NaiveDate> {
        match self {
            Self::Cleared => None,
            Self::Reconciled { on } => Some(on),
        }
    }
}

impl Serialize for ClearingState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut record = serializer.serialize_struct("ClearingState", 2)?;
        record.serialize_field("reconciled_date", &self.reconciled_on())?;
        record.serialize_field("status", &self.status())?;
        record.end()
    }
}

/// One persisted clearing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clearing {
    pub id: ClearingId,
    pub entry_id: EntryId,
    pub amount: PositiveMinorUnits,
    cleared_date: NaiveDate,
    pub statement_ref: Option<String>,
    #[serde(flatten)]
    state: ClearingState,
}

impl Clearing {
    /// The day the amount was cleared.
    #[must_use]
    pub const fn cleared_on(&self) -> NaiveDate {
        self.cleared_date
    }

    /// The current state and its reconciliation date, if any.
    #[must_use]
    pub const fn state(&self) -> ClearingState {
        self.state
    }

    /// Project a clearing onto the given day. A later promotion is not yet visible.
    #[must_use]
    pub fn as_of(mut self, cutoff: NaiveDate) -> Option<Self> {
        if self.cleared_date > cutoff {
            return None;
        }
        if matches!(self.state, ClearingState::Reconciled { on } if on > cutoff) {
            self.state = ClearingState::Cleared;
        }
        Some(self)
    }
}

/// Total cleared amount of a set of clearings.
///
/// # Errors
/// Returns a database error when the stored clearings exceed the minor-unit range.
pub fn cleared_sum(clearings: &[Clearing]) -> Result<i64, CliError> {
    let overflow =
        || CliError::Database("clearing total exceeds the stored minor-unit range".into());
    let total = clearings.iter().try_fold(Amount::ZERO, |total, clearing| {
        total
            .checked_add(Amount::from(clearing.amount))
            .ok_or_else(overflow)
    })?;
    i64::try_from(total.minor_units()).map_err(|_| overflow())
}

/// Derives the entry-level summary status from its clearings.
///
/// # Errors
/// Returns a database error when the stored clearing total is outside the entry amount.
pub fn derive_status(
    entry_amount: PositiveMinorUnits,
    clearings: &[Clearing],
) -> Result<EntryStatus, CliError> {
    let cleared = cleared_sum(clearings)?;
    let remaining = remaining_amount(entry_amount, cleared)?;
    Ok(if clearings.is_empty() || remaining > 0 {
        EntryStatus::Uncleared
    } else if clearings
        .iter()
        .all(|c| c.state.status() == ClearingStatus::Reconciled)
    {
        EntryStatus::Reconciled
    } else {
        EntryStatus::Cleared
    })
}

pub(crate) fn remaining_amount(
    entry_amount: PositiveMinorUnits,
    cleared: i64,
) -> Result<i64, CliError> {
    entry_amount
        .minor_units()
        .checked_sub(cleared)
        .filter(|remaining| *remaining >= 0)
        .ok_or_else(|| CliError::Database("clearing total exceeds the stored entry amount".into()))
}

/// What a clear request asks for.
#[derive(Debug, Clone, Copy)]
pub struct ClearSpec<'a> {
    /// Amount to clear in minor units; `None` means "the full remaining
    /// amount".
    pub amount: Option<PositiveMinorUnits>,
    pub status: ClearingStatus,
    pub statement: Option<&'a str>,
    /// `YYYY-MM-DD` date of the request: the `cleared_date` of a new clearing
    /// and, with status reconciled, the `reconciled_date` of every clearing it
    /// creates or promotes.
    pub date: NaiveDate,
    /// Date of the entry's transaction; no clearing may predate it.
    pub txn_date: NaiveDate,
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
    Insert {
        amount: PositiveMinorUnits,
        promote: Vec<ClearingId>,
    },
    /// Promote existing clearings to reconciled; insert nothing.
    Promote { ids: Vec<ClearingId> },
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
/// Returns [`CliError::Validation`] for over-clearing,
/// a conflicting amount for an already-applied statement, a date before the
/// transaction, a reconciled date before a promoted clearing's cleared date,
/// or a `cleared` request on locked (reconciled) clearings.
pub fn plan_clear(
    entry_amount: PositiveMinorUnits,
    existing: &[Clearing],
    spec: &ClearSpec<'_>,
) -> Result<ClearPlan, CliError> {
    if spec.date < spec.txn_date {
        let what = if spec.status == ClearingStatus::Reconciled {
            "reconciled"
        } else {
            "clearing"
        };
        return Err(CliError::Validation(format!(
            "{what} date {} is before the transaction date {}",
            spec.date, spec.txn_date
        )));
    }

    if let Some(stmt) = spec.statement
        && let Some(prior) = existing
            .iter()
            .find(|c| c.statement_ref.as_deref() == Some(stmt))
    {
        if let Some(a) = spec.amount
            && a != prior.amount
        {
            return Err(CliError::Validation(format!(
                "entry {} was already cleared for {} minor units by statement '{stmt}'; \
                 refusing to record a different amount ({a})",
                prior.entry_id, prior.amount
            )));
        }
        return match (spec.status, prior.state.status()) {
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

    let remaining = remaining_amount(entry_amount, cleared_sum(existing)?)?;
    if spec.status == ClearingStatus::Cleared
        && remaining <= 0
        && let Some(first) = existing.first()
        && existing
            .iter()
            .all(|c| c.state.status() == ClearingStatus::Reconciled)
    {
        return Err(locked_error(first.entry_id, None));
    }
    if let Some(amount) = spec.amount
        && amount.minor_units() > remaining
    {
        return Err(CliError::Validation(format!(
            "over-clearing: requested {amount} minor units but only {remaining} of {entry_amount} remain uncleared"
        )));
    }

    let amount = match spec.amount {
        None if remaining > 0 => Some(remaining.try_into()?),
        amount => amount,
    };

    let promote: Vec<ClearingId> = if spec.status == ClearingStatus::Reconciled
        && spec.amount.is_none()
        && spec.statement.is_none()
    {
        let mut ids = Vec::new();
        for c in existing
            .iter()
            .filter(|c| c.state.status() == ClearingStatus::Cleared)
        {
            check_promotable(c, spec.date)?;
            ids.push(c.id);
        }
        ids
    } else {
        Vec::new()
    };

    if let Some(amount) = amount {
        Ok(ClearPlan::Insert { amount, promote })
    } else if promote.is_empty() {
        Ok(ClearPlan::Skip(SkipReason::FullyCleared))
    } else {
        Ok(ClearPlan::Promote { ids: promote })
    }
}

/// A clearing can only be reconciled on or after its cleared date.
fn check_promotable(c: &Clearing, reconciled_date: NaiveDate) -> Result<(), CliError> {
    if reconciled_date < c.cleared_date {
        return Err(CliError::Validation(format!(
            "reconciled date {reconciled_date} is before the cleared date {} of clearing {} \
             (entry {}); reconcile on or after the cleared date",
            c.cleared_date, c.id, c.entry_id
        )));
    }
    Ok(())
}

/// The error for a `cleared` request on reconciled (locked) clearings.
fn locked_error(entry_id: EntryId, statement: Option<&str>) -> CliError {
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
pub fn plan_unclear(existing: &[Clearing], statement: Option<&str>) -> Vec<ClearingId> {
    existing
        .iter()
        .filter(|c| statement.is_none_or(|s| c.statement_ref.as_deref() == Some(s)))
        .map(|c| c.id)
        .collect()
}

// ---------------------------------------------------------------------------
// Read-only queries
// ---------------------------------------------------------------------------

/// Lists an entry's clearings within this company, oldest first.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_clearings(company: &Company<'_>, entry_id: EntryId) -> Result<Vec<Clearing>, CliError> {
    let mut stmt = company.read().prepare_cached(
        "SELECT c.id, c.entry_id, c.amount, c.cleared_date, c.statement_ref, c.status, \
         c.reconciled_date \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
         JOIN entry_clearings c ON c.entry_id = e.id \
         WHERE e.id = ?1 AND e.company_slug = ?2",
    )?;
    let rows = stmt.query_map(
        params![entry_id, company.slug().as_str()],
        clearing_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    out.sort_unstable_by_key(|clearing| (clearing.cleared_date, clearing.id));
    Ok(out)
}

fn clearing_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Clearing> {
    let cleared_date = super::calendar_date::read(row, 3)?;
    let status = row
        .get::<_, String>(5)?
        .parse::<ClearingStatus>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(5, Type::Text, Box::new(error))
        })?;
    let reconciled = super::calendar_date::read_optional(row, 6)?;
    let state = match (status, reconciled) {
        (ClearingStatus::Cleared, None) => ClearingState::Cleared,
        (ClearingStatus::Reconciled, Some(on)) if on >= cleared_date => {
            ClearingState::Reconciled { on }
        }
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                6,
                Type::Text,
                Box::new(CliError::Database(
                    "persisted clearing status and dates disagree".into(),
                )),
            ));
        }
    };
    Ok(Clearing {
        id: row.get(0)?,
        entry_id: row.get(1)?,
        amount: row.get(2)?,
        cleared_date,
        statement_ref: row.get(4)?,
        state,
    })
}

/// An entry resolved for clearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryTarget {
    pub transaction_id: TransactionId,
    pub entry_id: EntryId,
    pub account_code: String,
    pub amount: PositiveMinorUnits,
    pub currency: Currency,
    pub reference: Option<String>,
    pub date: NaiveDate,
}

const TARGET_SELECT: &str = "SELECT t.id, e.id, e.account_code, e.amount, t.currency, \
     t.reference, t.date \
     FROM entries e JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug";

fn map_target(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryTarget> {
    Ok(EntryTarget {
        transaction_id: row.get(0)?,
        entry_id: row.get(1)?,
        account_code: row.get(2)?,
        amount: row.get(3)?,
        currency: row.get(4)?,
        reference: row.get(5)?,
        date: super::calendar_date::read(row, 6)?,
    })
}

/// Resolves an entry by transaction id and entry id.
///
/// # Errors
///
/// Returns `CliError::NotFound` when the entry does not belong to the
/// transaction in this company.
pub fn get_entry_target(
    company: &Company<'_>,
    transaction_id: TransactionId,
    entry_id: EntryId,
) -> Result<EntryTarget, CliError> {
    let slug = company.slug().as_str();
    company
        .read()
        .query_row(
            &format!("{TARGET_SELECT} WHERE t.company_slug = ?1 AND t.id = ?2 AND e.id = ?3"),
            params![slug, transaction_id, entry_id],
            map_target,
        )
        .optional()?
        .ok_or_else(|| {
            CliError::NotFound(format!(
                "entry {entry_id} not found in transaction {transaction_id} for company '{slug}'"
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
    company: &Company<'_>,
    reference: &str,
    account: &str,
) -> Result<EntryTarget, CliError> {
    let slug = company.slug().as_str();
    let conn = company.read();
    let txn_id: Option<TransactionId> = conn
        .query_row(
            "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
            params![slug, reference],
            |row| row.get(0),
        )
        .optional()?;
    let Some(txn_id) = txn_id else {
        return Err(CliError::NotFound(format!(
            "no transaction with reference '{reference}' in company '{slug}'"
        )));
    };

    let mut stmt = conn.prepare(&format!(
        "{TARGET_SELECT} WHERE t.id = ?1 AND e.account_code = ?2 AND t.company_slug = ?3 ORDER BY e.id"
    ))?;
    let mut found = stmt
        .query_map(params![txn_id, account, slug], map_target)?
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

    fn date(value: &str) -> NaiveDate {
        super::super::calendar_date::parse_day(value)
            .unwrap_or_else(|error| panic!("test date: {error}"))
    }

    fn units(amount: i64) -> PositiveMinorUnits {
        amount
            .try_into()
            .unwrap_or_else(|error| panic!("invalid fixture amount: {error}"))
    }

    fn c(
        id: i64,
        amount: PositiveMinorUnits,
        stmt: Option<&str>,
        status: ClearingStatus,
    ) -> Clearing {
        Clearing {
            id: crate::db::ids::fixture_id(id),
            entry_id: crate::db::ids::fixture_id(1),
            amount,
            cleared_date: date(TXN_DATE),
            statement_ref: stmt.map(str::to_string),
            state: status.on(date(TXN_DATE)),
        }
    }

    const TXN_DATE: &str = "2026-01-01";
    const DATE: &str = "2026-01-31";

    fn spec(amount: Option<i64>, statement: Option<&str>) -> ClearSpec<'_> {
        ClearSpec {
            amount: amount.map(units),
            status: ClearingStatus::Cleared,
            statement,
            date: date(DATE),
            txn_date: date(TXN_DATE),
        }
    }

    fn reconcile<'a>(statement: Option<&'a str>, on: &str) -> ClearSpec<'a> {
        ClearSpec {
            amount: None,
            status: ClearingStatus::Reconciled,
            statement,
            date: date(on),
            txn_date: date(TXN_DATE),
        }
    }

    use ClearingStatus::{Cleared, Reconciled};

    #[test]
    fn stored_clearings_require_matching_state_and_date() -> rusqlite::Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let decode = |cleared, status, reconciled| {
            conn.query_row(
                "SELECT 1, 1, 100, ?1, NULL, ?2, ?3",
                params![cleared, status, reconciled],
                clearing_from_row,
            )
        };
        for (start, status, end) in [
            ("invalid", "cleared", None),
            ("2026-02-01", "cleared", Some("2026-02-02")),
            ("2026-02-01", "reconciled", None),
            ("2026-02-01", "reconciled", Some("2026-01-31")),
        ] {
            assert!(
                decode(start, status, end).is_err(),
                "{status}: {start} {end:?}"
            );
        }

        let clearing = decode(
            "2026-02-01 09:00:00",
            "reconciled",
            Some("2026-02-15T17:00:00"),
        )?;
        assert_eq!(clearing.cleared_on(), date("2026-02-01"));
        assert_eq!(
            clearing.state(),
            ClearingState::Reconciled {
                on: date("2026-02-15")
            }
        );
        let earlier = clearing
            .as_of(date("2026-02-01"))
            .and_then(|value| serde_json::to_value(value).ok());
        assert_eq!(
            earlier,
            Some(serde_json::json!({
                "id": 1, "entry_id": 1, "amount": 100,
                "cleared_date": "2026-02-01", "statement_ref": null,
                "reconciled_date": null, "status": "cleared"
            }))
        );
        Ok(())
    }

    #[test]
    fn planner_compares_calendar_days_instead_of_text() {
        let mut later = spec(None, None);
        later.txn_date = date("9999-12-31");
        later.date = date("+10000-01-01");
        assert!(plan_clear(units(100), &[], &later).is_ok());
    }

    #[test]
    fn persisted_clearing_totals_reject_overflow_and_overclear() {
        let wide = [
            c(1, units(i64::MAX), None, Cleared),
            c(2, units(1), None, Cleared),
        ];
        assert!(matches!(cleared_sum(&wide), Err(CliError::Database(_))));
        assert!(matches!(
            derive_status(units(1), &[c(1, units(2), None, Cleared)]),
            Err(CliError::Database(_))
        ));
    }

    #[test]
    fn derive_status_rules() -> Result<(), CliError> {
        assert_eq!(derive_status(units(100), &[])?, EntryStatus::Uncleared);
        assert_eq!(
            derive_status(units(100), &[c(1, units(40), None, Cleared)])?,
            EntryStatus::Uncleared
        );
        assert_eq!(
            derive_status(
                units(100),
                &[
                    c(1, units(40), None, Cleared),
                    c(2, units(60), None, Reconciled)
                ]
            )?,
            EntryStatus::Cleared
        );
        assert_eq!(
            derive_status(
                units(100),
                &[
                    c(1, units(40), None, Reconciled),
                    c(2, units(60), None, Reconciled)
                ]
            )?,
            EntryStatus::Reconciled
        );
        Ok(())
    }

    #[test]
    fn default_clears_full_remaining() {
        assert_eq!(
            plan_clear(units(100), &[], &spec(None, None)).ok(),
            Some(ClearPlan::Insert {
                amount: units(100),
                promote: vec![]
            })
        );
        assert_eq!(
            plan_clear(
                units(100),
                &[c(1, units(30), Some("S1"), Cleared)],
                &spec(None, None)
            )
            .ok(),
            Some(ClearPlan::Insert {
                amount: units(70),
                promote: vec![]
            })
        );
    }

    #[test]
    fn partial_clears_accumulate_until_full() {
        let existing = [c(1, units(30), Some("S1"), Cleared)];
        assert_eq!(
            plan_clear(units(100), &existing, &spec(Some(70), Some("S2"))).ok(),
            Some(ClearPlan::Insert {
                amount: units(70),
                promote: vec![]
            })
        );
        let full = [
            c(1, units(30), Some("S1"), Cleared),
            c(2, units(70), Some("S2"), Cleared),
        ];
        assert_eq!(
            plan_clear(units(100), &full, &spec(None, None)).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
    }

    #[test]
    fn over_clearing_is_rejected() {
        let existing = [c(1, units(60), None, Cleared)];
        assert!(matches!(
            plan_clear(units(100), &existing, &spec(Some(41), None)),
            Err(CliError::Validation(_))
        ));
        assert!(plan_clear(units(100), &existing, &spec(Some(40), None)).is_ok());
        assert!(PositiveMinorUnits::try_from(0_i64).is_err());
    }

    #[test]
    fn same_statement_is_idempotent() {
        let existing = [c(7, units(30), Some("S1"), Cleared)];
        assert_eq!(
            plan_clear(units(100), &existing, &spec(Some(30), Some("S1"))).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
        );
        assert_eq!(
            plan_clear(units(100), &existing, &spec(None, Some("S1"))).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
        );
        // A different amount for the same statement is a conflict.
        assert!(matches!(
            plan_clear(units(100), &existing, &spec(Some(31), Some("S1"))),
            Err(CliError::Validation(_))
        ));
        // Reconciling a statement promotes its clearing.
        assert_eq!(
            plan_clear(units(100), &existing, &reconcile(Some("S1"), DATE)).ok(),
            Some(ClearPlan::Promote {
                ids: vec![crate::db::ids::fixture_id(7)]
            })
        );
    }

    #[test]
    fn reconcile_in_full_promotes_existing() {
        let reconcile = reconcile(None, DATE);
        let partial = [c(1, units(40), None, Cleared)];
        assert_eq!(
            plan_clear(units(100), &partial, &reconcile).ok(),
            Some(ClearPlan::Insert {
                amount: units(60),
                promote: vec![crate::db::ids::fixture_id(1)]
            })
        );
        let full = [c(1, units(100), None, Cleared)];
        assert_eq!(
            plan_clear(units(100), &full, &reconcile).ok(),
            Some(ClearPlan::Promote {
                ids: vec![crate::db::ids::fixture_id(1)]
            })
        );
        let done = [c(1, units(100), None, Reconciled)];
        assert_eq!(
            plan_clear(units(100), &done, &reconcile).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
    }

    #[test]
    fn date_before_transaction_is_rejected() {
        let mut early = spec(None, None);
        early.date = date("2025-12-31");
        let err = plan_clear(units(100), &[], &early);
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("before the transaction date")),
            "{err:?}"
        );
        // Same day is fine; a stored date with a time is compared by day.
        let mut same = spec(None, None);
        same.date = date(TXN_DATE);
        same.txn_date = date("2026-01-01 09:00:00");
        assert!(plan_clear(units(100), &[], &same).is_ok());
        // The rule applies to the reconciled date too.
        let err = plan_clear(units(100), &[], &reconcile(None, "2025-12-31"));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.starts_with("reconciled date")),
            "{err:?}"
        );
    }

    #[test]
    fn reconciled_date_must_not_precede_cleared_date() {
        let mut feb = c(7, units(100), Some("S1"), Cleared);
        feb.cleared_date = date("2026-02-15");
        let existing = [feb];
        // Promote by statement and promote in full both check the date.
        for spec in [
            reconcile(Some("S1"), "2026-02-14"),
            reconcile(None, "2026-02-14"),
        ] {
            let err = plan_clear(units(100), &existing, &spec);
            assert!(
                matches!(&err, Err(CliError::Validation(m)) if m.contains("before the cleared date")),
                "{err:?}"
            );
        }
        assert_eq!(
            plan_clear(units(100), &existing, &reconcile(Some("S1"), "2026-02-15")).ok(),
            Some(ClearPlan::Promote {
                ids: vec![crate::db::ids::fixture_id(7)]
            })
        );
    }

    #[test]
    fn cleared_request_on_reconciled_clearings_is_locked() {
        // By statement.
        let existing = [c(7, units(30), Some("S1"), Reconciled)];
        let err = plan_clear(units(100), &existing, &spec(None, Some("S1")));
        assert!(
            matches!(&err, Err(CliError::Validation(m))
                if m.contains("locked") && m.contains("--status uncleared --statement S1")),
            "{err:?}"
        );
        // Fully reconciled entry, no statement.
        let full = [
            c(1, units(40), None, Reconciled),
            c(2, units(60), Some("S2"), Reconciled),
        ];
        let err = plan_clear(units(100), &full, &spec(None, None));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("locked")),
            "{err:?}"
        );
        let err = plan_clear(units(100), &full, &spec(Some(1), Some("S3")));
        assert!(
            matches!(&err, Err(CliError::Validation(m)) if m.contains("locked")),
            "{err:?}"
        );
        // A partially reconciled entry can still clear its remainder.
        assert_eq!(
            plan_clear(units(100), &existing, &spec(None, None)).ok(),
            Some(ClearPlan::Insert {
                amount: units(70),
                promote: vec![]
            })
        );
        // Mixed, fully covered: still a plain skip.
        let mixed = [
            c(1, units(40), None, Reconciled),
            c(2, units(60), None, Cleared),
        ];
        assert_eq!(
            plan_clear(units(100), &mixed, &spec(None, None)).ok(),
            Some(ClearPlan::Skip(SkipReason::FullyCleared))
        );
        // Reconciling an already reconciled statement is still a skip.
        assert_eq!(
            plan_clear(units(100), &existing, &reconcile(Some("S1"), DATE)).ok(),
            Some(ClearPlan::Skip(SkipReason::AlreadyClearedByStatement))
        );
    }

    #[test]
    fn unclear_selects_by_statement() {
        let existing = [
            c(1, units(30), Some("S1"), Cleared),
            c(2, units(20), None, Cleared),
        ];
        assert_eq!(
            plan_unclear(&existing, None),
            vec![crate::db::ids::fixture_id(1), crate::db::ids::fixture_id(2)]
        );
        assert_eq!(
            plan_unclear(&existing, Some("S1")),
            vec![crate::db::ids::fixture_id(1)]
        );
        assert_eq!(
            plan_unclear(&existing, Some("S9")),
            Vec::<ClearingId>::new()
        );
    }
}
