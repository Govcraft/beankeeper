//! Separate irreversible provider evidence from retryable ledger posting.

use beankeeper::types::DebitOrCredit;
use chrono::NaiveDate;
use rusqlite::params;

use crate::{
    db::{
        audit::{
            Actor,
            documents::{apply_payment, return_disbursement_allocation},
            post_in_audited_scope,
            savepoint::AuditedTx,
        },
        companies::Company,
        disbursements::{Disbursement, read},
        documents::AllocationId,
        ids::TransactionId,
        periods,
        transactions::{ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams},
    },
    disbursement::DisbursementId,
    error::CliError,
};

use super::{invalid, persist};

pub(super) fn reconcile(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    id: &DisbursementId,
    date: NaiveDate,
) -> Result<Disbursement, CliError> {
    let mut before = read(company, id)?;
    if before.settlement_posting.is_none() {
        let confirmed = before
            .confirmed
            .as_ref()
            .ok_or_else(|| invalid("the host has not confirmed settlement"))?;
        if date < confirmed.on {
            return Err(invalid("accounting date precedes provider settlement"));
        }
        periods::ensure_open(tx.read(), company.slug(), date)?;
        let posting = post(tx, actor, company, &before, date, PaymentLeg::Settlement)?;
        let mut after = before.clone();
        after.settlement_posting = Some(posting);
        persist(tx, actor, company, Some(&before), &after)?;
        apply_payment(tx, actor, company, posting, date, after.terms.bills.clone())?;
        before = after;
    }
    if let Some(returned) = &before.returned
        && before.return_posting.is_none()
    {
        if date < returned.on {
            return Err(invalid("accounting date precedes the provider return"));
        }
        periods::ensure_open(tx.read(), company.slug(), date)?;
        let settlement = before
            .settlement_posting
            .ok_or_else(|| invalid("the settlement journal must be recorded first"))?;
        let mut stmt = tx.read().prepare(
            "SELECT id FROM document_allocations WHERE company_slug=?1 AND source_id=?2 AND reversed_on IS NULL ORDER BY id",
        )?;
        let active = stmt
            .query_map(params![company.slug(), settlement], |row| {
                row.get::<_, AllocationId>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if active.len() != before.terms.bills.len() {
            return Err(CliError::Database(
                "payment allocations do not match the original disbursement".into(),
            ));
        }
        let posting = post(tx, actor, company, &before, date, PaymentLeg::Return)?;
        for allocation in active {
            return_disbursement_allocation(tx, actor, company, &allocation, date)?;
        }
        let mut after = before.clone();
        after.return_posting = Some(posting);
        persist(tx, actor, company, Some(&before), &after)?;
        before = after;
    }
    Ok(before)
}

#[derive(Clone, Copy)]
enum PaymentLeg {
    Settlement,
    Return,
}

fn post(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    record: &Disbursement,
    date: NaiveDate,
    kind: PaymentLeg,
) -> Result<TransactionId, CliError> {
    let (label, direction) = match kind {
        PaymentLeg::Settlement => ("Vendor payment", DebitOrCredit::Debit),
        PaymentLeg::Return => ("Vendor payment return", DebitOrCredit::Credit),
    };
    let prefix = match kind {
        PaymentLeg::Settlement => "provider-settlement",
        PaymentLeg::Return => "provider-return",
    };
    let reference = format!("{prefix}-{}", record.id);
    let entries = [
        PostEntryParams {
            account_code: record.payable_account.to_string(),
            direction,
            amount: record.amount_minor,
            memo: None,
            tax_category: None,
        },
        PostEntryParams {
            account_code: record.terms.cash_account.to_string(),
            direction: direction.opposite(),
            amount: record.amount_minor,
            memo: None,
            tax_category: None,
        },
    ];
    match post_in_audited_scope(
        tx,
        &PostTransactionParams {
            company_slug: company.slug(),
            description: &format!("{label} {}", record.id),
            metadata: None,
            currency: record.currency,
            date,
            entries: &entries,
            correlate: None,
            reference: Some(&reference),
            on_conflict: ConflictStrategy::Error,
            actor,
        },
    )? {
        PostResult::Created(id) => Ok(id),
        PostResult::Skipped(_) => Err(invalid("payment journal reference already exists")),
    }
}
