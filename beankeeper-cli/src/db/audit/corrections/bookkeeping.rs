//! Routine agent changes submitted in the signed bookkeeping mode.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::{Applied, Correction, PendingCorrection, reserve};
use crate::{
    db::{
        Actor, Db,
        audit::{counterparties, grant, projects, savepoint::AuditedTx},
        companies::{Company, CompanySlug},
        counterparties::{ContactDetails, CounterpartyId},
        ids::{EntryId, ReceiptId, TransactionId},
        projects::{ProjectTerms, SplitInput},
    },
    error::CliError,
    identity::{Granted, VerifiedAgent, WriteMode},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Bookkeeping {
    Counterparty(Party),
    Project(Project),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Party {
    Create {
        details: ContactDetails,
    },
    Associate {
        id: CounterpartyId,
        transaction: TransactionId,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Project {
    Create {
        terms: ProjectTerms,
    },
    Split {
        entry: EntryId,
        date: NaiveDate,
        splits: Vec<SplitInput>,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Submission<T> {
    Applied(T),
    Pending(PendingCorrection),
}

fn submit<T>(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    change: Bookkeeping,
    apply: impl FnOnce(&AuditedTx<'_>, &Actor, &Company<'_>) -> Result<T, CliError>,
) -> Result<Submission<T>, CliError> {
    if actor.identity().map(VerifiedAgent::write_mode) == Some(WriteMode::ProposeOnly) {
        return Granted::<grant::CorrectionPropose>::with_db(db, actor, |grant| {
            grant.mutate("sp_bookkeeping_propose", |tx, actor| {
                let scope = Company::bind(tx.read(), company.clone())?;
                // Check the same business rules as an immediate write. The
                // actual change and its receipt are applied only on approval.
                tx.preview_nested("sp_bookkeeping_preview", |tx| apply(tx, actor, &scope))?;
                reserve(
                    tx,
                    actor,
                    &scope,
                    &Correction::Bookkeeping { change }.into(),
                )
                .map(Submission::Pending)
            })
        });
    }
    Granted::<grant::Bookkeeping>::with_db(db, actor, |grant| {
        grant.mutate("sp_bookkeeping_apply", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            apply(tx, actor, &scope).map(Submission::Applied)
        })
    })
}

impl Party {
    pub(crate) fn submit(
        self,
        db: &Db,
        actor: &Actor,
        company: &CompanySlug,
    ) -> Result<Submission<counterparties::PartyMutationResult>, CliError> {
        submit(
            db,
            actor,
            company,
            Bookkeeping::Counterparty(self.clone()),
            |tx, actor, scope| self.apply(tx, actor, scope),
        )
    }

    fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
    ) -> Result<counterparties::PartyMutationResult, CliError> {
        match self {
            Self::Create { details } => counterparties::create(tx, actor, scope, details),
            Self::Associate { id, transaction } => {
                counterparties::associate(tx.connection(), actor, scope, id, transaction)
            }
        }
    }
}

impl Project {
    pub(crate) fn submit(
        self,
        db: &Db,
        actor: &Actor,
        company: &CompanySlug,
    ) -> Result<Submission<projects::ProjectChange>, CliError> {
        submit(
            db,
            actor,
            company,
            Bookkeeping::Project(self.clone()),
            |tx, actor, scope| self.apply(tx, actor, scope),
        )
    }

    fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
    ) -> Result<projects::ProjectChange, CliError> {
        match self {
            Self::Create { terms } => projects::create(tx, actor, scope, terms),
            Self::Split {
                entry,
                date,
                splits,
            } => projects::set_splits(tx, actor, scope, entry, date, splits),
        }
    }
}

impl Bookkeeping {
    pub(super) fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
    ) -> Result<Applied, CliError> {
        fn result(
            value: impl Serialize,
            receipt_id: Option<ReceiptId>,
        ) -> Result<Applied, CliError> {
            Ok(Applied::Bookkeeping {
                receipt_id,
                result: serde_json::to_value(value)
                    .map_err(|err| CliError::General(format!("bookkeeping result: {err}")))?,
            })
        }
        match self {
            Self::Counterparty(change) => {
                use counterparties::PartyMutationResult::{Association, Record};
                let value = change.apply(tx, actor, scope)?;
                let (Association { receipt_id, .. } | Record { receipt_id, .. }) = &value;
                result(&value, *receipt_id)
            }
            Self::Project(change) => {
                use projects::ProjectChange::{Budget, Project, Split, Work};
                let value = change.apply(tx, actor, scope)?;
                let receipt = match &value {
                    Project { receipt_id, .. } | Work { receipt_id, .. } => *receipt_id,
                    Split { receipt_id, .. } | Budget { receipt_id, .. } => Some(*receipt_id),
                };
                result(value, receipt)
            }
        }
    }
}
