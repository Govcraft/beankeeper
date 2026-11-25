//! Corrections use the same review commands as posting and rename proposals.

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, Company, ReadConn, ReversalSource,
    ReverseTransactionParams,
    grant::{
        AgentUndo, Approve, CorrectionApprove, CorrectionPropose, Granted, OwnerUndo, PostingGrant,
        ResolveProposal, Reverse,
    },
    recategorize::{self, Recategorization},
    receipts::{self, AgentReceiptUndo, ReceiptId, ReceiptUndoOutcome},
    record, reverse_in_scope, reverse_transaction,
    savepoint::AuditedTx,
    snapshot,
};
use crate::{
    commands::reference::TransactionReference,
    db::{
        Db,
        companies::CompanySlug,
        ids::TransactionId,
        proposals::{self, ProposalId},
        transactions::PostResult,
    },
    error::CliError,
    identity::{VerifiedAgent, WriteMode},
    request::LocalDate,
};

/// Validated inputs to reverse an existing transaction.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reversal {
    id: TransactionId,
    date: Option<NaiveDate>,
    description: Option<String>,
    reference: Option<TransactionReference>,
}

impl
    From<(
        TransactionId,
        Option<NaiveDate>,
        Option<String>,
        Option<TransactionReference>,
    )> for Reversal
{
    fn from(
        (id, date, description, reference): (
            TransactionId,
            Option<NaiveDate>,
            Option<String>,
            Option<TransactionReference>,
        ),
    ) -> Self {
        Self {
            id,
            date,
            description,
            reference,
        }
    }
}

impl Reversal {
    pub(crate) const fn id(&self) -> TransactionId {
        self.id
    }

    fn params<'a>(
        &'a self,
        company: &'a CompanySlug,
        requested_on: NaiveDate,
    ) -> ReverseTransactionParams<'a> {
        ReverseTransactionParams {
            company_slug: company,
            id: self.id,
            date: self.date.unwrap_or(requested_on),
            description: self.description.as_deref(),
            reference: self.reference.as_ref().map(TransactionReference::as_str),
        }
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Correction {
    Reverse(Reversal),
    Recategorize(Recategorization),
    Undo { receipt: ReceiptId },
}

impl Correction {
    fn same_intent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Recategorize(left), Self::Recategorize(right)) => left.same_intent(right),
            _ => self == other,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    correction: Correction,
    requested_on: NaiveDate,
}

