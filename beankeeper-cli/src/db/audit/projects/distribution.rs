//! Effective, exact project splits of source posting lines.

use beankeeper::types::{AccountType, DebitOrCredit};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};

use crate::{
    db::{
        audit::{Actor, AuditAction, AuditEntity, savepoint::AuditedTx},
        companies::Company,
        ids::{EntryId, TransactionId},
        periods,
        projects::{
            BillableState, EntryDistribution, ProfitEntry, ProjectError, ProjectId, ProjectSplit,
            ProjectSplitId, SplitInput, check_splits, decode, find_project, get_entry,
        },
    },
    error::CliError,
};

use super::{ProjectChange, encode, record};

pub(super) fn set_splits(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    entry_id: EntryId,
    day: NaiveDate,
    splits: Vec<SplitInput>,
) -> Result<ProjectChange, CliError> {
    let entry = get_entry(company, entry_id)?;
    if !matches!(entry.kind, AccountType::Expense | AccountType::Revenue) {
        return Err(ProjectError::WrongEntryType.into());
    }
    if day < entry.date {
        return Err(ProjectError::InvalidDate.into());
    }
    periods::ensure_open(company.read(), company.slug(), day)?;
    let before = latest(company, entry_id)?;
    if before
        .as_ref()
        .is_some_and(|previous| previous.effective_on >= day)
    {
        return Err(CliError::Validation(
            "a new split must have a later effective date than the current distribution".into(),
        ));
    }
    ensure_not_billed(company, entry_id, day)?;
    ensure_editable_posting(company, entry.transaction)?;
    for split in &splits {
        let target = find_project(company, &split.project)?;
        target.require_active()?;
        if target.terms.currency != entry.currency {
            return Err(CliError::Validation(
                "project and posting currency must match".into(),
            ));
        }
        if split.billable == BillableState::Billable
            && (entry.kind != AccountType::Expense || entry.direction != DebitOrCredit::Debit)
        {
            return Err(CliError::Validation(
                "only positive expense splits can be billed to a customer".into(),
            ));
        }
    }
    persist(tx, actor, company, &entry, day, splits)
}

fn latest(company: &Company<'_>, entry: EntryId) -> Result<Option<EntryDistribution>, CliError> {
    let previous: Option<String> = company.read().query_row(
        "SELECT data FROM project_distributions WHERE company_slug=?1 AND entry_id=?2 ORDER BY effective_on DESC LIMIT 1",
        params![company.slug(), entry], |row| row.get(0),
    ).optional()?;
    previous.as_deref().map(decode).transpose()
}

fn ensure_not_billed(
    company: &Company<'_>,
    entry: EntryId,
    day: NaiveDate,
) -> Result<(), CliError> {
    let claimed: bool = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM project_splits split
         JOIN project_invoice_sources claim ON claim.company_slug=split.company_slug AND claim.split_id=split.id
         WHERE split.company_slug=?1 AND split.entry_id=?2 AND claim.claimed_on IS NOT NULL AND (claim.released_on IS NULL OR claim.released_on>?3))",
        params![company.slug(),entry,day], |row| row.get(0),
    )?;
    if claimed {
        Err(ProjectError::SourceUnavailable.into())
    } else {
        Ok(())
    }
}

fn persist(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    entry: &ProfitEntry,
    day: NaiveDate,
    splits: Vec<SplitInput>,
) -> Result<ProjectChange, CliError> {
    let unallocated_minor = check_splits(entry.amount, &splits)?;
    let splits = splits
        .into_iter()
        .map(|assignment| ProjectSplit {
            id: ProjectSplitId::generate(),
            assignment,
        })
        .collect();
    let distribution = EntryDistribution {
        entry_id: entry.id,
        transaction_id: entry.transaction,
        effective_on: day,
        splits,
        unallocated_minor,
    };
    let key = format!("{}/{}", entry.id, day);
    let receipt_id = record(
        tx,
        actor,
        company,
        (
            AuditEntity::ProjectDistribution,
            AuditAction::ProjectAllocate,
        ),
        &key,
        (None, &distribution),
    )?;
    tx.connection().execute(
        "INSERT INTO project_distributions(company_slug,entry_id,effective_on,receipt,data) VALUES(?1,?2,?3,?4,?5)",
        params![company.slug(),entry.id,day,receipt_id,encode(&distribution)?],
    )?;
    for split in &distribution.splits {
        tx.connection().execute(
            "INSERT INTO project_splits(company_slug,id,entry_id,effective_on,project_id,amount_minor,billable)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![company.slug(),split.id,entry.id,day,split.assignment.project,split.assignment.amount_minor,
                split.assignment.billable.storage_flag()],
        )?;
    }
    Ok(ProjectChange::Split {
        distribution,
        receipt_id,
    })
}

