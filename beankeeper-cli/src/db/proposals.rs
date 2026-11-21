//! Pending, immutable posting requests. Only audited resolution mutates them.

use mti::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::audit::{self, Actor};
use super::transactions::{ConflictStrategy, PostEntryParams, PostTransactionParams};
use crate::capability::Capability;
use crate::error::CliError;

/// A typed, time-sortable identifier for a pending posting request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalId(MagicTypeId);

impl ProposalId {
    fn new() -> Self {
        Self("proposal".create_type_id::<V7>())
    }

    /// The canonical identifier stored in SQLite and exposed in JSON.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::str::FromStr for ProposalId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = value
            .parse::<MagicTypeId>()
            .map_err(|error| CliError::Validation(error.to_string()))?;
        if id
            .prefix_str()
            .map_err(|error| CliError::Validation(error.to_string()))?
            != "proposal"
        {
            return Err(CliError::Validation("expected a proposal TypeID".into()));
        }
        Ok(Self(id))
    }
}

impl std::fmt::Display for ProposalId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for ProposalId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Frozen posting request, including resolved account tax defaults and retry policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostingPayload {
    pub description: String,
    pub metadata: Option<String>,
    pub currency: String,
    pub date: String,
    pub entries: Vec<PostEntryParams>,
    pub correlate: Option<i64>,
    pub reference: Option<String>,
    pub on_conflict: ConflictStrategy,
}

impl PostingPayload {
    /// Reuse the normal posting API with the approver's verified principal.
    #[must_use]
    pub fn params<'a>(&'a self, company: &'a str, actor: &'a Actor) -> PostTransactionParams<'a> {
        PostTransactionParams {
            company_slug: company,
            description: &self.description,
            metadata: self.metadata.as_deref(),
            currency: &self.currency,
            date: &self.date,
            entries: &self.entries,
            correlate: self.correlate,
            reference: self.reference.as_deref(),
            on_conflict: self.on_conflict,
            actor,
        }
    }
}

/// Reviewable proposal and its immutable proposer attribution and resolution.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: ProposalId,
    pub company_slug: String,
    pub status: String,
    pub payload: PostingPayload,
    pub reference: Option<String>,
    pub debit_total: i64,
    pub currency: String,
    pub proposer_uri: String,
    pub proposer_jti: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by_uri: Option<String>,
    pub resolved_by_jti: Option<String>,
    pub transaction_id: Option<i64>,
    pub reason: Option<String>,
}

const COLUMNS: &str = "id, company_slug, status, payload, reference, debit_total, currency, proposer_uri,
    proposer_jti, created_at, resolved_at, resolved_by_uri, resolved_by_jti, transaction_id, reason";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
    let raw: String = row.get(3)?;
    let payload = serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Proposal {
        id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|error: CliError| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        company_slug: row.get(1)?,
        status: row.get(2)?,
        payload,
        reference: row.get(4)?,
        debit_total: row.get(5)?,
        currency: row.get(6)?,
        proposer_uri: row.get(7)?,
        proposer_jti: row.get(8)?,
        created_at: row.get(9)?,
        resolved_at: row.get(10)?,
        resolved_by_uri: row.get(11)?,
        resolved_by_jti: row.get(12)?,
        transaction_id: row.get(13)?,
        reason: row.get(14)?,
    })
}

/// Load a proposal only in the selected company.
///
/// # Errors
/// Returns not found or a database error.
pub fn get(conn: &Connection, company: &str, id: &str) -> Result<Proposal, CliError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM txn_proposals WHERE company_slug = ?1 AND id = ?2"),
        params![company, id],
        from_row,
    )
    .optional()?
    .ok_or_else(|| CliError::NotFound(format!("proposal '{id}' not found")))
}

/// List pending proposals, or include all resolved outcomes for review.
///
/// # Errors
/// Returns policy integrity, capability or storage errors.
pub fn list(
    conn: &Connection,
    company: &str,
    actor: &Actor,
    include_resolved: bool,
) -> Result<Vec<Proposal>, CliError> {
    let all = can_review_all(conn, actor)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM txn_proposals WHERE company_slug = ?1
         AND (?2 OR status = 'pending') AND (?3 OR proposer_uri = ?4) ORDER BY created_at, id"
    ))?;
    Ok(stmt
        .query_map(
            params![company, include_resolved, all, actor.agent_uri()],
            from_row,
        )?
        .collect::<Result<_, _>>()?)
}

/// Whether the caller can see other proposers' requests. Admin grants review only.
///
/// # Errors
/// Returns policy integrity or capability errors.
pub(crate) fn can_review_all(conn: &Connection, actor: &Actor) -> Result<bool, CliError> {
    if !super::policy::get(conn)?.enforce {
        return Ok(true);
    }
    match actor.identity() {
        Some(identity) => {
            Ok(identity.allows(Capability::Approve)? || identity.allows(Capability::Admin)?)
        }
        None => Ok(false),
    }
}