impl From<Correction> for Request {
    fn from(correction: Correction) -> Self {
        Self {
            correction,
            requested_on: LocalDate::default().into(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct PendingCorrection {
    proposal_id: ProposalId,
    status: Pending,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Pending {
    Pending,
}

impl From<ProposalId> for PendingCorrection {
    fn from(proposal_id: ProposalId) -> Self {
        Self {
            proposal_id,
            status: Pending::Pending,
        }
    }
}

impl PendingCorrection {
    pub(crate) const fn id(&self) -> &ProposalId {
        &self.proposal_id
    }
}

pub(crate) enum ReverseSubmission {
    Applied(AppliedReversal),
    Proposed(PendingCorrection),
}

pub(crate) struct AppliedReversal {
    result: PostResult,
    receipt: ReceiptId,
}

impl AppliedReversal {
    fn load(
        reader: ReadConn<'_>,
        company: &CompanySlug,
        result: PostResult,
    ) -> Result<Self, CliError> {
        let (PostResult::Created(id) | PostResult::Skipped(id)) = result;
        let receipt = reader.query_row(
            "SELECT id FROM audit_log WHERE company_slug = ?1 AND related_posting = ?2 AND action = 'reverse'",
            params![company, id],
            |row| row.get(0),
        )?;
        Ok(Self { result, receipt })
    }

    pub(crate) const fn result(&self) -> PostResult {
        self.result
    }

    pub(crate) const fn receipt(&self) -> ReceiptId {
        self.receipt
    }

    const fn reversal_id(&self) -> TransactionId {
        match self.result {
            PostResult::Created(id) | PostResult::Skipped(id) => id,
        }
    }
}

pub(crate) enum RecategorizeSubmission {
    Applied(recategorize::Applied),
    Proposed(PendingCorrection),
}

pub(crate) fn recategorize(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    request: Recategorization,
) -> Result<RecategorizeSubmission, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => grant.mutate("sp_recategorize", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            request
                .apply(tx, actor, &scope)
                .map(RecategorizeSubmission::Applied)
        }),
        PostingGrant::Proposal(grant) => {
            grant.mutate("sp_propose_recategorization", |tx, actor| {
                let scope = Company::bind(tx.read(), company.clone())?;
                if let Some(applied) = request.preview(tx, &scope)? {
                    return Ok(RecategorizeSubmission::Applied(applied));
                }
                reserve(tx, actor, &scope, &Correction::Recategorize(request).into())
                    .map(RecategorizeSubmission::Proposed)
            })
        }
    })
}

pub(crate) enum UndoSubmission {
    Applied(ReceiptUndoOutcome),
    Proposed(PendingCorrection),
}

pub(crate) fn reverse(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    reversal: Reversal,
) -> Result<ReverseSubmission, CliError> {
    if actor.identity().map(VerifiedAgent::write_mode) == Some(WriteMode::ProposeOnly) {
        return Granted::<CorrectionPropose>::with_db(db, actor, |grant| {
            grant.mutate("sp_propose_reversal", |tx, actor| {
                let scope = Company::bind(tx.read(), company.clone())?;
                reserve(tx, actor, &scope, &Correction::Reverse(reversal).into())
                    .map(ReverseSubmission::Proposed)
            })
        });
    }
    Granted::<Reverse>::with_db(db, actor, |grant| {
        let result = reverse_transaction(
            grant,
            &reversal.params(company, LocalDate::default().into()),
        )?;
        AppliedReversal::load(db.read(), company, result).map(ReverseSubmission::Applied)
    })
}

pub(crate) fn undo(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<UndoSubmission, CliError> {
    if let Some(identity) = actor.identity() {
        if super::grant::permits::<OwnerUndo>(identity)? {
            return Granted::<OwnerUndo>::with_db(db, actor, |grant| {
                receipts::undo_change(grant, company, receipt).map(UndoSubmission::Applied)
            });
        }
        if identity.write_mode() == WriteMode::ProposeOnly {
            if super::statement::is_statement_receipt(db.read(), company, receipt)? {
                return Err(CliError::Forbidden(
                    "statement undo requires direct receipt undo authority".into(),
                ));
            }
            return Granted::<CorrectionPropose>::with_db(db, actor, |grant| {
                grant.mutate("sp_propose_undo", |tx, actor| {
                    let scope = Company::bind_retained(tx.read(), company.clone())?;
                    let target = AgentReceiptUndo::load(&scope, receipt)?;
                    if let Some(outcome) = target.recorded(&scope)? {
                        return Ok(UndoSubmission::Applied(ReceiptUndoOutcome::Single(outcome)));
                    }
                    reserve(tx, actor, &scope, &Correction::Undo { receipt }.into())
                        .map(UndoSubmission::Proposed)
                })
            });
        }
    }
    Granted::<AgentUndo>::with_db(db, actor, |grant| {
        receipts::undo_as_agent(grant, company, receipt)
            .map(ReceiptUndoOutcome::Single)
            .map(UndoSubmission::Applied)
    })
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
enum Applied {
    Recategorized(Box<recategorize::Applied>),
    Reversed {
        transaction: TransactionId,
        reversal: TransactionId,
        receipt: ReceiptId,
    },
    Undone {
        undo_of: ReceiptId,
        undo_receipt: ReceiptId,
    },
}

impl Applied {
    fn desktop_path(&self, company: &CompanySlug) -> String {
        let receipt = match *self {
            Self::Recategorized(ref applied) => return applied.desktop_path(company),
            Self::Undone { undo_receipt, .. } => undo_receipt,
            Self::Reversed { receipt, .. } => receipt,
        };
        receipts::desktop_receipt_path(company, receipt)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Resolution {
    Approved {
        #[serde(flatten)]
        applied: Applied,
    },
    Rejected {
        reason: Option<String>,
    },
    Withdrawn {
        reason: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReviewState {
    Pending {
        status: Pending,
    },
    Resolved {
        #[serde(flatten)]
        resolution: Resolution,
        desktop_path: Option<String>,
    },
}

#[derive(Serialize)]
pub(crate) struct CorrectionProposal {
    id: ProposalId,
    kind: ProposalKind,
    company_slug: CompanySlug,
    proposer_uri: String,
    created_at: String,
    payload: Request,
    #[serde(flatten)]
    state: ReviewState,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ProposalKind {
    Correction,
}

struct StoredProposal {
    id: ProposalId,
    proposer_uri: String,
    created_at: String,
    request: Request,
    resolution: Option<Resolution>,
}

type ProposalRow = (ProposalId, String, String, String, Option<String>);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProposalRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

impl TryFrom<ProposalRow> for StoredProposal {
    type Error = CliError;

    fn try_from(
        (id, proposer_uri, created_at, requested, resolved): ProposalRow,
    ) -> Result<Self, CliError> {
        fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, CliError> {
            serde_json::from_str(raw).map_err(|error| {
                CliError::Database(format!("invalid stored correction proposal: {error}"))
            })
        }
        Ok(Self {
            id,
            proposer_uri,
            created_at,
            request: decode(&requested)?,
            resolution: resolved.as_deref().map(decode).transpose()?,
        })
    }
}

impl StoredProposal {
    fn review(self, company: &CompanySlug) -> CorrectionProposal {
        let state = if let Some(resolution) = self.resolution {
            let desktop_path = match &resolution {
                Resolution::Approved { applied } => Some(applied.desktop_path(company)),
                _ => None,
            };
            ReviewState::Resolved {
                resolution,
                desktop_path,
            }
        } else {
            ReviewState::Pending {
                status: Pending::Pending,
            }
        };
        CorrectionProposal {
            id: self.id,
            kind: ProposalKind::Correction,
            company_slug: company.clone(),
            proposer_uri: self.proposer_uri,
            created_at: self.created_at,
            payload: self.request,
            state,
        }
    }
}

const SELECT: &str = "SELECT proposed.entity_id, proposed.agent_uri, proposed.changed_at,
       proposed.after, resolved.after
     FROM audit_log proposed
     LEFT JOIN audit_log resolved ON resolved.company_slug = proposed.company_slug
       AND resolved.entity = proposed.entity AND resolved.entity_id = proposed.entity_id
       AND resolved.action = 'correction_resolve'
     WHERE proposed.company_slug = ?1 AND proposed.action = 'correction_propose'";

fn reserve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    request: &Request,
) -> Result<PendingCorrection, CliError> {
    let mut statement = tx.read().prepare(&format!(
        "{SELECT} AND resolved.id IS NULL AND proposed.agent_uri = ?2 ORDER BY proposed.id"
    ))?;
    for row in statement.query_map(params![scope.slug(), actor.agent_uri()], read_row)? {
        let proposal = StoredProposal::try_from(row?)?;
        if proposal.request.correction.same_intent(&request.correction) {
            return Ok(proposal.id.into());
        }
    }
    let id = ProposalId::new();
    record(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(scope.slug()),
            entity: AuditEntity::CorrectionProposal,
            entity_id: id.as_str(),
            action: AuditAction::CorrectionPropose,
            snapshots: (None, Some(&snapshot(request))),
        },
    )?;
    Ok(id.into())
}

pub(crate) fn list(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    actor: &Actor,
    include_resolved: bool,
) -> Result<Vec<CorrectionProposal>, CliError> {
    let all = proposals::can_review_all(reader, actor)?;
    let mut statement = reader.prepare(&format!(
        "{SELECT} AND (?2 OR resolved.id IS NULL) AND (?3 OR proposed.agent_uri = ?4)
         ORDER BY proposed.id"
    ))?;
    statement
        .query_map(
            params![company, include_resolved, all, actor.agent_uri()],
            read_row,
        )?
        .map(|row| Ok(StoredProposal::try_from(row?)?.review(company)))
        .collect()
}

fn find(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    id: &ProposalId,
) -> Result<Option<StoredProposal>, CliError> {
    tx.read()
        .query_row(
            &format!("{SELECT} AND proposed.entity_id = ?2"),
            params![company, id],
            read_row,
        )
        .optional()?
        .map(StoredProposal::try_from)
        .transpose()
}

pub(crate) fn approve(
    grant: &Granted<'_, Approve>,
    company: &CompanySlug,
    id: &ProposalId,
) -> Result<Option<CorrectionProposal>, CliError> {
    grant.mutate_in_scope("sp_approve_correction", |tx, actor| {
        let Some(proposal) = find(tx, company, id)? else {
            return Ok(None);
        };
        let approver = Granted::<CorrectionApprove>::check(tx, actor)?;
        approver.mutate_in_scope("sp_apply_correction", |tx, actor| {
            match proposal.resolution {
                Some(Resolution::Approved { .. }) => return Ok(Some(proposal.review(company))),
                Some(_) => return Err(already_resolved(id)),
                None => {}
            }
            let scope = Company::bind_retained(tx.read(), company.clone())?;
            let applied = match &proposal.request.correction {
                Correction::Reverse(reversal) => {
                    let result = reverse_in_scope(
                        tx,
                        actor,
                        &reversal.params(company, proposal.request.requested_on),
                        ReversalSource::Standalone,
                    )?;
                    let applied = AppliedReversal::load(tx.read(), company, result)?;
                    Applied::Reversed {
                        transaction: reversal.id(),
                        reversal: applied.reversal_id(),
                        receipt: applied.receipt,
                    }
                }
                Correction::Recategorize(request) => {
                    Applied::Recategorized(Box::new(request.apply(tx, actor, &scope)?))
                }
                Correction::Undo { receipt } => {
                    let outcome = AgentReceiptUndo::load(&scope, *receipt)?.apply(
                        tx,
                        actor,
                        &scope,
                        proposal.request.requested_on,
                    )?;
                    Applied::Undone {
                        undo_of: *receipt,
                        undo_receipt: outcome.receipt(),
                    }
                }
            };
            resolve(
                tx,
                actor,
                company,
                proposal,
                Resolution::Approved { applied },
            )
            .map(Some)
        })
    })
}

pub(crate) fn reject(
    grant: &Granted<'_, ResolveProposal>,
    company: &CompanySlug,
    id: &ProposalId,
    reason: Option<&str>,
) -> Result<Option<CorrectionProposal>, CliError> {
    grant.mutate_in_scope("sp_reject_correction", |tx, actor| {
        let Some(proposal) = find(tx, company, id)? else {
            return Ok(None);
        };
        if proposal.resolution.is_some() {
            return Err(already_resolved(id));
        }
        let reason = reason.map(str::to_owned);
        if actor.agent_uri() == Some(proposal.proposer_uri.as_str()) {
            resolve(
                tx,
                actor,
                company,
                proposal,
                Resolution::Withdrawn { reason },
            )
            .map(Some)
        } else {
            Granted::<CorrectionApprove>::check(tx, actor)?.mutate_in_scope(
                "sp_reject_proposed_correction",
                |tx, actor| {
                    resolve(
                        tx,
                        actor,
                        company,
                        proposal,
                        Resolution::Rejected { reason },
                    )
                    .map(Some)
                },
            )
        }
    })
}

fn resolve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    mut proposal: StoredProposal,
    resolution: Resolution,
) -> Result<CorrectionProposal, CliError> {
    record(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::CorrectionProposal,
            entity_id: proposal.id.as_str(),
            action: AuditAction::CorrectionResolve,
            snapshots: (
                Some(&snapshot(&proposal.request)),
                Some(&snapshot(&resolution)),
            ),
        },
    )?;
    proposal.resolution = Some(resolution);
    Ok(proposal.review(company))
}

fn already_resolved(id: &ProposalId) -> CliError {
    CliError::Validation(format!("proposal '{id}' is already resolved"))
}
