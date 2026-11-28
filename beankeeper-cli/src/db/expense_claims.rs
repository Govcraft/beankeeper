//! Company-scoped receipt intake, review and reimbursement accounting.

mod receipt;
mod types;
mod workflow;

pub(crate) use receipt::{Findings, MAX_RECEIPT_BYTES, ReceiptFile, ReceiptView, SuppliedFindings};
pub(crate) use types::{ClaimFunding, ClaimTerms};
pub use types::{ExpenseClaimId, ReceiptIntakeId};
pub(crate) use workflow::{ChangeRequest, ClaimAction, DraftRequest, apply, create, intake};

use std::path::Path;

use beankeeper::types::Sha256Hex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use self::types::{ClaimRevision, ClaimStatus, ReviewerIdentity};
use super::{
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::TransactionId,
};
use crate::error::CliError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaimView {
    pub(super) id: ExpenseClaimId,
    pub(super) revision: ClaimRevision,
    pub(super) status: ClaimStatus,
    pub(super) terms: ClaimTerms,
    draft_owner: ReviewerIdentity,
    submitter: Option<ReviewerIdentity>,
    reviewed_by: Option<ReviewerIdentity>,
    pub(super) approval_journal: Option<TransactionId>,
    pub(super) reimbursed_minor: i64,
    pub(super) reason: Option<String>,
    pub(super) duplicate_reason: Option<String>,
    pub(super) last_payment: Option<TransactionId>,
    pub(super) last_journal: Option<TransactionId>,
    pub(super) intent: Sha256Hex,
}

impl ClaimView {
    pub(crate) fn title(&self) -> String {
        let amount = crate::output::amounts::MoneyField::from_minor(
            "amount",
            self.terms.amount_minor,
            self.terms.currency,
        );
        format!(
            "{}: {} {} {} ({})",
            self.id,
            self.terms.merchant,
            amount.decimal(),
            self.terms.currency.code(),
            self.status.code()
        )
    }

    pub(crate) fn csv_row(&self) -> ClaimCsvRow<'_> {
        ClaimCsvRow {
            id: &self.id,
            status: self.status.code(),
            merchant: self.terms.merchant.as_ref(),
            currency: self.terms.currency,
            amount_minor: self.terms.amount_minor.minor_units(),
            reimbursed_minor: self.reimbursed_minor,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ClaimCsvRow<'a> {
    id: &'a ExpenseClaimId,
    status: &'static str,
    merchant: &'a str,
    currency: beankeeper::types::Currency,
    amount_minor: i64,
    reimbursed_minor: i64,
}

pub(crate) fn show(company: &Company<'_>, id: &ExpenseClaimId) -> Result<ClaimView, CliError> {
    let details: Option<String> = company.read().query_row(
        "SELECT details FROM expense_claim_revisions WHERE company_slug=?1 AND claim_id=?2 ORDER BY revision DESC LIMIT 1",
        params![company.slug(),id], |row| row.get(0),
    ).optional()?;
    details
        .as_deref()
        .map(parse)
        .transpose()?
        .ok_or_else(|| CliError::NotFound(format!("expense claim {id} is absent from this book")))
}

pub(crate) fn list(company: &Company<'_>) -> Result<Vec<ClaimView>, CliError> {
    let mut query = company.read().prepare(
        "SELECT revision.details FROM expense_claims claim
           JOIN expense_claim_revisions revision ON revision.company_slug=claim.company_slug AND revision.claim_id=claim.id
              AND revision.revision=(SELECT MAX(r.revision) FROM expense_claim_revisions r WHERE r.company_slug=claim.company_slug AND r.claim_id=claim.id)
           WHERE claim.company_slug=?1 ORDER BY claim.id DESC LIMIT 100",
    )?;
    let rows = query.query_map([company.slug()], |row| row.get::<_, String>(0))?;
    rows.map(|raw| parse(&raw?)).collect()
}

pub(crate) fn receipt_show(
    company: &Company<'_>,
    id: &ReceiptIntakeId,
) -> Result<ReceiptView, CliError> {
    let raw = company
        .read()
        .query_row(
            "SELECT details FROM receipt_intakes WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("receipt {id} is absent from this book")))?;
    parse(&raw)
}

fn parse<'a, T: Deserialize<'a>>(data: &'a str) -> Result<T, CliError> {
    serde_json::from_str(data)
        .map_err(|error| CliError::Database(format!("expense claim data: {error}")))
}

fn receipt_path(hash: &Sha256Hex) -> String {
    format!("attachments/{}", hash.as_str())
}

fn require_receipt_file(path: &Path, view: &ReceiptView) -> Result<(), CliError> {
    let file_path = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(receipt_path(&view.hash));
    let findings = SuppliedFindings {
        expected_digest: Some(view.hash.clone()),
        findings: Findings::default(),
    };
    ReceiptFile::open(&file_path, &findings).map(|_| ())
}

pub(super) fn protects(
    reader: ReadConn<'_>,
    slug: &CompanySlug,
    id: TransactionId,
) -> Result<bool, CliError> {
    if reader.schema_version()? < super::schema::EXPENSE_CLAIMS_SCHEMA_VERSION {
        return Ok(false);
    }
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM expense_claim_journals WHERE company_slug=?1 AND transaction_id=?2)",
        params![slug,id], |row| row.get(0),
    )?)
}

