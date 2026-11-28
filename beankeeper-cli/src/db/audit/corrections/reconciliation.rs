//! Exact clearing and monthly statement evidence for approval-mode agents.

use serde::{Deserialize, Serialize};

use super::{Applied, Correction, reserve};
use crate::{
    commands::txn_clear::{ClearanceEvidence, ClearanceRequest, ClearanceResponse},
    db::{
        Actor, Db,
        audit::{
            Reconciliation,
            corrections::bookkeeping::Submission,
            grant::{self, Granted},
            savepoint::AuditedTx,
            statement::{self, StatementInput, StatementReceipt, StatementReview},
        },
        companies::{Company, CompanySlug},
        statement::StatementClose,
    },
    error::CliError,
    identity::{VerifiedAgent, WriteMode},
};

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum ReconciliationChange {
    ClearEntries {
        request: ClearanceRequest,
        entries: Vec<ClearanceEvidence>,
    },
    CompleteStatement {
        request: StatementInput,
        statement: StatementReview,
    },
}

impl ReconciliationChange {
    pub(super) const fn title(&self) -> &'static str {
        match self {
            Self::ClearEntries { .. } => "Change statement clearings",
            Self::CompleteStatement { .. } => "Complete a monthly statement",
        }
    }

    pub(super) fn apply(
        &self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        company: &CompanySlug,
    ) -> Result<Applied, CliError> {
        let (receipt, result) = match self {
            Self::ClearEntries { request, entries } => {
                let (response, current) =
                    request.run_with_evidence(&Reconciliation::new(tx, actor), company)?;
                if &current != entries {
                    return Err(stale());
                }
                (None, serde_json::to_value(response))
            }
            Self::CompleteStatement { request, statement } => {
                let response =
                    statement::reconcile(tx, actor, company, request.clone(), Some(statement))?;
                (Some(response.receipt()), serde_json::to_value(response))
            }
        };
        Ok(Applied::Reconciliation {
            receipt_id: receipt,
            result: result.map_err(|error| {
                CliError::General(format!("cannot serialize reconciliation result: {error}"))
            })?,
        })
    }
}

pub(in crate::db::audit) fn stale() -> CliError {
    CliError::Validation(
        "the statement evidence changed after the proposal was submitted. Review the new evidence and submit a new proposal".into(),
    )
}

fn requires_approval(actor: &Actor) -> bool {
    actor.identity().map(VerifiedAgent::write_mode) == Some(WriteMode::ProposeOnly)
}

enum Preparation<T> {
    Request(Box<ReconciliationChange>),
    Unchanged(T),
}

fn submit<T>(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    prepare: impl FnOnce(&AuditedTx<'_>, &Actor) -> Result<Preparation<T>, CliError>,
) -> Result<Submission<T>, CliError> {
    Granted::<grant::CorrectionPropose>::with_db(db, actor, |grant| {
        grant.mutate("sp_propose_reconciliation", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            let change = match prepare(tx, actor)? {
                Preparation::Unchanged(response) => return Ok(Submission::Applied(response)),
                Preparation::Request(change) => change,
            };
            reserve(
                tx,
                actor,
                &scope,
                &Correction::Reconciliation { change }.into(),
            )
            .map(Submission::Pending)
        })
    })
}

pub(crate) fn clear(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    request: ClearanceRequest,
) -> Result<Submission<ClearanceResponse>, CliError> {
    if requires_approval(actor) {
        return submit(db, actor, company, |tx, actor| {
            let (response, entries) = tx.preview_nested("sp_preview_clearance", |tx| {
                request.run_with_evidence(&Reconciliation::new(tx, actor), company)
            })?;
            if entries.iter().all(ClearanceEvidence::unchanged) {
                return Ok(Preparation::Unchanged(response));
            }
            Ok(Preparation::Request(Box::new(
                ReconciliationChange::ClearEntries { request, entries },
            )))
        });
    }
    Granted::<grant::StatementReconcile>::with_db(db, actor, |grant| {
        grant.mutate("sp_reconcile_entries", |tx, actor| {
            request
                .run(&Reconciliation::new(tx, actor), company)
                .map(Submission::Applied)
        })
    })
}

pub(crate) fn complete(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    request: StatementInput,
) -> Result<Submission<StatementReceipt>, CliError> {
    if request.close == StatementClose::ClosePeriod {
        if let Some(identity) = actor.identity()
            && !grant::permits::<grant::StatementClose>(identity)?
        {
            return Err(CliError::Forbidden(
                "closing a company period requires direct ledger/reconcile and ledger/admin access"
                    .into(),
            ));
        }
        return Granted::<grant::StatementClose>::with_db(db, actor, |grant| {
            statement::close(grant, company, request).map(Submission::Applied)
        });
    }
    if requires_approval(actor) {
        return submit(db, actor, company, |tx, _| {
            let statement = statement::preview(tx, company, request.clone())?;
            Ok(Preparation::Request(Box::new(
                ReconciliationChange::CompleteStatement { request, statement },
            )))
        });
    }
    Granted::<grant::StatementReconcile>::with_db(db, actor, |grant| {
        grant.mutate("sp_statement_reconcile", |tx, actor| {
            statement::reconcile(tx, actor, company, request, None).map(Submission::Applied)
        })
    })
}
