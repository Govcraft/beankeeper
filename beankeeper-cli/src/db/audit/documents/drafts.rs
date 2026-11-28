//! Draft revisions, issuance and reproducible contact snapshots.

use std::collections::BTreeMap;

use beankeeper::types::{AccountType, Currency};
use chrono::NaiveDate;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{DocumentOutcome, persist_event};
use crate::{
    db::{
        ListAccountParams, accounts,
        audit::{
            Actor, AuditAction, AuditEntity, counterparties, post_in_audited_scope,
            savepoint::AuditedTx,
        },
        companies::Company,
        counterparties::find_counterparty,
        documents::{
            DocumentDraft, DocumentError, DocumentId, DocumentRecord, DocumentRevision, DueDate,
            IssuedCounterparty, IssuedDocument, JournalPlan, find_document, plan_issue,
        },
        fx, periods,
        tax_codes::{self, TaxError, TaxedLine, VatEvidence, VatOrigin},
        transactions::{ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams},
    },
    error::CliError,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct IssueIntent {
    pub id: DocumentId,
    pub revision: DocumentRevision,
    pub date: NaiveDate,
    pub due_date: DueDate,
    pub currency: Currency,
    pub counterparty: IssuedCounterparty,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vat: Vec<TaxedLine>,
}

fn validate_accounts(
    company: &Company<'_>,
    draft: &DocumentDraft,
) -> Result<JournalPlan, CliError> {
    let accounts = accounts::list_accounts(
        company,
        &ListAccountParams {
            type_filter: None,
            name_filter: None,
        },
    )?
    .into_iter()
    .map(|row| (row.code, row.account_type))
    .collect::<BTreeMap<String, AccountType>>();
    plan_issue(draft, |code| accounts.get(code.as_str()).copied()).map_err(Into::into)
}

fn terms(company: &Company<'_>, draft: &DocumentDraft) -> Result<IssuedCounterparty, CliError> {
    let party = find_counterparty(company, &draft.counterparty_id)?;
    party.require_active_role(draft.kind.party_role())?;
    Ok((&party).into())
}

fn functional_currency(company: &Company<'_>) -> Result<Currency, CliError> {
    let reader = company.read();
    if let Some(currency) = fx::functional_currency(reader, company.slug())? {
        return Ok(currency);
    }
    let mut statement = reader.prepare_cached(
        "SELECT DISTINCT currency FROM posted_transactions WHERE company_slug = ?1 ORDER BY currency LIMIT 2",
    )?;
    let currencies = statement
        .query_map([company.slug()], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<Currency>>>()?;
    match currencies.as_slice() {
        [] => Ok(Currency::default()),
        [currency] => Ok(*currency),
        _ => Err(CliError::Validation(
            "configure the book's functional currency before issuing documents in a multi-currency ledger".into(),
        )),
    }
}

pub(in crate::db::audit) fn save_draft(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    edit: Option<(&DocumentId, DocumentRevision)>,
    draft: DocumentDraft,
) -> Result<DocumentOutcome, CliError> {
    terms(company, &draft)?;
    validate_accounts(company, &draft)?;
    let previous = edit
        .map(|(id, revision)| -> Result<_, CliError> {
            crate::db::audit::projects::protect_invoice_terms(company, id)?;
            let record = find_document(company, id)?;
            if record.issued.is_some() {
                return Err(DocumentError::AlreadyIssued.into());
            }
            if record.revision != revision {
                return Err(DocumentError::StaleDraft.into());
            }
            Ok(record)
        })
        .transpose()?;
    let (id, revision, action) = match &previous {
        Some(record) => (
            record.id.clone(),
            record.revision.next()?,
            AuditAction::DocumentAmend,
        ),
        None => (
            DocumentId::generate(),
            DocumentRevision::default(),
            AuditAction::DocumentDraft,
        ),
    };
    let reused: bool = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM book_documents WHERE company_slug = ?1 AND number = ?2 AND id <> ?3)",
        params![company.slug(), draft.number.as_str(), id], |row| row.get(0),
    )?;
    if reused {
        return Err(CliError::Validation(format!(
            "document number '{}' is already in use",
            draft.number
        )));
    }
    let record = DocumentRecord {
        id,
        revision,
        draft,
        issued: None,
    };
    let receipt = persist_event(
        tx,
        actor,
        company.slug(),
        (AuditEntity::Document, action),
        &record.id,
        (previous.as_ref(), &record),
    )?;
    tx.connection().execute(
        "INSERT INTO book_documents(company_slug,id,number,counterparty_id,revision,draft,receipt)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(company_slug,id) DO UPDATE SET number = excluded.number,
             counterparty_id = excluded.counterparty_id, revision = excluded.revision,
             draft = excluded.draft, receipt = excluded.receipt",
        params![
            company.slug(),
            record.id,
            record.draft.number.as_str(),
            record.draft.counterparty_id,
            u32::from(record.revision),
            super::encode(&record.draft)?,
            receipt
        ],
    )?;
    Ok(DocumentOutcome::Document {
        record: Box::new(record),
        receipt_id: Some(receipt),
    })
}

