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

/// Who resolved a proposal, preserving local resolutions made without a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedBy {
    /// Local operator.
    Local,
    /// An operator with a verified agent token.
    Agent {
        /// Agent URI on record.
        uri: String,
        /// ID of the token used for this resolution.
        jti: String,
    },
}

impl ResolvedBy {
    fn from_columns(uri: Option<String>, jti: Option<String>) -> Option<Self> {
        match (uri, jti) {
            (None, None) => Some(Self::Local),
            (Some(uri), Some(jti)) => Some(Self::Agent { uri, jti }),
            _ => None,
        }
    }

    pub(super) fn columns(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::Local => (None, None),
            Self::Agent { uri, jti } => (Some(uri), Some(jti)),
        }
    }
}

/// Resolution attribution shared by each terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Stored UTC resolution timestamp.
    pub at: String,
    /// Operator who resolved the proposal.
    pub by: ResolvedBy,
}

impl Resolution {
    fn new(conn: &Connection, actor: &Actor) -> Result<Self, CliError> {
        let at = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
        let by = actor
            .identity()
            .map_or(ResolvedBy::Local, |identity| ResolvedBy::Agent {
                uri: identity.uri().to_string(),
                jti: identity.jti().to_string(),
            });
        Ok(Self { at, by })
    }
}

/// Review lifecycle. Approved proposals always identify their transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalState {
    /// Awaiting a resolution.
    Pending,
    /// Approved and posted.
    Approved {
        /// When and by whom the proposal was approved.
        resolution: Resolution,
        /// Transaction created for this proposal.
        transaction_id: i64,
    },
    /// Declined by a reviewer.
    Rejected {
        /// When and by whom the proposal was rejected.
        resolution: Resolution,
        /// Optional reviewer explanation.
        reason: Option<String>,
    },
    /// Withdrawn by the proposer.
    Withdrawn {
        /// When and by whom the proposal was withdrawn.
        resolution: Resolution,
        /// Optional explanation from the proposer.
        reason: Option<String>,
    },
}

impl ProposalState {
    /// Status label in existing clients and SQLite.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved { .. } => "approved",
            Self::Rejected { .. } => "rejected",
            Self::Withdrawn { .. } => "withdrawn",
        }
    }

    /// Optional rejection or withdrawal reason.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Rejected { reason, .. } | Self::Withdrawn { reason, .. } => reason.as_deref(),
            Self::Pending | Self::Approved { .. } => None,
        }
    }

    fn resolution(&self) -> Option<&Resolution> {
        match self {
            Self::Pending => None,
            Self::Approved { resolution, .. }
            | Self::Rejected { resolution, .. }
            | Self::Withdrawn { resolution, .. } => Some(resolution),
        }
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status = row.get_ref(2)?.as_str()?;
        let at: Option<String> = row.get(10)?;
        let by = ResolvedBy::from_columns(row.get(11)?, row.get(12)?);
        let transaction_id: Option<i64> = row.get(13)?;
        let reason: Option<String> = row.get(14)?;
        match (status, at, by, transaction_id, reason) {
            ("pending", None, Some(ResolvedBy::Local), None, None) => Ok(Self::Pending),
            ("approved", Some(at), Some(by), Some(transaction_id), None) => Ok(Self::Approved {
                resolution: Resolution { at, by },
                transaction_id,
            }),
            ("rejected", Some(at), Some(by), None, reason) => Ok(Self::Rejected {
                resolution: Resolution { at, by },
                reason,
            }),
            ("withdrawn", Some(at), Some(by), None, reason) => Ok(Self::Withdrawn {
                resolution: Resolution { at, by },
                reason,
            }),
            _ => Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(CliError::Database(format!(
                    "invalid stored proposal resolution for status '{status}'"
                ))),
            )),
        }
    }
}

/// Reviewable proposal and its immutable proposer attribution and resolution.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub id: ProposalId,
    pub company_slug: String,
    pub state: ProposalState,
    pub payload: PostingPayload,
    pub reference: Option<String>,
    pub debit_total: i64,
    pub currency: String,
    pub proposer_uri: String,
    pub proposer_jti: String,
    pub created_at: String,
}

