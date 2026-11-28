//! Audited project, capacity and journal-attribution mutations.

mod billing;
mod distribution;
mod verification;

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{
    db::{
        Db,
        companies::{Company, CompanySlug},
        counterparties::{CounterpartyRole, find_counterparty},
        ids::{EntryId, ReceiptId},
        projects::{
            EntryDistribution, Project, ProjectBudgetInput, ProjectError, ProjectId, ProjectTerms,
            SplitInput, TimeEntry, TimeEntryId, WorkInput, find_project, find_time,
        },
    },
    error::CliError,
};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links,
    grant::{Admin, Approve, Budget, Granted, Post, sealed::GrantKind},
    savepoint::AuditedTx,
};

pub(crate) use billing::{BillingChange, ProjectInvoiceInput, cancel_invoice, draft_invoice};
pub(super) use billing::{
    apply_invoice_credit, claim_invoice, protect_invoice_terms, validate_invoice,
};
pub(super) use distribution::{guard_project_posting, mirror_reversal};
pub(crate) use verification::verify;

pub(crate) enum ProjectMutation {
    Create(ProjectTerms),
    Archive(ProjectId),
    Record(WorkInput),
    Approve(TimeEntryId, NaiveDate),
    Split(EntryId, NaiveDate, Vec<SplitInput>),
    Budget(ProjectBudgetInput),
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ProjectChange {
    Project {
        project: Project,
        receipt_id: Option<ReceiptId>,
    },
    Work {
        work: TimeEntry,
        receipt_id: Option<ReceiptId>,
    },
    Split {
        distribution: EntryDistribution,
        receipt_id: ReceiptId,
    },
    Budget {
        budget: ProjectBudgetInput,
        receipt_id: ReceiptId,
    },
}

pub(crate) fn mutate(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    mutation: ProjectMutation,
) -> Result<ProjectChange, CliError> {
    match mutation {
        ProjectMutation::Create(terms) => {
            with_scope::<Admin>(db, actor, slug, |tx, actor, company| {
                save_project(tx, actor, company, ProjectEdit::New(terms))
            })
        }
        ProjectMutation::Archive(id) => {
            with_scope::<Admin>(db, actor, slug, |tx, actor, company| {
                save_project(tx, actor, company, ProjectEdit::Archive(&id))
            })
        }
        ProjectMutation::Record(work) => {
            with_scope::<Post>(db, actor, slug, |tx, actor, company| {
                save_work(tx, actor, company, WorkEdit::New(work))
            })
        }
        ProjectMutation::Approve(id, date) => {
            with_scope::<Approve>(db, actor, slug, |tx, actor, company| {
                save_work(tx, actor, company, WorkEdit::Approve(&id, date))
            })
        }
        ProjectMutation::Split(id, date, splits) => {
            with_scope::<Admin>(db, actor, slug, |tx, actor, company| {
                distribution::set_splits(tx, actor, company, id, date, splits)
            })
        }
        ProjectMutation::Budget(input) => {
            with_scope::<Budget>(db, actor, slug, |tx, actor, company| {
                set_budget(tx, actor, company, input)
            })
        }
    }
}

fn with_scope<C: GrantKind>(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    apply: impl FnOnce(&AuditedTx<'_>, &Actor, &Company<'_>) -> Result<ProjectChange, CliError>,
) -> Result<ProjectChange, CliError> {
    Granted::<C>::with_db(db, actor, |grant| {
        grant.mutate("sp_project", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            apply(tx, actor, &company)
        })
    })
}

enum ProjectEdit<'a> {
    New(ProjectTerms),
    Archive(&'a ProjectId),
}

fn save_project(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    edit: ProjectEdit<'_>,
) -> Result<ProjectChange, CliError> {
    let (before, after, action) = match edit {
        ProjectEdit::New(terms) => {
            find_counterparty(company, &terms.customer)?
                .require_active_role(CounterpartyRole::Customer)?;
            let reused: bool = company.read().query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE company_slug=?1 AND code=?2)",
                params![company.slug(), terms.code.as_str()],
                |row| row.get(0),
            )?;
            if reused {
                return Err(CliError::Validation(format!(
                    "project code '{}' already exists",
                    terms.code
                )));
            }
            (
                None,
                Project {
                    id: ProjectId::generate(),
                    terms,
                    archived: false,
                },
                AuditAction::ProjectCreate,
            )
        }
        ProjectEdit::Archive(id) => {
            let before = find_project(company, id)?;
            if before.archived {
                return Ok(ProjectChange::Project {
                    project: before,
                    receipt_id: None,
                });
            }
            let mut after = before.clone();
            after.archived = true;
            (Some(before), after, AuditAction::ProjectArchive)
        }
    };
    let receipt = record(
        tx,
        actor,
        company,
        (AuditEntity::Project, action),
        &after.id,
        (before.as_ref(), &after),
    )?;
    tx.connection().execute(
        "INSERT INTO projects(company_slug,id,code,customer_id,data,archived,receipt)
         VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(company_slug,id) DO UPDATE SET
         data=excluded.data,archived=excluded.archived,receipt=excluded.receipt",
        params![
            company.slug(),
            after.id,
            after.terms.code.as_str(),
            after.terms.customer,
            encode(&after)?,
            after.archived,
            receipt
        ],
    )?;
    Ok(ProjectChange::Project {
        project: after,
        receipt_id: Some(receipt),
    })
}

