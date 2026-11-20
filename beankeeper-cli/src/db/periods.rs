//! Company-scoped inclusive posting cutoffs, checked against their audit snapshots.

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::error::CliError;

/// Current cutoff and last close/reopen attribution. Reopened rows remain stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeriodLock {
    pub company_slug: String,
    /// All transaction dates on or before this date are locked; `None` means open.
    pub closed_through: Option<String>,
    pub changed_at: String,
    pub actor: String,
    pub reason: Option<String>,
}

/// Parse a canonical cutoff date for storage and calendar comparisons.
///
/// # Errors
/// Rejects invalid or noncanonical dates outside `YYYY-MM-DD`.
pub fn parse_date(raw: &str) -> Result<chrono::NaiveDate, CliError> {
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|error| CliError::Validation(format!("invalid period date '{raw}': {error}")))?;
    if raw.len() != 10 || date.to_string() != raw {
        return Err(CliError::Validation(
            "period date must use YYYY-MM-DD".into(),
        ));
    }
    Ok(date)
}

/// Read current state and require an identical latest audit snapshot, even if
/// someone bypassed triggers to delete or replace the stored row.
///
/// # Errors
/// Returns a database error for missing, invalid, or unaudited state.
pub fn get(conn: &Connection, company: &str) -> Result<Option<PeriodLock>, CliError> {
    let state = conn
        .query_row(
            "SELECT company_slug, closed_through, changed_at, actor, reason
         FROM period_locks WHERE company_slug = ?1",
            [company],
            |row| {
                Ok(PeriodLock {
                    company_slug: row.get(0)?,
                    closed_through: row.get(1)?,
                    changed_at: row.get(2)?,
                    actor: row.get(3)?,
                    reason: row.get(4)?,
                })
            },
        )
        .optional()?;
    let latest: Option<Option<String>> = conn
        .query_row(
            "SELECT after FROM audit_log WHERE entity = 'period' AND company_slug = ?1
         AND entity_id = ?1 ORDER BY id DESC LIMIT 1",
            [company],
            |row| row.get(0),
        )
        .optional()?;
    let valid = match (&state, latest) {
        (None, None) => true,
        (Some(lock), Some(Some(raw))) => {
            let valid_date = lock.closed_through.as_deref().map_or_else(
                || {
                    lock.reason
                        .as_ref()
                        .is_some_and(|reason| !reason.trim().is_empty())
                },
                |date| parse_date(date).is_ok(),
            );
            valid_date
                && serde_json::from_str::<serde_json::Value>(&raw).ok()
                    == Some(serde_json::json!(lock))
        }
        _ => false,
    };
    if !valid {
        return Err(CliError::Database(format!(
            "period lock for company '{company}' does not match its audit history"
        )));
    }
    Ok(state)
}

/// Reject a new posting inside the company's inclusive closed interval.
/// Call inside the same savepoint as the posting.
///
/// # Errors
/// Returns validation for a locked date, or database errors for inconsistent state.
pub fn check_posting(conn: &Connection, company: &str, date: &str) -> Result<(), CliError> {
    if let Some(cutoff) = get(conn, company)?.and_then(|lock| lock.closed_through)
        && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|error| CliError::Validation(format!("invalid transaction date: {error}")))?
            <= parse_date(&cutoff)?
    {
        return Err(CliError::Validation(format!(
            "transaction date {date} is in a closed period for company '{company}' (closed through {cutoff}); use period reopen with a reason to unlock it"
        )));
    }
    Ok(())
}

/// Verify stored rows and histories, including histories whose row was removed.
///
/// # Errors
/// Returns database errors for inconsistent state or missing company tenancy.
pub fn verify(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT company_slug FROM period_locks
         UNION SELECT company_slug FROM audit_log WHERE entity = 'period'",
    )?;
    for company in stmt.query_map([], |row| row.get::<_, String>(0))? {
        let company = company?;
        super::companies::get_company(conn, &company)?;
        get(conn, &company)?;
    }
    Ok(())
}

/// List the company's current state, including the last reopen if any.
///
/// # Errors
/// Returns database errors for inconsistent state or an unknown company.
pub fn list(conn: &Connection, company: &str) -> Result<Vec<PeriodLock>, CliError> {
    super::companies::get_company(conn, company)?;
    Ok(get(conn, company)?.into_iter().collect())
}
