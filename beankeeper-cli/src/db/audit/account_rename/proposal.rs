//! Chart proposals share the posting proposal review commands and permissions.

use beankeeper::types::{AccountCode, AccountName};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, Company, CompanySlug, CurrentAccount,
    LabelChange, ProposalId, ReadConn, ReceiptId, RenameSubmission, decode, desktop_receipt_path,
    ensure_available, snapshot, store_label,
};
use crate::db::audit::{record, savepoint::AuditedTx};
use crate::db::proposals;
use crate::error::CliError;
use crate::identity::{Approve, Granted, ResolveProposal};

#[derive(Serialize, Deserialize)]
struct RequestedRename {
    code: AccountCode,
    name: AccountName,
    account_revision: Option<ReceiptId>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Resolution {
    Approved { receipt_id: ReceiptId },
    Rejected { reason: Option<String> },
    Withdrawn { reason: Option<String> },
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReviewState {
    Pending {
        status: &'static str,
    },
    Resolved {
        #[serde(flatten)]
        resolution: Resolution,
        desktop_path: Option<String>,
    },
}

#[derive(Serialize)]
pub(crate) struct RenameProposal {
    kind: &'static str,
    id: ProposalId,
    company_slug: CompanySlug,
    proposer_uri: String,
    created_at: String,
    payload: RequestedRename,
    #[serde(flatten)]
    state: ReviewState,
}

struct StoredProposal {
    id: ProposalId,
    proposer_uri: String,
    created_at: String,
    request: RequestedRename,
    resolution: Option<Resolution>,
}

type ProposalRow = (ProposalId, String, String, String, Option<String>);

impl TryFrom<ProposalRow> for StoredProposal {
    type Error = CliError;

    fn try_from(
        (id, proposer_uri, created_at, requested, resolved): ProposalRow,
    ) -> Result<Self, CliError> {
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
    fn review(self, company: &CompanySlug) -> RenameProposal {
        let state = if let Some(resolution) = self.resolution {
            let desktop_path = match resolution {
                Resolution::Approved { receipt_id } => {
                    Some(desktop_receipt_path(company, receipt_id))
                }
                _ => None,
            };
            ReviewState::Resolved {
                resolution,
                desktop_path,
            }
        } else {
            ReviewState::Pending { status: "pending" }
        };
        RenameProposal {
            kind: "account_rename",
            id: self.id,
            company_slug: company.clone(),
            proposer_uri: self.proposer_uri,
            created_at: self.created_at,
            payload: self.request,
            state,
        }
    }
}

pub(super) fn reserve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    code: AccountCode,
    name: AccountName,
) -> Result<RenameSubmission, CliError> {
    let current = CurrentAccount::load(scope, &code)?;
    if current.label.as_str() == name.as_ref() {
        return Ok(RenameSubmission::Unchanged { code, name });
    }
    ensure_available(scope, &code, name.as_ref())?;
    let request = RequestedRename {
        code,
        name,
        account_revision: current.revision(),
    };
    let raw = snapshot(&request);
    let existing = tx
        .read()
        .query_row(
            &format!(
                "SELECT proposed.entity_id FROM {audit} proposed
         WHERE proposed.company_slug = ?1 AND proposed.action = 'account_rename_propose'
           AND proposed.agent_uri = ?2 AND proposed.after = ?3
           AND NOT EXISTS (
               SELECT 1 FROM {audit} resolved WHERE resolved.company_slug = proposed.company_slug
               AND resolved.entity = proposed.entity AND resolved.entity_id = proposed.entity_id
               AND resolved.action = 'account_rename_resolve'
           )
         ORDER BY proposed.id DESC LIMIT 1",
                audit = tx.read().bookkeeping_audit()
            ),
            params![scope.slug(), actor.agent_uri(), raw],
            |row| row.get(0),
        )
        .optional()?;
    let proposal_id = if let Some(id) = existing {
        id
    } else {
        let id = ProposalId::new();
        record(
            tx.connection(),
            actor,
            AuditChange {
                company_slug: Some(scope.slug()),
                entity: AuditEntity::AccountRenameProposal,
                entity_id: id.as_str(),
                action: AuditAction::AccountRenamePropose,
                snapshots: (None, Some(&raw)),
            },
        )?;
        id
    };
    Ok(RenameSubmission::Proposed {
        code: request.code,
        name: request.name,
        proposal_id,
    })
}

fn select_proposals(reader: ReadConn<'_>, filter: &str) -> String {
    let audit = reader.bookkeeping_audit();
    format!(
        "SELECT proposed.entity_id, proposed.agent_uri, proposed.changed_at,
       proposed.after, resolved.after
     FROM {audit} proposed
     LEFT JOIN {audit} resolved ON resolved.company_slug = proposed.company_slug
       AND resolved.entity = proposed.entity AND resolved.entity_id = proposed.entity_id
       AND resolved.action = 'account_rename_resolve'
     WHERE proposed.company_slug = ?1 AND proposed.action = 'account_rename_propose'
       {filter}"
    )
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProposalRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

pub(crate) fn list(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    actor: &Actor,
    include_resolved: bool,
) -> Result<Vec<RenameProposal>, CliError> {
    let all = proposals::can_review_all(reader, actor)?;
    let mut stmt = reader.prepare(&select_proposals(
        reader,
        "AND (?2 OR resolved.id IS NULL) AND (?3 OR proposed.agent_uri = ?4)
         ORDER BY proposed.id",
    ))?;
    stmt.query_map(
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
    let reader = tx.read();
    reader
        .query_row(
            &select_proposals(reader, "AND proposed.entity_id = ?2"),
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
) -> Result<Option<RenameProposal>, CliError> {
    grant.mutate_in_scope("sp_approve_account_rename", |tx, actor| {
        let Some(proposal) = find(tx, company, id)? else {
            return Ok(None);
        };
        match proposal.resolution {
            Some(Resolution::Approved { .. }) => return Ok(Some(proposal.review(company))),
            Some(_) => return Err(already_resolved(id)),
            None => {}
        }
        let scope = Company::bind(tx.read(), company.clone())?;
        let request = &proposal.request;
        let current = CurrentAccount::load(&scope, &request.code)?;
        if current.revision() != request.account_revision
            || current.label.as_str() == request.name.as_ref()
        {
            return Err(CliError::Validation(format!(
                "account '{}' changed after proposal '{id}'; request a new rename",
                request.code,
            )));
        }
        let receipt_id = store_label(
            tx,
            actor,
            &scope,
            &request.code,
            &current.label,
            LabelChange::Rename(&request.name),
        )?;
        resolve(
            tx,
            actor,
            company,
            proposal,
            Resolution::Approved { receipt_id },
        )
        .map(Some)
    })
}

pub(crate) fn reject(
    grant: &Granted<'_, ResolveProposal>,
    company: &CompanySlug,
    id: &ProposalId,
    reason: Option<&str>,
) -> Result<Option<RenameProposal>, CliError> {
    grant.mutate_in_scope("sp_reject_account_rename", |tx, actor| {
        let Some(proposal) = find(tx, company, id)? else {
            return Ok(None);
        };
        if proposal.resolution.is_some() {
            return Err(already_resolved(id));
        }
        let reason = reason.map(str::to_owned);
        let resolution = if actor.agent_uri() == Some(proposal.proposer_uri.as_str()) {
            Resolution::Withdrawn { reason }
        } else {
            Granted::<Approve>::check(tx, actor)?;
            Resolution::Rejected { reason }
        };
        resolve(tx, actor, company, proposal, resolution).map(Some)
    })
}

fn resolve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    mut proposal: StoredProposal,
    resolution: Resolution,
) -> Result<RenameProposal, CliError> {
    record(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::AccountRenameProposal,
            entity_id: proposal.id.as_str(),
            action: AuditAction::AccountRenameResolve,
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
