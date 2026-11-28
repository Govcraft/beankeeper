//! Routine agent changes submitted in the signed bookkeeping mode.

use beankeeper::types::{AccountCode, CashClass};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::{Applied, Correction, PendingCorrection, reserve};
use crate::{
    db::{
        Actor, Db,
        audit::{
            cash_classification, counterparties, grant, payment_collection, projects,
            savepoint::AuditedTx,
        },
        companies::{Company, CompanySlug},
        counterparties::{ContactDetails, ContactPatch, CounterpartyId},
        ids::{EntryId, ReceiptId, TransactionId},
        projects::{ProjectBudgetInput, ProjectId, ProjectTerms, SplitInput},
    },
    error::CliError,
    identity::{Granted, VerifiedAgent, WriteMode},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Bookkeeping {
    Counterparty(Party),
    Project(Project),
    Classify(Classification),
    StripeCollection(StripeCollection),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Party {
    Create {
        details: ContactDetails,
    },
    Amend {
        id: CounterpartyId,
        patch: ContactPatch,
    },
    Archive {
        id: CounterpartyId,
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
    Archive {
        id: ProjectId,
    },
    Budget {
        budget: ProjectBudgetInput,
    },
    Split {
        entry: EntryId,
        date: NaiveDate,
        splits: Vec<SplitInput>,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Classification {
    pub(crate) code: AccountCode,
    pub(crate) class: CashClass,
}

impl Classification {
    pub(crate) fn submit(
        self,
        db: &Db,
        actor: &Actor,
        company: &CompanySlug,
    ) -> Result<Submission<cash_classification::ClassificationSubmission>, CliError> {
        submit::<grant::Bookkeeping, _>(
            db,
            actor,
            company,
            Bookkeeping::Classify(self.clone()),
            |tx, actor, scope| cash_classification::apply(tx, actor, scope, self.code, self.class),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum StripeCollection {
    ReportEvidence {
        evidence: payment_collection::reported::ReportedEvidence,
    },
    ReconcilePayout {
        manifest: payment_collection::payout::PayoutManifest,
    },
    ReconcileRefund {
        terms: payment_collection::refund::RefundSettlement,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Submission<T> {
    Applied(T),
    Pending(PendingCorrection),
}

fn submit<C: grant::sealed::GrantKind, T>(
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
    Granted::<C>::with_db(db, actor, |grant| {
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
        submit::<grant::Bookkeeping, _>(
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
            Self::Amend { id, patch } => counterparties::update(
                tx,
                actor,
                scope,
                counterparties::PartyEdit::Amend(id, patch),
            ),
            Self::Archive { id } => {
                counterparties::update(tx, actor, scope, counterparties::PartyEdit::Archive(id))
            }
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
        let change = Bookkeeping::Project(self.clone());
        let apply =
            |tx: &AuditedTx<'_>, actor: &Actor, scope: &Company<'_>| self.apply(tx, actor, scope);
        if matches!(&change, Bookkeeping::Project(Self::Budget { .. })) {
            submit::<grant::BookkeepingBudget, _>(db, actor, company, change, apply)
        } else {
            submit::<grant::Bookkeeping, _>(db, actor, company, change, apply)
        }
    }

    fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
    ) -> Result<projects::ProjectChange, CliError> {
        match self {
            Self::Create { terms } => projects::create(tx, actor, scope, terms),
            Self::Archive { id } => projects::archive(tx, actor, scope, &id),
            Self::Budget { budget } => projects::set_budget(tx, actor, scope, budget),
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
            Self::StripeCollection(change) => result(change.apply(tx, actor, scope)?, None),
            Self::Counterparty(change) => {
                use counterparties::PartyMutationResult::{Association, Record};
                let value = change.apply(tx, actor, scope)?;
                let (Association { receipt_id, .. } | Record { receipt_id, .. }) = &value;
                result(&value, *receipt_id)
            }
            Self::Classify(change) => {
                let value =
                    cash_classification::apply(tx, actor, scope, change.code, change.class)?;
                let receipt = Some(value.receipt_id);
                result(value, receipt)
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
