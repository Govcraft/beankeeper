//! Audit-owned journal and settlement operations for open items.

mod drafts;
mod review;
mod settlement;
mod verification;

use chrono::NaiveDate;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        Db,
        companies::{Company, CompanySlug},
        documents::{
            AllocationHistory, AllocationId, DocumentDraft, DocumentId, DocumentRecord,
            DocumentReviewId, DocumentRevision, find_document,
        },
        ids::{ReceiptId, TransactionId},
    },
    error::CliError,
};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links,
    grant::PostingGrant, savepoint::AuditedTx,
};

pub use settlement::{AllocateItem, AllocationResult, CreditInput};
pub(crate) use verification::verify;

/// A document write routed through the normal posting or proposal authority.
pub(crate) enum DocumentMutation {
    Draft(DocumentDraft),
    Amend(DocumentId, DocumentRevision, DocumentDraft),
    Issue(DocumentId, NaiveDate),
    Allocate(TransactionId, NaiveDate, Vec<AllocateItem>),
    Credit(CreditInput),
    Revoke(AllocationId, NaiveDate),
}

pub(crate) use review::{ApprovalAction, ReviewRecord, review, review_requests};

/// Result of an accounting document mutation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DocumentOutcome {
    /// Current document and change attribution.
    Document {
        record: Box<DocumentRecord>,
        receipt_id: Option<ReceiptId>,
    },
    /// A batch of applied settlements.
    Allocations {
        items: Vec<settlement::AllocationResult>,
        /// Part of the payment left unapplied on the effective date.
        #[serde(skip_serializing_if = "Option::is_none")]
        unapplied_minor: Option<i64>,
    },
    /// An allocation that was removed from active settlement.
    Revoked {
        allocation: AllocationHistory,
        receipt_id: Option<ReceiptId>,
    },
    /// A frozen request ready for an owner.
    Proposed {
        proposal_id: DocumentReviewId,
        receipt_id: ReceiptId,
    },
}

pub(crate) fn submit(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    mutation: DocumentMutation,
) -> Result<DocumentOutcome, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => grant.mutate("sp_document_direct", |tx, actor| {
            mutate(tx, actor, company, mutation, Execution::Direct)
        }),
        PostingGrant::Proposal(grant) => grant.mutate("sp_document_proposal", |tx, actor| {
            mutate(tx, actor, company, mutation, Execution::Propose)
        }),
    })
}

#[derive(Clone, Copy)]
enum Execution {
    Direct,
    Propose,
}

fn mutate(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    mutation: DocumentMutation,
    execution: Execution,
) -> Result<DocumentOutcome, CliError> {
    let company = Company::bind(tx.read(), slug.clone())?;
    let reviewed = match mutation {
        DocumentMutation::Draft(draft) => {
            return drafts::save_draft(tx, actor, &company, None, draft);
        }
        DocumentMutation::Amend(id, version, draft) => {
            return drafts::save_draft(tx, actor, &company, Some((&id, version)), draft);
        }
        DocumentMutation::Issue(id, date) => {
            let record = find_document(&company, &id)?;
            if let Some(issued) = &record.issued {
                return drafts::repeat_issue(&record, issued, date);
            }
            review::ReviewedMutation::Issue(drafts::prepare_issue(&company, &record, date)?)
        }
        DocumentMutation::Allocate(source, on, items) => {
            review::ReviewedMutation::Allocate { source, on, items }
        }
        DocumentMutation::Credit(input) => settlement::prepare_credit(&company, input)?,
        DocumentMutation::Revoke(id, on) => review::ReviewedMutation::Revoke { id, on },
    };
    match execution {
        Execution::Direct => reviewed.apply(tx, actor, &company),
        Execution::Propose => review::reserve(tx, actor, &company, reviewed),
    }
}

pub(super) fn guard_linked_posting(
    company: &Company<'_>,
    transaction: TransactionId,
) -> Result<(), CliError> {
    guard_posting(company, transaction, None)
}

pub(super) fn guard_linked_reversal(
    company: &Company<'_>,
    transaction: TransactionId,
    date: NaiveDate,
) -> Result<(), CliError> {
    guard_posting(company, transaction, Some(date))
}

fn guard_posting(
    company: &Company<'_>,
    transaction: TransactionId,
    reversal_date: Option<NaiveDate>,
) -> Result<(), CliError> {
    if company.read().schema_version()? < crate::db::schema::DOCUMENT_SCHEMA_VERSION {
        return Ok(());
    }
    let protected: bool = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM book_documents WHERE company_slug=?1 AND posting_id=?2)
           OR EXISTS(SELECT 1 FROM document_allocations WHERE company_slug=?1 AND source_id=?2
                     AND (kind='credit' OR ?3 IS NULL OR reversed_on IS NULL OR reversed_on>?3))",
        params![company.slug(), transaction, reversal_date],
        |row| row.get(0),
    )?;
    if protected {
        Err(CliError::Validation(format!(
            "posting '{transaction}' belongs to an open-item document. correct it using a credit or revoke its payment allocation first"
        )))
    } else {
        Ok(())
    }
}

fn encode(value: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(value)
        .map_err(|error| CliError::Database(format!("could not encode document data: {error}")))
}

fn persist_event<T: Serialize>(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: (AuditEntity, AuditAction),
    id: &impl std::fmt::Display,
    (before, after): (Option<&T>, &T),
) -> Result<ReceiptId, CliError> {
    let (entity, action) = event;
    let before = before.map(encode).transpose()?;
    let after = encode(after)?;
    append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity,
            action,
            entity_id: &id.to_string(),
            snapshots: (before.as_deref(), Some(&after)),
        },
        AuditLinks::None,
    )
}
