//! Freeze the exact document request until a separately authorized approval.

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    AllocateItem, CreditInput, DocumentOutcome,
    drafts::{self, IssueIntent},
    encode, persist_event, settlement,
};
use crate::{
    capability::Capability,
    db::{
        Db,
        audit::{
            Actor, AuditAction, AuditEntity,
            grant::{Approve, CorrectionApprove, Granted},
            savepoint::AuditedTx,
        },
        companies::{Company, CompanySlug},
        documents::{AllocationId, DocumentDraft, DocumentRecord, DocumentReviewId, decode},
        ids::{ReceiptId, TransactionId},
    },
    error::CliError,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(super) enum ReviewedMutation {
    Issue(IssueIntent),
    Allocate {
        source: TransactionId,
        on: NaiveDate,
        items: Vec<AllocateItem>,
    },
    Credit(CreditInput),
    VatCredit {
        input: CreditInput,
        vat: Box<crate::db::tax_codes::TaxedLine>,
    },
    Revoke {
        id: AllocationId,
        on: NaiveDate,
    },
}

impl ReviewedMutation {
    pub(super) fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        company: &Company<'_>,
    ) -> Result<DocumentOutcome, CliError> {
        match self {
            Self::Issue(intent) => drafts::apply_issue(tx, actor, company, intent),
            Self::Allocate { source, on, items } => {
                settlement::apply_payment(tx, actor, company, source, on, items)
            }
            Self::Credit(input) => settlement::credit(tx, actor, company, input, None),
            Self::VatCredit { input, vat } => {
                settlement::credit(tx, actor, company, input, Some(&vat))
            }
            Self::Revoke { id, on } => settlement::revoke(tx, actor, company, &id, on),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FrozenRequest {
    schema: FrozenSchema,
    mutation: ReviewedMutation,
}

#[derive(Debug, Serialize, Deserialize)]
enum FrozenSchema {
    #[serde(rename = "1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewState {
    Pending,
    Approved,
    Rejected,
}

impl ReviewState {
    const fn code(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ReviewResolution {
    Approved { result: Box<DocumentOutcome> },
    Rejected { reason: String },
}

/// A frozen invoice or bill request and its review decision.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewRecord {
    id: DocumentReviewId,
    proposer: String,
    request: FrozenRequest,
    status: ReviewState,
    resolution: Option<ReviewResolution>,
}

impl ReviewRecord {
    /// The company-scoped document proposal identifier.
    #[must_use]
    pub const fn id(&self) -> &DocumentReviewId {
        &self.id
    }

    /// The verified identity that submitted this document change.
    #[must_use]
    pub fn proposer_uri(&self) -> &str {
        &self.proposer
    }

    /// Whether the request is awaiting a decision.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self.status, ReviewState::Pending)
    }

    /// Load the exact audited draft revision named by an issuance proposal.
    ///
    /// # Errors
    /// Returns an error if the review's frozen draft revision cannot be verified.
    pub fn issuance_terms(&self, company: &Company<'_>) -> Result<Option<DocumentDraft>, CliError> {
        let ReviewedMutation::Issue(intent) = &self.request.mutation else {
            return Ok(None);
        };
        let reader = company.read();
        let raw: String = reader
            .query_row(
                &format!(
                    "SELECT after FROM {} WHERE company_slug=?1 AND entity='document'
                 AND entity_id=?2 AND action IN ('document_draft','document_amend')
                 AND json_extract(after, '$.revision')=?3 ORDER BY id DESC LIMIT 1",
                    reader.bookkeeping_audit()
                ),
                params![company.slug(), intent.id, u32::from(intent.revision)],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                CliError::Database("frozen document draft is missing from audit history".into())
            })?;
        let record: DocumentRecord = decode(&raw)?;
        if record.id != intent.id || record.revision != intent.revision {
            return Err(CliError::Database(
                "frozen document draft differs from the review request".into(),
            ));
        }
        Ok(Some(record.draft))
    }

    /// The proposed document operation in plain language.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match &self.request.mutation {
            ReviewedMutation::Issue(_) => "Issue an invoice or bill",
            ReviewedMutation::Allocate { .. } => "Allocate a payment",
            ReviewedMutation::Credit(_) | ReviewedMutation::VatCredit { .. } => {
                "Apply a document credit"
            }
            ReviewedMutation::Revoke { .. } => "Revoke a document allocation",
        }
    }
}