impl Serialize for Proposal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;

        let resolution = self.state.resolution();
        let transaction_id = match &self.state {
            ProposalState::Approved { transaction_id, .. } => Some(*transaction_id),
            _ => None,
        };
        let (uri, jti) = resolution.map_or((None, None), |value| value.by.columns());
        let mut wire = serializer.serialize_struct("Proposal", 15)?;
        wire.serialize_field("id", &self.id)?;
        wire.serialize_field("company_slug", &self.company_slug)?;
        wire.serialize_field("status", self.state.code())?;
        wire.serialize_field("payload", &self.payload)?;
        wire.serialize_field("reference", &self.reference)?;
        wire.serialize_field("debit_total", &self.debit_total)?;
        wire.serialize_field("currency", &self.currency)?;
        wire.serialize_field("proposer_uri", &self.proposer_uri)?;
        wire.serialize_field("proposer_jti", &self.proposer_jti)?;
        wire.serialize_field("created_at", &self.created_at)?;
        wire.serialize_field("resolved_at", &resolution.map(|value| value.at.as_str()))?;
        wire.serialize_field("resolved_by_uri", &uri)?;
        wire.serialize_field("resolved_by_jti", &jti)?;
        wire.serialize_field("transaction_id", &transaction_id)?;
        wire.serialize_field("reason", &self.state.reason())?;
        wire.end()
    }
}

/// A pending proposal loaded from the same book that will resolve it.
struct PendingProposal<'a> {
    conn: &'a Connection,
    before: Proposal,
}

impl<'a> PendingProposal<'a> {
    fn load(conn: &'a Connection, company: &str, id: &str) -> Result<Self, CliError> {
        verify_resolutions(conn)?;
        let before = get(conn, company, id)?;
        if before.state != ProposalState::Pending {
            return Err(CliError::Validation(format!(
                "proposal '{id}' is already {}",
                before.state.code(),
            )));
        }
        Ok(Self { conn, before })
    }

    fn approve(self, grant: &ApprovalGrant<'_>) -> Result<(Proposal, Proposal), CliError> {
        let actor = grant.actor;
        let mut payload = self.before.payload.clone();
        validate_posting(self.conn, &payload.params(&self.before.company_slug, actor))?;
        let (existing, proposal_id) = (payload.metadata.as_deref(), self.before.id.as_str());
        let mut metadata: serde_json::Value =
            existing.and_then(super::metadata::normalize).map_or_else(
                || serde_json::json!({}),
                super::metadata::NormalizedMetadata::into_value,
            );
        metadata["proposal_id"] = serde_json::json!(proposal_id);
        payload.metadata = Some(metadata.to_string());
        let posting = ApprovedPosting {
            conn: self.conn,
            company: &self.before.company_slug,
            actor,
            payload,
        };
        let transaction_id = match super::transactions::post_approved_transaction(posting)? {
            super::transactions::PostResult::Created(id) => id,
            super::transactions::PostResult::Skipped(_) => {
                return Err(CliError::Validation(
                    "proposal reference was already posted; withdraw this proposal".into(),
                ));
            }
        };
        let state = ProposalState::Approved {
            resolution: Resolution::new(self.conn, actor)?,
            transaction_id,
        };
        Ok(self.transition(state))
    }

    fn reject(self, actor: &Actor, reason: Option<&str>) -> Result<(Proposal, Proposal), CliError> {
        let own = actor.agent_uri() == Some(self.before.proposer_uri.as_str());
        let can_approve = actor
            .identity()
            .map(|identity| identity.allows(Capability::Approve))
            .transpose()?
            .unwrap_or(false);
        let required = if own && !can_approve {
            Capability::Propose
        } else {
            Capability::Approve
        };
        audit::require_capability(self.conn, actor, required)?;
        let resolution = Resolution::new(self.conn, actor)?;
        let reason = reason.map(str::to_owned);
        let state = if own {
            ProposalState::Withdrawn { resolution, reason }
        } else {
            ProposalState::Rejected { resolution, reason }
        };
        Ok(self.transition(state))
    }

