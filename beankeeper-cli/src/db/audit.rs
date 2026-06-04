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
        if let Some(name) = explicit
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
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

/// Sets the clearance status of an entry, recording the transition.
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
    in_savepoint(conn, "sp_entry_status", || {
        let current: String = conn
            .query_row(
                "SELECT status FROM entries \
                 WHERE id = ?1 AND transaction_id = ?2 AND company_slug = ?3",
                params![entry_id, transaction_id, company_slug],
                |row| row.get(0),
            )
            .map_err(|_| {
                CliError::NotFound(format!(
                    "entry {entry_id} not found in transaction {transaction_id} for company '{company_slug}'"
                ))
            })?;
        let from = EntryStatus::from_str(&current)?;

        conn.execute(
            "UPDATE entries SET status = ?1 \
             WHERE id = ?2 AND transaction_id = ?3 AND company_slug = ?4",
            params![to.as_str(), entry_id, transaction_id, company_slug],
        )?;

        record(
            conn,
            actor,
            Some(company_slug),
            AuditEntity::Entry,
            &entry_id.to_string(),
            AuditAction::StatusChange,
            Some(&snapshot(&serde_json::json!({ "status": from.as_str() }))),
            Some(&snapshot(&serde_json::json!({ "status": to.as_str() }))),
        )?;

        Ok(from)
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

        if let Some(ref meta) = partner_metadata {
            if meta.contains("\"correlate\"") {
                return Err(CliError::Validation(format!(
                    "transaction #{partner_id} is already correlated"
                )));
            }
        }

        let updated = merge_correlate_into_metadata(partner_metadata.as_deref(), new_txn_id);

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
            Some(&snapshot(&serde_json::json!({ "metadata": partner_metadata }))),
            Some(&snapshot(&serde_json::json!({
                "metadata": updated,
                "correlated_with": new_txn_id,
            }))),
        )?;

        Ok(())
    })
}

/// Merge a `correlate` key into existing metadata.
///
/// - No existing metadata -> `{"correlate": id}`
/// - Existing JSON object -> insert `correlate` key
/// - Existing plain string -> `{"correlate": id, "ref": "old_string"}`
fn merge_correlate_into_metadata(existing: Option<&str>, correlate_id: i64) -> String {
    match existing {
        None => format!(r#"{{"correlate":{correlate_id}}}"#),
        Some(s) if s.starts_with('{') => {
            let rest = &s[1..]; // skip opening brace
            if rest.trim_start().starts_with('}') {
                format!(r#"{{"correlate":{correlate_id}}}"#)
            } else {
                format!(r#"{{"correlate":{correlate_id},{rest}"#)
            }
        }
        Some(s) => format!(
            r#"{{"correlate":{correlate_id},"ref":{}}}"#,
            serde_json::json!(s)
        ),
    }
}

/// Upserts a single month's budget, recording the prior amount (if any).
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_budget(conn: &Connection, actor: &Actor, p: &SetBudgetParams<'_>) -> Result<BudgetRow, CliError> {
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
        let victims = collect_budget_victims(conn, company_slug, account_code, currency, year, month)?;

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
        let mapped = stmt.query_map(params![company_slug, account_code, currency, year, m], map_row)?;
        mapped.collect::<Result<_, _>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, month, amount, notes FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 AND year = ?4",
        )?;
        let mapped = stmt.query_map(params![company_slug, account_code, currency, year], map_row)?;
        mapped.collect::<Result<_, _>>()?
    };

    Ok(rows)
}

/// Deletes an account by company slug and code, recording a snapshot.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the account does not exist, or
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
/// Returns `CliError::NotFound` if the company does not exist, or
/// `CliError::Sqlite` on database errors.
pub fn delete_company(conn: &Connection, actor: &Actor, slug: &str) -> Result<(), CliError> {
    in_savepoint(conn, "sp_company_delete", || {
        let row: CompanyRow = super::companies::get_company(conn, slug)?;

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
        for s in [EntryStatus::Uncleared, EntryStatus::Cleared, EntryStatus::Reconciled] {
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

        let (actor_col, action, before, after): (String, String, Option<String>, Option<String>) = db
            .conn()
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

    #[test]
    fn delete_account_missing_errors() {
        let db = seed();
        let actor = Actor::new("tester");
        assert!(delete_account(db.conn(), &actor, "acme", "9999").is_err());
    }
}