enum WorkEdit<'a> {
    New(WorkInput),
    Approve(&'a TimeEntryId, NaiveDate),
}

fn save_work(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    edit: WorkEdit<'_>,
) -> Result<ProjectChange, CliError> {
    let (before, after, action) = match edit {
        WorkEdit::New(work) => {
            find_project(company, &work.project)?.require_active()?;
            if let Some(rate) = work.rate_minor {
                rate.priced_minutes(work.minutes)?;
            }
            (
                None,
                TimeEntry {
                    id: TimeEntryId::generate(),
                    work,
                    approved_on: None,
                },
                AuditAction::ProjectWorkRecord,
            )
        }
        WorkEdit::Approve(id, date) => {
            let before = find_time(company, id)?;
            if date < before.work.date {
                return Err(ProjectError::InvalidDate.into());
            }
            if let Some(current) = before.approved_on {
                return if date == current {
                    Ok(ProjectChange::Work {
                        work: before,
                        receipt_id: None,
                    })
                } else {
                    Err(CliError::Validation("work is already approved".into()))
                };
            }
            let mut after = before.clone();
            after.approved_on = Some(date);
            (Some(before), after, AuditAction::ProjectWorkApprove)
        }
    };
    let receipt = record(
        tx,
        actor,
        company,
        (AuditEntity::ProjectWork, action),
        &after.id,
        (before.as_ref(), &after),
    )?;
    tx.connection().execute(
        "INSERT INTO project_time_entries(company_slug,id,project_id,worked_on,approved_on,data,receipt)
         VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(company_slug,id) DO UPDATE SET
         approved_on=excluded.approved_on,data=excluded.data,receipt=excluded.receipt",
        params![company.slug(),after.id,after.work.project,after.work.date,after.approved_on,encode(&after)?,receipt],
    )?;
    Ok(ProjectChange::Work {
        work: after,
        receipt_id: Some(receipt),
    })
}

fn set_budget(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    input: ProjectBudgetInput,
) -> Result<ProjectChange, CliError> {
    find_project(company, &input.project)?.require_active()?;
    let amount = i64::try_from(input.amount_minor).map_err(|_| ProjectError::AmountOutOfRange)?;
    let old: Option<i64> = company.read().query_row(
        "SELECT amount_minor FROM project_expense_budgets WHERE company_slug=?1 AND project_id=?2 AND month_start=?3",
        params![company.slug(),input.project,input.month.start()], |row| row.get(0),
    ).optional()?;
    let before = old
        .map(u64::try_from)
        .transpose()
        .map_err(|_| ProjectError::AmountOutOfRange)?
        .map(|amount_minor| ProjectBudgetInput {
            amount_minor,
            ..input.clone()
        });
    let key = format!("{}/{}", input.project, input.month);
    let receipt_id = record(
        tx,
        actor,
        company,
        (AuditEntity::ProjectBudget, AuditAction::ProjectBudgetSet),
        &key,
        (before.as_ref(), &input),
    )?;
    tx.connection().execute(
        "INSERT INTO project_expense_budgets(company_slug,project_id,month_start,amount_minor,receipt)
         VALUES(?1,?2,?3,?4,?5) ON CONFLICT(company_slug,project_id,month_start) DO UPDATE SET
         amount_minor=excluded.amount_minor,receipt=excluded.receipt",
        params![company.slug(),input.project,input.month.start(),amount,receipt_id],
    )?;
    Ok(ProjectChange::Budget {
        budget: input,
        receipt_id,
    })
}

fn record<T: Serialize>(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    event: (AuditEntity, AuditAction),
    entity_id: &impl std::fmt::Display,
    (before, after): (Option<&T>, &T),
) -> Result<ReceiptId, CliError> {
    let (entity, action) = event;
    let previous = before.map(encode).transpose()?;
    let current = encode(after)?;
    append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company.slug()),
            entity,
            action,
            entity_id: &entity_id.to_string(),
            snapshots: (previous.as_deref(), Some(&current)),
        },
        AuditLinks::None,
    )
}

fn encode(value: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(value)
        .map_err(|error| CliError::Database(format!("could not encode project snapshot: {error}")))
}