    fn transition(self, state: ProposalState) -> (Proposal, Proposal) {
        let mut after = self.before.clone();
        after.state = state;
        (self.before, after)
    }
}

/// Authorization checked before any proposal details are loaded.
struct ApprovalGrant<'a> {
    actor: &'a Actor,
}

impl<'a> ApprovalGrant<'a> {
    fn authorize(conn: &Connection, actor: &'a Actor) -> Result<Self, CliError> {
        audit::require_capability(conn, actor, Capability::Approve)?;
        Ok(Self { actor })
    }
}

pub(super) fn approve(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &str,
) -> Result<Proposal, CliError> {
    audit::atomically(conn, || {
        let grant = ApprovalGrant::authorize(conn, actor)?;
        let (before, after) = PendingProposal::load(conn, company, id)?.approve(&grant)?;
        audit::record_proposal_resolution(ProposalTransition {
            conn,
            actor,
            before,
            after,
        })
    })
}

pub(super) fn reject(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &str,
    reason: Option<&str>,
) -> Result<Proposal, CliError> {
    audit::atomically(conn, || {
        let (before, after) = PendingProposal::load(conn, company, id)?.reject(actor, reason)?;
        audit::record_proposal_resolution(ProposalTransition {
            conn,
            actor,
            before,
            after,
        })
    })
}

/// A consumed pending proposal and its authorized resolution.
pub(super) struct ProposalTransition<'a> {
    conn: &'a Connection,
    actor: &'a Actor,
    before: Proposal,
    after: Proposal,
}

impl<'a> ProposalTransition<'a> {
    pub(super) fn into_parts(self) -> (&'a Connection, &'a Actor, Proposal, Proposal) {
        (self.conn, self.actor, self.before, self.after)
    }
}

/// The exact frozen posting being approved. Its only constructor is approval
/// of a pending proposal, and it is consumed on the originating connection.
pub(super) struct ApprovedPosting<'a> {
    conn: &'a Connection,
    company: &'a str,
    actor: &'a Actor,
    payload: PostingPayload,
}

impl<'a> ApprovedPosting<'a> {
    pub(super) fn connection(&self) -> &'a Connection {
        self.conn
    }

    pub(super) fn params(&self) -> PostTransactionParams<'_> {
        self.payload.params(self.company, self.actor)
    }
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
        state: ProposalState::from_row(row)?,
        payload,
        reference: row.get(4)?,
        debit_total: row.get(5)?,
        currency: row.get(6)?,
        proposer_uri: row.get(7)?,
        proposer_jti: row.get(8)?,
        created_at: row.get(9)?,
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
        let action = match proposal.state {
            ProposalState::Pending => return Err(invalid()),
            ProposalState::Approved { .. } => "proposal_approve",
            ProposalState::Rejected { .. } | ProposalState::Withdrawn { .. } => "proposal_reject",
        };
        let mut audits = conn.prepare("SELECT action, after FROM audit_log WHERE entity = 'proposal' AND company_slug = ?1 AND entity_id = ?2")?;
        let mut audits = audits.query(params![company, id])?;
        let valid = if let Some(row) = audits.next()? {
            let stored_action: String = row.get(0)?;
            let after: String = row.get(1)?;
            stored_action == action
                && serde_json::from_str::<serde_json::Value>(&after).ok() == Some(expected)
                && audits.next()?.is_none()
        } else {
            false
        };
        if let ProposalState::Approved {
            resolution,
            transaction_id,
        } = &proposal.state
        {
            let (uri, jti) = resolution.by.columns();
            let (transaction, _) =
                super::transactions::get_transaction(conn, &company, *transaction_id)
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
                || transaction.agent_uri.as_deref() != uri
                || transaction.token_jti.as_deref() != jti
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