pub(super) fn attribute_invoice(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    posting: TransactionId,
    project: &ProjectId,
) -> Result<(), CliError> {
    let mut statement = company.read().prepare(
        "SELECT e.id,t.id,t.date,a.type,e.direction,e.amount,t.currency
         FROM entries e JOIN posted_transactions t ON t.company_slug=e.company_slug AND t.id=e.transaction_id
         JOIN accounts a ON a.company_slug=e.company_slug AND a.code=e.account_code
         WHERE e.company_slug=?1 AND e.transaction_id=?2 AND a.type='revenue' ORDER BY e.id",
    )?;
    // Collect IDs before opening any writes on the same connection.
    let entries = statement
        .query_map(params![company.slug(), posting], |row| {
            row.get::<_, EntryId>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in entries {
        let entry = get_entry(company, id)?;
        persist(
            tx,
            actor,
            company,
            &entry,
            entry.date,
            vec![SplitInput {
                project: project.clone(),
                amount_minor: entry.amount,
                billable: BillableState::Internal,
            }],
        )?;
    }
    Ok(())
}

fn ensure_editable_posting(company: &Company<'_>, posting: TransactionId) -> Result<(), CliError> {
    let protected: bool = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM bookkeeping_audit
           WHERE company_slug=?1 AND action='reverse'
             AND (entity_id=CAST(?2 AS TEXT) OR related_posting=?2))
           OR EXISTS(SELECT 1 FROM project_invoice_drafts d JOIN book_documents doc
             ON doc.company_slug=d.company_slug AND doc.id=d.document_id
             WHERE doc.company_slug=?1 AND (doc.posting_id=?2 OR EXISTS(
               SELECT 1 FROM document_allocations credit WHERE credit.company_slug=doc.company_slug
                 AND credit.document_id=doc.id AND credit.kind='credit' AND credit.source_id=?2)))",
        params![company.slug(), posting],
        |row| row.get(0),
    )?;
    if protected {
        return Err(CliError::Validation(
            "use the project document or its original posting to correct this distribution".into(),
        ));
    }
    Ok(())
}

pub(in crate::db::audit) fn guard_project_posting(
    company: &Company<'_>,
    posting: TransactionId,
) -> Result<(), CliError> {
    if company.read().schema_version()? < crate::db::schema::PROJECT_SCHEMA_VERSION {
        return Ok(());
    }
    let attributed: bool = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM entries e JOIN project_distributions d
          ON d.company_slug=e.company_slug AND d.entry_id=e.id
          WHERE e.company_slug=?1 AND e.transaction_id=?2)",
        params![company.slug(), posting],
        |row| row.get(0),
    )?;
    if attributed {
        return Err(CliError::Validation(
            "reverse and repost the journal to recategorize a project allocation".into(),
        ));
    }
    Ok(())
}

pub(in crate::db::audit) fn mirror_reversal(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    original: TransactionId,
    reversal: TransactionId,
    date: NaiveDate,
) -> Result<(), CliError> {
    if company.read().schema_version()? < crate::db::schema::PROJECT_SCHEMA_VERSION {
        return Ok(());
    }
    let mut query = company.read().prepare(
        "WITH source AS (
            SELECT id,ROW_NUMBER() OVER (ORDER BY id) position FROM entries
              WHERE company_slug=?1 AND transaction_id=?2
         ), undo AS (
            SELECT id,ROW_NUMBER() OVER (ORDER BY id) position FROM entries
              WHERE company_slug=?1 AND transaction_id=?3
         )
         SELECT d.data,undo.id FROM source JOIN undo USING(position)
         JOIN project_distributions d ON d.company_slug=?1 AND d.entry_id=source.id
         WHERE d.effective_on=(SELECT MAX(current.effective_on) FROM project_distributions current
           WHERE current.company_slug=d.company_slug AND current.entry_id=d.entry_id)
         ORDER BY source.position",
    )?;
    let revisions = query
        .query_map(params![company.slug(), original, reversal], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, EntryId>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (raw, reverse_entry) in revisions {
        let source: EntryDistribution = decode(&raw)?;
        if date < source.effective_on {
            return Err(CliError::Validation(
                "reversal date predates the current project distribution".into(),
            ));
        }
        ensure_not_billed(company, source.entry_id, date)?;
        let entries = source
            .splits
            .into_iter()
            .map(|split| SplitInput {
                billable: BillableState::Internal,
                ..split.assignment
            })
            .collect();
        persist(
            tx,
            actor,
            company,
            &get_entry(company, reverse_entry)?,
            date,
            entries,
        )?;
    }
    Ok(())
}
