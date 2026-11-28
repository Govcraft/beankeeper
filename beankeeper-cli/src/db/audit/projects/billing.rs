//! Freeze source facts in draft invoices and claim them inside the posting savepoint.

use std::collections::{BTreeMap, BTreeSet};

use beankeeper::types::AccountCode;
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        Db,
        amounts::PositiveMinorUnits,
        audit::{
            Actor, AuditAction, AuditEntity,
            documents::{DocumentOutcome, drafts::save_draft},
            grant::{Granted, Post, PostingGrant},
            savepoint::AuditedTx,
        },
        companies::{Company, CompanySlug},
        documents::{
            DocumentDraft, DocumentId, DocumentKind, DocumentLine, DocumentNumber, DocumentRecord,
            DueDate, IssuedDocument, find_document,
        },
        ids::{ReceiptId, TransactionId},
        projects::{
            AvailableSource, BillingSource, ProjectError, ProjectId, decode, find_project, unbilled,
        },
        schema::PROJECT_SCHEMA_VERSION,
        tax_codes::LineTax,
    },
    error::CliError,
};

use super::{distribution::attribute_invoice, encode, record};

/// Selected approved obligations to include in a new invoice draft.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectInvoiceInput {
    /// Project issuing its customer's invoice.
    pub project: ProjectId,
    /// Unique invoice number.
    pub number: DocumentNumber,
    /// Receivable control account.
    #[schemars(with = "String")]
    pub receivable_account: AccountCode,
    /// Revenue account used for billed work and expenses.
    #[schemars(with = "String")]
    pub revenue_account: AccountCode,
    /// First day when work or costs can qualify.
    #[schemars(with = "String")]
    pub from: NaiveDate,
    /// Last day of the approved source window.
    #[schemars(with = "String")]
    pub through: NaiveDate,
    /// Explicit due date when overriding the customer's default terms.
    pub due_date: Option<DueDate>,
    /// Optional VAT code and price treatment shared by the invoice lines.
    pub tax: Option<LineTax>,
    /// Unique exact source IDs selected by the caller.
    pub sources: Vec<BillingSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BillingLine {
    source: BillingSource,
    amount_minor: PositiveMinorUnits,
    claimed_on: Option<NaiveDate>,
    released_on: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::db::audit) struct ProjectInvoice {
    document_id: DocumentId,
    project_id: ProjectId,
    from: NaiveDate,
    through: NaiveDate,
    terms: DocumentDraft,
    cancelled: bool,
    lines: Vec<BillingLine>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BillingChange {
    pub result: DocumentOutcome,
    pub sources: Vec<BillingSource>,
    pub receipt_id: ReceiptId,
}

pub(crate) fn draft_invoice(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    input: ProjectInvoiceInput,
) -> Result<BillingChange, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => grant.mutate("sp_project_draft", |tx, actor| {
            draft_in_scope(tx, actor, slug, input)
        }),
        PostingGrant::Proposal(grant) => grant.mutate("sp_project_draft", |tx, actor| {
            draft_in_scope(tx, actor, slug, input)
        }),
    })
}

fn draft_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    input: ProjectInvoiceInput,
) -> Result<BillingChange, CliError> {
    let company = Company::bind(tx.read(), slug.clone())?;
    let project = find_project(&company, &input.project)?;
    project.require_active()?;
    let sources = selected_sources(&company, &input)?;
    let terms = DocumentDraft {
        kind: DocumentKind::Invoice,
        number: input.number,
        counterparty_id: project.terms.customer,
        control_account: input.receivable_account,
        due_date: input.due_date,
        lines: sources
            .iter()
            .map(|source| {
                Ok(DocumentLine {
                    description: normalize_description(&source.description)
                        .parse::<crate::db::documents::LineDescription>()?,
                    quantity: "1".parse::<crate::db::documents::LineQuantity>()?,
                    unit_price_minor: source.amount_minor.into(),
                    account: input.revenue_account.clone(),
                    tax: input.tax.clone(),
                })
            })
            .collect::<Result<_, crate::db::documents::DocumentError>>()?,
    };
    let outcome = save_draft(tx, actor, &company, None, terms.clone())?;
    let DocumentOutcome::Document { record: draft, .. } = &outcome else {
        return Err(CliError::Database(
            "project invoice did not produce an editable draft".into(),
        ));
    };
    let manifest = ProjectInvoice {
        document_id: draft.id.clone(),
        project_id: project.id,
        from: input.from,
        through: input.through,
        terms,
        cancelled: false,
        lines: sources
            .into_iter()
            .map(|source| BillingLine {
                source: source.source,
                amount_minor: source.amount_minor,
                claimed_on: None,
                released_on: None,
            })
            .collect(),
    };
    let receipt = store_manifest(
        tx,
        actor,
        &company,
        None,
        &manifest,
        AuditAction::ProjectInvoiceDraft,
    )?;
    Ok(BillingChange {
        result: outcome,
        sources: manifest.lines.into_iter().map(|line| line.source).collect(),
        receipt_id: receipt,
    })
}

