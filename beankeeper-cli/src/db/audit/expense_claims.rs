//! Records expense workflow writes and their immutable audit evidence.

use rusqlite::params;
use serde::Serialize;

use super::{Actor, Creation, record_insert, savepoint::AuditedTx};
use crate::{
    db::{
        companies::CompanySlug,
        expense_claims::{ClaimView, ReceiptView},
        ids::TransactionId,
    },
    error::CliError,
};

fn details(value: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_value(value)
        .map(|object| object.to_string())
        .map_err(|error| CliError::Database(format!("expense data: {error}")))
}

pub(in crate::db) fn store_receipt(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    view: &ReceiptView,
) -> Result<(), CliError> {
    tx.connection().execute(
        "INSERT INTO receipt_intakes(id,company_slug,digest,details) VALUES (?1,?2,?3,?4)",
        params![view.id, slug, view.hash.as_str(), details(view)?],
    )?;
    record_insert(
        tx.connection(),
        actor,
        slug,
        Creation::ReceiptIntake,
        &view.id.to_string(),
        &serde_json::json!(view),
    )
}

pub(in crate::db) fn store_claim(
    tx: &AuditedTx<'_>,
    slug: &CompanySlug,
    claim: &ClaimView,
    reference: &str,
) -> Result<(), CliError> {
    tx.connection().execute(
        "INSERT INTO expense_claims(id,company_slug,receipt_id,claimant,request_key) VALUES (?1,?2,?3,?4,?5)",
        params![claim.id,slug,claim.terms.receipt,claim.terms.claimant,reference],
    )?;
    Ok(())
}

pub(in crate::db) fn store_revision(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    claim: &ClaimView,
    event: &str,
    reference: &str,
) -> Result<(), CliError> {
    let version = claim.revision.get();
    tx.connection().execute(
        "INSERT INTO expense_claim_revisions(company_slug,claim_id,revision,status,event,change_key,details)
            VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![slug,claim.id,version,claim.status.code(),event,reference,details(claim)?],
    )?;
    record_insert(
        tx.connection(),
        actor,
        slug,
        Creation::ExpenseClaimRevision,
        &format!("{}:{version}", claim.id),
        &serde_json::json!(claim),
    )
}

pub(in crate::db) fn link_journal(
    tx: &AuditedTx<'_>,
    slug: &CompanySlug,
    claim: &ClaimView,
    transaction: TransactionId,
    role: &str,
) -> Result<(), CliError> {
    tx.connection().execute(
        "INSERT INTO expense_claim_journals(transaction_id,company_slug,claim_id,role) VALUES (?1,?2,?3,?4)",
        params![transaction,slug,claim.id,role],
    )?;
    Ok(())
}