pub(super) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let audit = reader.bookkeeping_audit().to_string();
    verify_records(
        reader,
        &audit,
        "receipt_intakes",
        "receipt_intake",
        "record.id",
    )?;
    verify_records(
        reader,
        &audit,
        "expense_claim_revisions",
        "expense_claim_revision",
        "record.claim_id || ':' || record.revision",
    )?;
    verify_claim_journals(reader)
}

fn verify_claim_journals(reader: ReadConn<'_>) -> Result<(), CliError> {
    let is_invalid = reader.query_row(
        "WITH expected(company_slug,claim_id,transaction_id,role) AS (
            SELECT company_slug,claim_id,json_extract(details,'$.last_journal'),
                   CASE status WHEN 'approved' THEN 'liability'
                               WHEN 'card_matched' THEN 'card'
                               WHEN 'voided' THEN 'cancellation' END
              FROM expense_claim_revisions WHERE json_type(details,'$.last_journal')='integer'
            UNION ALL
            SELECT company_slug,claim_id,json_extract(details,'$.last_payment'),'payment'
              FROM expense_claim_revisions WHERE json_type(details,'$.last_payment')='integer'
        )
        SELECT EXISTS(
            SELECT company_slug,claim_id,transaction_id,role FROM expense_claim_journals
            EXCEPT SELECT * FROM expected
        ) OR EXISTS(
            SELECT * FROM expected
            EXCEPT SELECT company_slug,claim_id,transaction_id,role FROM expense_claim_journals
        ) OR EXISTS(
            SELECT 1 FROM expense_claims claim
            WHERE NOT EXISTS (
                SELECT 1 FROM expense_claim_revisions r WHERE r.company_slug=claim.company_slug
                    AND r.claim_id=claim.id AND r.revision=1 AND r.change_key=claim.request_key
            )
        ) OR EXISTS (
            SELECT 1 FROM expense_claim_revisions r JOIN expense_claims claim
              ON claim.company_slug=r.company_slug AND claim.id=r.claim_id
            WHERE claim.claimant IS NOT json_extract(r.details,'$.terms.claimant')
               OR claim.receipt_id IS NOT json_extract(r.details,'$.terms.receipt')
        )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if is_invalid {
        return Err(CliError::Database(
            "expense claim journal links or original evidence do not match their revisions".into(),
        ));
    }
    Ok(())
}

fn verify_records(
    reader: ReadConn<'_>,
    audit: &str,
    table: &str,
    entity: &str,
    record_id: &str,
) -> Result<(), CliError> {
    let invalid = reader.query_row(&format!(
        "SELECT (SELECT COUNT(*) FROM {table}) <> (SELECT COUNT(*) FROM {audit} WHERE entity=?1) OR
         EXISTS(SELECT 1 FROM {table} record WHERE NOT EXISTS(
            SELECT 1 FROM {audit} event WHERE event.company_slug=record.company_slug AND event.entity=?1
              AND event.entity_id={record_id} AND event.action='create' AND json(event.after)=json(record.details)))"
    ),[entity], |row| row.get::<_,bool>(0))?;
    if invalid {
        return Err(CliError::Database(format!(
            "expense audit evidence differs for {entity}"
        )));
    }
    Ok(())
}
