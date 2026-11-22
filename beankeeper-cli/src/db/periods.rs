//! Company-scoped inclusive posting cutoffs, checked against their audit snapshots.

use std::{fmt, str::FromStr};

use chrono::{Datelike as _, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use serde::{Serialize, Serializer};

use super::connection::ReadConn;
use crate::error::CliError;

/// A calendar date representable in the ledger's canonical cutoff format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PeriodDate(NaiveDate);

impl PeriodDate {
    /// The inclusive calendar cutoff.
    #[must_use]
    pub const fn date(self) -> NaiveDate {
        self.0
    }
}

impl TryFrom<NaiveDate> for PeriodDate {
    type Error = CliError;

    fn try_from(date: NaiveDate) -> Result<Self, Self::Error> {
        if (0..=9999).contains(&date.year()) {
            Ok(Self(date))
        } else {
            Err(noncanonical_date())
        }
    }
}

impl FromStr for PeriodDate {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.len() != 10
            || !raw.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                _ => byte.is_ascii_digit(),
            })
        {
            return Err(noncanonical_date());
        }
        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(Self)
            .map_err(|error| CliError::Validation(format!("invalid period date '{raw}': {error}")))
    }
}

impl fmt::Display for PeriodDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

fn noncanonical_date() -> CliError {
    CliError::Validation("period date must use YYYY-MM-DD".into())
}

/// A nonempty explanation for reopening a company's posting period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenReason(Box<str>);

impl ReopenReason {
    /// The recorded explanation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_stored(reason: String) -> Result<Self, CliError> {
        if reason.trim().is_empty() {
            return Err(missing_reason());
        }
        Ok(Self(reason.into_boxed_str()))
    }
}

impl FromStr for ReopenReason {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(missing_reason());
        }
        Ok(Self(trimmed.into()))
    }
}

fn missing_reason() -> CliError {
    CliError::Validation("period reopen requires a nonempty reason".into())
}

/// The two ways to write a period change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeriodChange {
    /// Close through a specific calendar date.
    Close(PeriodDate),
    /// Reopen the entire interval with an audited explanation.
    Reopen(ReopenReason),
}

/// The state of a company that has recorded at least one period change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeriodState {
    /// The last change reopened the period.
    Open(ReopenReason),
    /// Posting is locked on or before the inclusive cutoff.
    ClosedThrough {
        /// The active cutoff.
        cutoff: PeriodDate,
        /// Optional reason read from records written by the previous embedding API.
        legacy_reason: Option<String>,
    },
}

/// Current cutoff and last close/reopen attribution. Reopened rows remain stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodLock {
    pub company_slug: String,
    pub state: PeriodState,
    pub changed_at: String,
    pub actor: String,
}

impl PeriodLock {
    /// The current inclusive cutoff, when closed.
    #[must_use]
    pub const fn closed_through(&self) -> Option<PeriodDate> {
        match self.state {
            PeriodState::Open(_) => None,
            PeriodState::ClosedThrough { cutoff, .. } => Some(cutoff),
        }
    }

    /// The last change's reason, if recorded.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.state {
            PeriodState::Open(reason) => Some(reason.as_str()),
            PeriodState::ClosedThrough { legacy_reason, .. } => legacy_reason.as_deref(),
        }
    }
}

impl Serialize for PeriodLock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Preserve the SQL columns and historical audit snapshot shape.
        #[derive(Serialize)]
        struct Snapshot<'a> {
            company_slug: &'a str,
            closed_through: Option<PeriodDate>,
            changed_at: &'a str,
            actor: &'a str,
            reason: Option<&'a str>,
        }

        Snapshot {
            company_slug: &self.company_slug,
            closed_through: self.closed_through(),
            changed_at: &self.changed_at,
            actor: &self.actor,
            reason: self.reason(),
        }
        .serialize(serializer)
    }
}

struct StoredPeriodLock {
    company_slug: String,
    closed_through: Option<String>,
    changed_at: String,
    actor: String,
    reason: Option<String>,
}

impl StoredPeriodLock {
    fn decode(self) -> Result<PeriodLock, CliError> {
        let state = match self.closed_through {
            Some(raw) => PeriodState::ClosedThrough {
                cutoff: raw.parse()?,
                legacy_reason: self.reason,
            },
            None => PeriodState::Open(ReopenReason::from_stored(
                self.reason.ok_or_else(missing_reason)?,
            )?),
        };
        Ok(PeriodLock {
            company_slug: self.company_slug,
            state,
            changed_at: self.changed_at,
            actor: self.actor,
        })
    }
}

/// Parse a canonical cutoff date for storage and calendar comparisons.
/// # Errors
/// Rejects invalid or noncanonical dates outside `YYYY-MM-DD`.
pub fn parse_date(raw: &str) -> Result<NaiveDate, CliError> {
    raw.parse::<PeriodDate>().map(PeriodDate::date)
}

fn inconsistent_history(company: &str) -> CliError {
    CliError::Database(format!(
        "period lock for company '{company}' does not match its audit history"
    ))
}