/// Store a request without adding a transaction or touching the posted audit chain.
///
/// # Errors
/// Rejects missing identity, invalid postings and reserved/duplicate references.
pub fn create(
    conn: &Connection,
    p: &PostTransactionParams<'_>,
) -> Result<serde_json::Value, CliError> {
    let identity = p.actor.identity().ok_or_else(|| {
        CliError::Unauthorized("txn propose requires a verified agent token".into())
    })?;
    audit::atomically(conn, || {
        audit::require_capability(conn, p.actor, Capability::Propose)?;
        reject_reserved_metadata(p.metadata)?;
        validate_posting(conn, p)?;
        if let Some(reference) = p.reference {
            let posted: Option<i64> = conn
                .query_row(
                    "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
                    params![p.company_slug, reference],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = posted {
                return match p.on_conflict {
                    ConflictStrategy::Skip => {
                        Ok(serde_json::json!({"status":"skipped", "transaction_id": id}))
                    }
                    ConflictStrategy::Error => Err(CliError::Validation(format!(
                        "transaction with reference '{reference}' already exists (id: {id})"
                    ))),
                };
            }
            check_reference(conn, p.company_slug, reference)?;
        }
        let payload = PostingPayload {
            description: p.description.into(),
            metadata: p.metadata.map(str::to_owned),
            currency: p.currency.into(),
            date: p.date.into(),
            entries: p.entries.to_vec(),
            correlate: p.correlate,
            reference: p.reference.map(str::to_owned),
            on_conflict: p.on_conflict,
        };
        let id = ProposalId::new();
        let json = serde_json::to_string(&payload)
            .map_err(|error| CliError::General(error.to_string()))?;
        conn.execute(
            "INSERT INTO txn_proposals(id, company_slug, status, payload, reference, debit_total, currency, proposer_uri, proposer_jti)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id.as_str(), p.company_slug, json, p.reference, super::spend_limits::debit_total(p.entries)?, p.currency, identity.uri(), identity.jti()],
        )?;
        serde_json::to_value(get(conn, p.company_slug, id.as_str())?)
            .map_err(|error| CliError::General(error.to_string()))
    })
}

/// Reject attempts to forge the approval relationship through posting metadata.
///
/// # Errors
/// Returns a validation error if a reserved approval key is present, even null.
pub(crate) fn reject_reserved_metadata(metadata: Option<&str>) -> Result<(), CliError> {
    if let Some(object) = metadata.and_then(|raw| {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw).ok()
    }) && object.contains_key("proposal_id")
    {
        return Err(CliError::Validation(
            "metadata key 'proposal_id' is reserved for approvals".into(),
        ));
    }
    Ok(())
}

pub(crate) fn check_reference(
    conn: &Connection,
    company: &str,
    reference: &str,
) -> Result<(), CliError> {
    let reserved: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM txn_proposals WHERE company_slug = ?1 AND reference = ?2 AND status = 'pending')",
        params![company, reference], |row| row.get(0),
    )?;
    if reserved {
        return Err(CliError::Validation(format!(
            "reference '{reference}' is reserved by a pending proposal"
        )));
    }
    Ok(())
}

/// Revalidate frozen account references, dates, amounts and currency before approval.
pub(crate) fn validate_posting(
    conn: &Connection,
    p: &PostTransactionParams<'_>,
) -> Result<(), CliError> {
    super::spend_limits::debit_total(p.entries)?;
    let currency = beankeeper::types::Currency::from_code(p.currency)
        .map_err(|error| CliError::Validation(error.to_string()))?;
    let date = chrono::NaiveDate::parse_from_str(p.date, "%Y-%m-%d")
        .map_err(|error| CliError::Validation(error.to_string()))?;
    let mut journal = beankeeper::core::JournalEntry::new(date, p.description, currency);
    for entry in p.entries {
        let account = super::accounts::get_account(conn, p.company_slug, &entry.account_code)?;
        let account = super::accounts::row_to_account(&account)?;
        let amount = beankeeper::types::Money::from_minor(i128::from(entry.amount), currency);
        let line = if entry.direction.eq_ignore_ascii_case("debit") {
            beankeeper::types::Entry::debit(account, amount)?
        } else {
            beankeeper::types::Entry::credit(account, amount)?
        };
        journal = journal.entry(line)?;
    }
    journal.post()?;
    Ok(())
}

/// Match every resolution to exactly one immutable audit snapshot, including deletions.
/// Pending requests intentionally have no audit history.
///
/// # Errors
/// Returns database errors for missing, changed or unaudited resolutions.
pub(crate) fn verify_resolutions(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT company_slug, id FROM txn_proposals WHERE status != 'pending'
         UNION SELECT company_slug, entity_id FROM audit_log WHERE entity = 'proposal'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (company, id) = row?;
        let invalid = || {
            CliError::Database(format!(
                "proposal '{id}' does not match its resolution audit record"
            ))
        };
        let proposal = get(conn, &company, &id).map_err(|_| invalid())?;
        let expected = serde_json::to_value(&proposal).map_err(|_| invalid())?;
        let action = if proposal.status == "approved" {
            "proposal_approve"
        } else {
            "proposal_reject"
        };
        let mut audits = conn.prepare("SELECT action, after FROM audit_log WHERE entity = 'proposal' AND company_slug = ?1 AND entity_id = ?2")?;
        let mut audits = audits.query(params![company, id])?;
        let valid = if let Some(row) = audits.next()? {
            let stored_action: String = row.get(0)?;
            let after: String = row.get(1)?;
            stored_action == action
                && proposal.status != "pending"
                && serde_json::from_str::<serde_json::Value>(&after).ok() == Some(expected)
                && audits.next()?.is_none()
        } else {
            false
        };
        if proposal.status == "approved" {
            let transaction_id = proposal.transaction_id.ok_or_else(invalid)?;
            let (transaction, _) =
                super::transactions::get_transaction(conn, &company, transaction_id)
                    .map_err(|_| invalid())?;
            let link = transaction
                .metadata
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
            if link
                .as_ref()
                .and_then(|value| value.get("proposal_id"))
                .and_then(serde_json::Value::as_str)
                != Some(id.as_str())
                || transaction.agent_uri != proposal.resolved_by_uri
                || transaction.token_jti != proposal.resolved_by_jti
            {
                return Err(invalid());
            }
        }
        if !valid {
            return Err(invalid());
        }
    }
    Ok(())
}