fn normalize_description(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    if clean.trim().is_empty() {
        "Billable project expense".into()
    } else {
        clean
    }
}

fn selected_sources(
    company: &Company<'_>,
    input: &ProjectInvoiceInput,
) -> Result<Vec<AvailableSource>, CliError> {
    if input.sources.is_empty() || input.sources.len() > 1000 {
        return Err(CliError::Validation(
            "provide 1 to 1000 billing sources".into(),
        ));
    }
    let mut available = unbilled(company, &input.project, input.from, input.through)?
        .into_iter()
        .map(|source| (source.source.clone(), source))
        .collect::<BTreeMap<_, _>>();
    input
        .sources
        .iter()
        .map(|source| {
            available
                .remove(source)
                .ok_or(ProjectError::SourceUnavailable.into())
        })
        .collect()
}

fn load_manifest(
    company: &Company<'_>,
    id: &DocumentId,
) -> Result<Option<ProjectInvoice>, CliError> {
    if company.read().schema_version()? < PROJECT_SCHEMA_VERSION {
        return Ok(None);
    }
    let raw: Option<String> = company
        .read()
        .query_row(
            "SELECT data FROM project_invoice_drafts WHERE company_slug=?1 AND document_id=?2",
            params![company.slug(), id],
            |row| row.get(0),
        )
        .optional()?;
    raw.as_deref().map(decode).transpose()
}

fn store_manifest(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    before: Option<&ProjectInvoice>,
    after: &ProjectInvoice,
    action: AuditAction,
) -> Result<ReceiptId, CliError> {
    let receipt = record(
        tx,
        actor,
        company,
        (AuditEntity::ProjectInvoice, action),
        &after.document_id,
        (before, after),
    )?;
    tx.connection().execute(
        "INSERT INTO project_invoice_drafts(company_slug,document_id,project_id,data,cancelled,receipt)
         VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(company_slug,document_id) DO UPDATE SET
         data=excluded.data,cancelled=excluded.cancelled,receipt=excluded.receipt",
        params![company.slug(),after.document_id,after.project_id,encode(after)?,after.cancelled,receipt],
    )?;
    for (number, line) in (1_i64..).zip(&after.lines) {
        let (time_id, split_id) = match &line.source {
            BillingSource::Time(id) => (Some(id), None),
            BillingSource::Expense(id) => (None, Some(id)),
        };
        tx.connection().execute(
            "INSERT INTO project_invoice_sources(company_slug,document_id,line_number,time_id,split_id,expected_minor,claimed_on,released_on)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(company_slug,document_id,line_number) DO UPDATE
             SET claimed_on=excluded.claimed_on,released_on=excluded.released_on",
            params![company.slug(),after.document_id,number,time_id,split_id,line.amount_minor,line.claimed_on,line.released_on],
        )?;
    }
    Ok(receipt)
}

pub(crate) fn cancel_invoice(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DocumentId,
) -> Result<ReceiptId, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_project_cancel",|tx,actor| {
        let company=Company::bind(tx.read(),slug.clone())?;
        let before=load_manifest(&company,id)?.ok_or_else(|| CliError::NotFound(format!("project invoice draft '{id}' not found")))?;
        if find_document(&company,id)?.issued.is_some() {
            return Err(CliError::Validation("credit an issued project invoice instead of cancelling its draft".into()));
        }
        if before.cancelled {
            return Ok(company.read().query_row(
                "SELECT receipt FROM project_invoice_drafts WHERE company_slug=?1 AND document_id=?2",
                params![slug,id],|row| row.get(0),
            )?);
        }
        let mut after=before.clone();
        after.cancelled=true;
        store_manifest(tx,actor,&company,Some(&before),&after,AuditAction::ProjectInvoiceCancel)
    })
    })
}