/// Read current state and require an identical latest audit snapshot, even if
/// someone bypassed triggers to delete or replace the stored row.
///
/// # Errors
/// Returns a database error for missing, invalid, or unaudited state.
pub fn get(conn: ReadConn<'_>, company: &str) -> Result<Option<PeriodLock>, CliError> {
    let state = conn
        .query_row(
            "SELECT company_slug, closed_through, changed_at, actor, reason
         FROM period_locks WHERE company_slug = ?1",
            [company],
            |row| {
                Ok(StoredPeriodLock {
                    company_slug: row.get(0)?,
                    closed_through: row.get(1)?,
                    changed_at: row.get(2)?,
                    actor: row.get(3)?,
                    reason: row.get(4)?,
                })
            },
        )
        .optional()?
        .map(StoredPeriodLock::decode)
        .transpose()
        .map_err(|_| inconsistent_history(company))?;
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
            serde_json::from_str::<serde_json::Value>(&raw).ok() == Some(serde_json::json!(lock))
        }
        _ => false,
    };
    if !valid {
        return Err(inconsistent_history(company));
    }
    Ok(state)
}

/// Proof that a company and posting date passed the period gate on this connection.
#[must_use]
pub struct OpenPeriod<'a> {
    conn: &'a Connection,
    company: &'a str,
    date: NaiveDate,
}

impl<'a> OpenPeriod<'a> {
    pub(super) const fn into_parts(self) -> (&'a Connection, &'a str, NaiveDate) {
        (self.conn, self.company, self.date)
    }
}

/// Reject a new posting inside the company's inclusive closed interval.
/// Check and consume the result inside the same savepoint as the posting.
/// # Errors
/// Returns validation for a locked date, or database errors for inconsistent state.
pub fn check_posting<'a>(
    conn: &'a Connection,
    company: &'a str,
    date: NaiveDate,
) -> Result<OpenPeriod<'a>, CliError> {
    ensure_open(ReadConn::new(conn), company, date)?;
    Ok(OpenPeriod {
        conn,
        company,
        date,
    })
}

/// Validate a proposed posting date without creating a writer witness.
pub(super) fn ensure_open(
    conn: ReadConn<'_>,
    company: &str,
    date: NaiveDate,
) -> Result<(), CliError> {
    if let Some(cutoff) = get(conn, company)?.and_then(|lock| lock.closed_through())
        && date <= cutoff.date()
    {
        return Err(CliError::Validation(format!(
            "transaction date {date} is in a closed period for company '{company}' (closed through {cutoff}); use period reopen with a reason to unlock it"
        )));
    }
    Ok(())
}

/// Verify stored rows and histories, including histories whose row was removed.
/// # Errors
/// Returns database errors for inconsistent state or missing company tenancy.
pub fn verify(conn: ReadConn<'_>) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT company_slug FROM period_locks
         UNION SELECT company_slug FROM audit_log WHERE entity = 'period'",
    )?;
    for company in stmt.query_map([], |row| row.get::<_, String>(0))? {
        let company = company?;
        super::companies::read_company(conn, &company)?;
        get(conn, &company)?;
    }
    Ok(())
}

/// List the company's current state, including the last reopen if any.
/// # Errors
/// Returns database errors for inconsistent state or an unknown company.
pub fn list(conn: ReadConn<'_>, company: &str) -> Result<Vec<PeriodLock>, CliError> {
    super::companies::read_company(conn, company)?;
    Ok(get(conn, company)?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_inputs_validate_dates_and_reopen_reasons() -> Result<(), CliError> {
        assert!("2026-2-28".parse::<PeriodDate>().is_err());
        assert!("2026-02-29".parse::<PeriodDate>().is_err());
        assert!(
            NaiveDate::from_ymd_opt(10_000, 1, 1)
                .is_some_and(|date| { PeriodDate::try_from(date).is_err() })
        );
        assert!("  ".parse::<ReopenReason>().is_err());
        assert_eq!(
            "  missing receipt  ".parse::<ReopenReason>()?.as_str(),
            "missing receipt"
        );
        Ok(())
    }

    #[test]
    fn historical_period_state_serializes_in_the_same_shape() -> Result<(), CliError> {
        for (cutoff, reason, expected) in [
            (
                Some("2026-01-31"),
                "historic close note",
                r#"{"company_slug":"acme","closed_through":"2026-01-31","changed_at":"at","actor":"owner","reason":"historic close note"}"#,
            ),
            (
                None,
                "  historic reason  ",
                r#"{"company_slug":"acme","closed_through":null,"changed_at":"at","actor":"owner","reason":"  historic reason  "}"#,
            ),
        ] {
            let row = StoredPeriodLock {
                company_slug: "acme".into(),
                closed_through: cutoff.map(str::to_owned),
                changed_at: "at".into(),
                actor: "owner".into(),
                reason: Some(reason.into()),
            }
            .decode()?;
            assert_eq!(serde_json::to_string(&row).ok().as_deref(), Some(expected));
        }
        Ok(())
    }
}