pub(super) fn prepare_issue(
    company: &Company<'_>,
    record: &DocumentRecord,
    issue_date: NaiveDate,
) -> Result<IssueIntent, CliError> {
    periods::ensure_open(company.read(), company.slug(), issue_date)?;
    let plan = validate_accounts(company, &record.draft)?;
    let counterparty = terms(company, &record.draft)?;
    let due_date = DueDate::resolve(
        issue_date,
        record.draft.due_date,
        counterparty.payment_terms,
    )?;
    let currency = functional_currency(company)?;
    let vat = tax_codes::prepare_tax(company, &record.draft, &plan, currency, issue_date)?;
    Ok(IssueIntent {
        id: record.id.clone(),
        revision: record.revision,
        date: issue_date,
        due_date,
        currency,
        counterparty,
        vat,
    })
}

pub(super) fn apply_issue(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    intent: IssueIntent,
) -> Result<DocumentOutcome, CliError> {
    let before = find_document(company, &intent.id)?;
    if let Some(issued) = &before.issued {
        if before.revision != intent.revision
            || issued.counterparty != intent.counterparty
            || issued.due_date != intent.due_date
            || issued.currency != intent.currency
        {
            return Err(DocumentError::StaleDraft.into());
        }
        if issued.vat != intent.vat {
            return Err(TaxError::StaleReview.into());
        }
        return repeat_issue(&before, issued, intent.date);
    }
    let current = prepare_issue(company, &before, intent.date)?;
    if current.revision != intent.revision
        || current.counterparty != intent.counterparty
        || current.due_date != intent.due_date
        || current.currency != intent.currency
    {
        return Err(DocumentError::StaleDraft.into());
    }
    let manifest = crate::db::audit::projects::validate_invoice(company, &before, intent.date)?;
    if current.vat != intent.vat {
        return Err(TaxError::StaleReview.into());
    }
    let plan = tax_codes::with_issue_tax(
        validate_accounts(company, &before.draft)?,
        &before.draft,
        &intent.vat,
    )?;
    let entries: Vec<_> = plan
        .legs
        .into_iter()
        .map(|leg| PostEntryParams {
            account_code: leg.account.as_str().to_owned(),
            direction: leg.direction,
            amount: leg.amount,
            memo: None,
            tax_category: None,
        })
        .collect();
    let posting_id = match post_in_audited_scope(
        tx,
        &PostTransactionParams {
            company_slug: company.slug(),
            description: &format!("{} {}", before.draft.kind, before.draft.number),
            metadata: None,
            correlate: None,
            reference: Some(&format!("doc:issue:{}", before.id)),
            currency: intent.currency,
            date: intent.date,
            entries: &entries,
            on_conflict: ConflictStrategy::Error,
            actor,
        },
    )? {
        PostResult::Created(id) => id,
        PostResult::Skipped { .. } => {
            return Err(CliError::Database(
                "document issue posting was not created".into(),
            ));
        }
    };
    counterparties::associate(
        tx.connection(),
        actor,
        company,
        intent.counterparty.id.clone(),
        posting_id,
    )?;
    let issued = IssuedDocument {
        id: before.id.clone(),
        kind: before.draft.kind,
        number: before.draft.number.clone(),
        counterparty: intent.counterparty,
        issue_date: intent.date,
        due_date: intent.due_date,
        currency: intent.currency,
        control_account: before.draft.control_account.clone(),
        posting_id,
        lines: plan.lines,
        vat: intent.vat,
        total_minor: plan.total,
    };
    if let Some(manifest) = manifest {
        crate::db::audit::projects::claim_invoice(tx, actor, company, &manifest, &issued)?;
    }
    let mut record = before.clone();
    persist_issue(tx, actor, company, &before, &mut record, issued)
}

fn persist_issue(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    before: &DocumentRecord,
    record: &mut DocumentRecord,
    issued: IssuedDocument,
) -> Result<DocumentOutcome, CliError> {
    let issued_json = super::encode(&issued)?;
    let due = NaiveDate::from(issued.due_date);
    let control = issued.control_account.clone();
    let currency = issued.currency;
    let posting = issued.posting_id;
    let date = issued.issue_date;
    let vat = VatEvidence {
        origin: VatOrigin::Issue,
        effective_on: date,
        document_id: issued.id.clone(),
        posting_id: posting,
        kind: issued.kind,
        lines: issued.vat.clone(),
    };
    record.issued = Some(issued);
    let receipt = persist_event(
        tx,
        actor,
        company.slug(),
        (AuditEntity::Document, AuditAction::DocumentIssue),
        &record.id,
        (Some(before), record),
    )?;
    tx.connection().execute(
        "UPDATE book_documents SET issued=?1, issue_date=?2, due_date=?3,
            currency=?4, control_account=?5, posting_id=?6, receipt=?7
         WHERE company_slug=?8 AND id=?9 AND issued IS NULL",
        params![
            issued_json,
            date,
            due,
            currency.code(),
            control.as_str(),
            posting,
            receipt,
            company.slug(),
            record.id
        ],
    )?;
    crate::db::audit::tax_codes::persist_evidence(tx, actor, company, &vat)?;
    Ok(DocumentOutcome::Document {
        record: Box::new(record.clone()),
        receipt_id: Some(receipt),
    })
}

pub(super) fn repeat_issue(
    record: &DocumentRecord,
    document: &IssuedDocument,
    date: NaiveDate,
) -> Result<DocumentOutcome, CliError> {
    if document.issue_date == date {
        Ok(DocumentOutcome::Document {
            record: Box::new(record.clone()),
            receipt_id: None,
        })
    } else {
        Err(DocumentError::AlreadyIssued.into())
    }
}