pub(in crate::db::audit) fn protect_invoice_terms(
    company: &Company<'_>,
    id: &DocumentId,
) -> Result<(), CliError> {
    if load_manifest(company, id)?.is_some() {
        return Err(CliError::Validation(
            "cancel the project draft and select its sources again to change its terms".into(),
        ));
    }
    Ok(())
}

pub(in crate::db::audit) fn validate_invoice(
    company: &Company<'_>,
    draft: &DocumentRecord,
    date: NaiveDate,
) -> Result<Option<ProjectInvoice>, CliError> {
    let Some(manifest) = load_manifest(company, &draft.id)? else {
        return Ok(None);
    };
    if manifest.cancelled
        || date < manifest.through
        || encode(&manifest.terms)? != encode(&draft.draft)?
    {
        return Err(ProjectError::SourceUnavailable.into());
    }
    find_project(company, &manifest.project_id)?.require_active()?;
    let sources = unbilled(company, &manifest.project_id, manifest.from, date)?
        .into_iter()
        .map(|source| (source.source, source.amount_minor))
        .collect::<BTreeMap<_, _>>();
    let mut used = BTreeSet::new();
    for line in &manifest.lines {
        if !used.insert(&line.source)
            || sources.get(&line.source) != Some(&line.amount_minor)
            || source_has_claim_after(company, &line.source, date)?
        {
            return Err(ProjectError::SourceUnavailable.into());
        }
    }
    Ok(Some(manifest))
}

fn source_has_claim_after(
    company: &Company<'_>,
    source: &BillingSource,
    date: NaiveDate,
) -> Result<bool, CliError> {
    let (filter, key): (&str, &dyn rusqlite::ToSql) = match source {
        BillingSource::Time(id) => ("time_id", id),
        BillingSource::Expense(id) => ("split_id", id),
    };
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM project_invoice_sources WHERE company_slug=?1 AND {filter}=?2
          AND claimed_on IS NOT NULL AND (released_on IS NULL OR released_on>?3))"
    );
    Ok(company
        .read()
        .query_row(&sql, params![company.slug(), key, date], |row| row.get(0))?)
}

pub(in crate::db::audit) fn claim_invoice(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    manifest: &ProjectInvoice,
    issued: &IssuedDocument,
) -> Result<(), CliError> {
    let project = find_project(company, &manifest.project_id)?;
    if issued.currency != project.terms.currency || issued.lines.len() != manifest.lines.len() {
        return Err(ProjectError::SourceUnavailable.into());
    }
    let mut after = manifest.clone();
    for line in &mut after.lines {
        line.claimed_on = Some(issued.issue_date);
    }
    store_manifest(
        tx,
        actor,
        company,
        Some(manifest),
        &after,
        AuditAction::ProjectInvoiceClaim,
    )?;
    attribute_invoice(tx, actor, company, issued.posting_id, &project.id)
}

pub(in crate::db::audit) fn apply_invoice_credit(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    document: &DocumentId,
    (line_number, full_line_minor): (u32, i64),
    credit_posting: TransactionId,
) -> Result<(), CliError> {
    let Some(before) = load_manifest(company, document)? else {
        return Ok(());
    };
    attribute_invoice(tx, actor, company, credit_posting, &before.project_id)?;
    let (credited, latest): (i64, NaiveDate) = company.read().query_row(
        "SELECT SUM(amount_minor),MAX(effective_on) FROM document_allocations
         WHERE company_slug=?1 AND document_id=?2 AND credit_line=?3",
        params![company.slug(), document, line_number],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut after = before.clone();
    let position = line_number
        .checked_sub(1)
        .ok_or(ProjectError::SourceUnavailable)? as usize;
    let line = after
        .lines
        .get_mut(position)
        .ok_or(ProjectError::SourceUnavailable)?;
    if line.released_on.is_none() && credited == full_line_minor {
        line.released_on = Some(latest);
        store_manifest(
            tx,
            actor,
            company,
            Some(&before),
            &after,
            AuditAction::ProjectInvoiceRelease,
        )?;
    }
    Ok(())
}
