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
            grant::{CorrectionApprove, Granted},
            savepoint::AuditedTx,
        },
        companies::{Company, CompanySlug},
        documents::{AllocationId, DocumentReviewId, decode},
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
            Self::Credit(input) => settlement::credit(tx, actor, company, input),
            Self::Revoke { id, on } => settlement::revoke(tx, actor, company, id, on),
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

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReviewRecord {
    id: DocumentReviewId,
    proposer: String,
    request: FrozenRequest,
    status: ReviewState,
    resolution: Option<ReviewResolution>,
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
        None,
        &record,
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

pub(crate) fn review_requests(
    company: &Company<'_>,
    actor: &Actor,
) -> Result<Vec<ReviewRecord>, CliError> {
    let viewer = actor.identity();
    let can_approve = viewer
        .map(|identity| identity.allows(Capability::Approve))
        .transpose()?
        .unwrap_or(false);
    let viewer_uri = actor.agent_uri();
    let mut statement = company.read().prepare(
        "SELECT id, proposer, request, status, resolution FROM document_review_requests
         WHERE company_slug=?1 AND (?2 OR proposer=?3) ORDER BY id DESC LIMIT 100",
    )?;
    let rows = statement.query_map(params![company.slug(), can_approve, viewer_uri], review_row)?;
    rows.map(|row| decode_review(row?)).collect()
}

pub(crate) enum ApprovalAction {
    Approve,
    Reject(String),
}

pub(crate) fn review(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: DocumentReviewId,
    action: ApprovalAction,
) -> Result<ReviewRecord, CliError> {
    Granted::<CorrectionApprove>::with_db(db, actor, |grant| {
        grant.mutate("sp_document_review", |tx, actor| {
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
            persist_resolution(tx, actor, slug, record, change, &old, raw_resolution)
        })
    })
}

fn persist_resolution(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    record: ReviewRecord,
    change: AuditAction,
    old: &str,
    raw_resolution: String,
) -> Result<ReviewRecord, CliError> {
    let before = decode::<ReviewRecord>(old)?;
    let receipt: ReceiptId = persist_event(
        tx,
        actor,
        slug,
        (AuditEntity::DocumentReview, change),
        &record.id,
        Some(&before),
        &record,
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