pub(super) fn reserve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    mutation: ReviewedMutation,
) -> Result<DocumentOutcome, CliError> {
    let proposer = actor.agent_uri().ok_or_else(|| {
        CliError::Unauthorized("document proposals require a verified agent".into())
    })?;
    let request = FrozenRequest {
        schema: FrozenSchema::V1,
        mutation,
    };
    let raw = encode(&request)?;
    if let Some((proposal_id, receipt_id)) = tx.connection().query_row(
        "SELECT id, receipt FROM document_review_requests
         WHERE company_slug=?1 AND proposer=?2 AND status='pending' AND request=?3 ORDER BY id LIMIT 1",
        params![company.slug(), proposer, raw], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()? {
        return Ok(DocumentOutcome::Proposed { proposal_id, receipt_id });
    }
    let record = ReviewRecord {
        id: DocumentReviewId::generate(),
        proposer: proposer.to_owned(),
        request,
        status: ReviewState::Pending,
        resolution: None,
    };
    let receipt_id = persist_event(
        tx,
        actor,
        company.slug(),
        (AuditEntity::DocumentReview, AuditAction::DocumentPropose),
        &record.id,
        (None, &record),
    )?;
    tx.connection().execute(
        "INSERT INTO document_review_requests(company_slug,id,request,proposer,status,receipt)
         VALUES(?1,?2,?3,?4,'pending',?5)",
        params![company.slug(), record.id, raw, record.proposer, receipt_id],
    )?;
    Ok(DocumentOutcome::Proposed {
        proposal_id: record.id,
        receipt_id,
    })
}

fn review_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(DocumentReviewId, String, String, String, Option<String>)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

type StoredReview = (DocumentReviewId, String, String, String, Option<String>);

fn decode_review(
    (id, proposer, request, state, resolution): StoredReview,
) -> Result<ReviewRecord, CliError> {
    let status = match state.as_str() {
        "pending" => ReviewState::Pending,
        "approved" => ReviewState::Approved,
        "rejected" => ReviewState::Rejected,
        _ => {
            return Err(CliError::Database(
                "invalid stored document proposal status".into(),
            ));
        }
    };
    Ok(ReviewRecord {
        id,
        proposer,
        request: decode(&request)?,
        status,
        resolution: resolution.as_deref().map(decode).transpose()?,
    })
}

/// List visible document proposals in the selected company, optionally including resolved records.
///
/// # Errors
/// Returns ledger or authorization errors.
pub fn list(
    company: &Company<'_>,
    actor: &Actor,
    include_resolved: bool,
) -> Result<Vec<ReviewRecord>, CliError> {
    let can_approve = may_review(actor)?;
    let mut statement = company.read().prepare(
        "SELECT id, proposer, request, status, resolution FROM document_review_requests
         WHERE company_slug=?1 AND (?2 OR proposer=?3) AND (?4 OR status='pending')
         ORDER BY id DESC LIMIT 100",
    )?;
    let rows = statement.query_map(
        params![
            company.slug(),
            can_approve,
            actor.agent_uri(),
            include_resolved
        ],
        review_row,
    )?;
    rows.map(|row| decode_review(row?)).collect()
}

/// Read an individual document request visible to the reviewer.
///
/// # Errors
/// Returns ledger or authorization errors.
pub fn get(
    company: &Company<'_>,
    actor: &Actor,
    id: &DocumentReviewId,
) -> Result<Option<ReviewRecord>, CliError> {
    let can_approve = may_review(actor)?;
    company
        .read()
        .query_row(
            "SELECT id, proposer, request, status, resolution FROM document_review_requests
         WHERE company_slug=?1 AND id=?2 AND (?3 OR proposer=?4)",
            params![company.slug(), id, can_approve, actor.agent_uri()],
            review_row,
        )
        .optional()?
        .map(decode_review)
        .transpose()
}

fn may_review(actor: &Actor) -> Result<bool, CliError> {
    Ok(actor
        .identity()
        .map(|identity| identity.allows(Capability::Approve))
        .transpose()?
        .unwrap_or(false))
}

/// A decision on a frozen document request.
pub enum ApprovalAction {
    /// Apply the request.
    Approve,
    /// Decline and record the reviewer's reason.
    Reject(String),
}

pub(crate) fn review(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DocumentReviewId,
    action: ApprovalAction,
) -> Result<ReviewRecord, CliError> {
    Granted::<CorrectionApprove>::with_db(db, actor, |grant| decide(&grant, slug, id, action))
}

/// Resolve a document proposal using a writer grant already held by the caller.
///
/// # Errors
/// Requires signed independent approval, matching frozen document terms, and an open ledger.
pub fn decide_document(
    grant: &Granted<'_, Approve>,
    slug: &CompanySlug,
    id: &DocumentReviewId,
    action: ApprovalAction,
) -> Result<ReviewRecord, CliError> {
    grant.mutate_in_scope("sp_document_review_grant", |tx, actor| {
        let approver = Granted::<CorrectionApprove>::check(tx, actor)?;
        decide(&approver, slug, id, action)
    })
}

fn decide(
    grant: &Granted<'_, CorrectionApprove>,
    slug: &CompanySlug,
    id: &DocumentReviewId,
    action: ApprovalAction,
) -> Result<ReviewRecord, CliError> {
    grant.mutate_in_scope("sp_document_review", |tx, actor| {
        let company = Company::bind(tx.read(), slug.clone())?;
        let before = tx
            .connection()
            .query_row(
                "SELECT id, proposer, request, status, resolution FROM document_review_requests
             WHERE company_slug=?1 AND id=?2",
                params![slug, id],
                review_row,
            )
            .optional()?
            .ok_or_else(|| {
                CliError::NotFound(format!("document proposal '{id}' not found in '{slug}'"))
            })?;
        let mut record = decode_review(before)?;
        if record.status != ReviewState::Pending {
            if matches!(
                (&action, record.status),
                (ApprovalAction::Approve, ReviewState::Approved)
                    | (ApprovalAction::Reject(_), ReviewState::Rejected)
            ) {
                return Ok(record);
            }
            return Err(CliError::Validation(
                "document proposal was already resolved".into(),
            ));
        }
        let old = encode(&record)?;
        let (status, resolution, change) = match action {
            ApprovalAction::Approve => {
                if Some(record.proposer.as_str()) == actor.agent_uri() {
                    return Err(CliError::Forbidden(
                        "the proposer cannot approve their own request".into(),
                    ));
                }
                (
                    ReviewState::Approved,
                    ReviewResolution::Approved {
                        result: Box::new(
                            record.request.mutation.clone().apply(tx, actor, &company)?,
                        ),
                    },
                    AuditAction::DocumentApprove,
                )
            }
            ApprovalAction::Reject(reason) => {
                let reason = reason.trim();
                if reason.is_empty() || reason.len() > 500 {
                    return Err(CliError::Validation(
                        "reason must contain 1 to 500 bytes".into(),
                    ));
                }
                (
                    ReviewState::Rejected,
                    ReviewResolution::Rejected {
                        reason: reason.to_owned(),
                    },
                    AuditAction::DocumentReject,
                )
            }
        };
        record.status = status;
        let raw_resolution = encode(&resolution)?;
        record.resolution = Some(resolution);
        persist_resolution(tx, actor, slug, record, change, (&old, &raw_resolution))
    })
}

fn persist_resolution(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    record: ReviewRecord,
    change: AuditAction,
    (old, raw_resolution): (&str, &str),
) -> Result<ReviewRecord, CliError> {
    let before = decode::<ReviewRecord>(old)?;
    let receipt: ReceiptId = persist_event(
        tx,
        actor,
        slug,
        (AuditEntity::DocumentReview, change),
        &record.id,
        (Some(&before), &record),
    )?;
    tx.connection().execute(
        "UPDATE document_review_requests SET status=?1,resolution=?2,receipt=?3
          WHERE company_slug=?4 AND id=?5 AND status='pending'",
        params![
            record.status.code(),
            raw_resolution,
            receipt,
            slug,
            record.id
        ],
    )?;
    Ok(record)
}
