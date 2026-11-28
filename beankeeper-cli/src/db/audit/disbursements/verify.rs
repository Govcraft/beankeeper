//! Verify payment projections and journals against signed audit and provider evidence.

use beankeeper::types::DebitOrCredit;
use rusqlite::params;

use crate::{
    db::{
        companies::Company,
        connection::ReadConn,
        disbursements::{Disbursement, ProviderEvidence, decode},
        documents::DocumentId,
        ids::TransactionId,
        transactions::get_transaction,
    },
    disbursement::TransferState,
    error::CliError,
};

pub(in crate::db) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let history = reader.bookkeeping_audit();
    let invalid: bool = reader.query_row(
        &format!(
            "SELECT EXISTS(
                SELECT 1 FROM disbursements d WHERE NOT EXISTS(
                    SELECT 1 FROM {history} a WHERE a.id=d.receipt AND a.entity='disbursement'
                      AND a.company_slug=d.company_slug AND a.entity_id=d.id AND a.after=d.details
                      AND NOT EXISTS(SELECT 1 FROM {history} n WHERE n.entity=a.entity
                         AND n.company_slug=a.company_slug AND n.entity_id=a.entity_id AND n.id>a.id)
                )) OR EXISTS(
                SELECT 1 FROM {history} a WHERE a.entity='disbursement' AND NOT EXISTS(
                    SELECT 1 FROM disbursements d WHERE d.company_slug=a.company_slug AND d.id=a.entity_id)
                )"
        ),
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(bad("payment state differs from its audit evidence"));
    }
    let mut query = reader
        .prepare("SELECT company_slug,details FROM disbursements ORDER BY company_slug,receipt")?;
    let rows = query.query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (slug, details) = row?;
        let company = Company::bind_retained(reader, slug)?;
        verify_record(&company, &decode(&details)?)?;
    }
    Ok(())
}

fn verify_record(company: &Company<'_>, record: &Disbursement) -> Result<(), CliError> {
    let mut bills = company.read().prepare_cached(
        "SELECT document_id,amount_minor FROM disbursement_bills
         WHERE company_slug=?1 AND disbursement_id=?2 ORDER BY document_id",
    )?;
    let rows = bills
        .query_map(params![company.slug(), record.id], |row| {
            Ok((row.get::<_, DocumentId>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() != record.terms.bills.len()
        || !rows
            .iter()
            .zip(&record.terms.bills)
            .all(|((id, amount), bill)| {
                id == &bill.document_id && *amount == bill.amount_minor.minor_units()
            })
    {
        return Err(bad(
            "reserved bill allocations differ from the frozen payment instruction",
        ));
    }
    if let Some(review) = &record.review
        && (review.reviewer == record.author
            || review.fingerprint != super::fingerprint(company, record, &review.destination)?)
    {
        return Err(bad(
            "payment approval does not identify the frozen terms and independent owner",
        ));
    }
    if !matches!(
        record.state,
        TransferState::Draft | TransferState::Approved(_) | TransferState::Rejected
    ) && !super::attempt_exists(company.read(), &record.id)?
    {
        return Err(bad(
            "dispatched payment is missing its durable attempt record",
        ));
    }
    let settlement_date = verify_journal(
        company,
        record,
        record.settlement_posting,
        record.confirmed.as_ref(),
        DebitOrCredit::Debit,
    )?;
    let return_date = verify_journal(
        company,
        record,
        record.return_posting,
        record.returned.as_ref(),
        DebitOrCredit::Credit,
    )?;
    if let Some(settlement) = record.settlement_posting {
        let mut allocations = company.read().prepare_cached(
            "SELECT document_id,amount_minor,effective_on,reversed_on FROM document_allocations
             WHERE company_slug=?1 AND source_id=?2 ORDER BY document_id",
        )?;
        let rows = allocations
            .query_map(params![company.slug(), settlement], |row| {
                Ok((
                    row.get::<_, DocumentId>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, chrono::NaiveDate>(2)?,
                    row.get::<_, Option<chrono::NaiveDate>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() != record.terms.bills.len()
            || !rows
                .iter()
                .zip(&record.terms.bills)
                .all(|((id, amount, date, reversed), bill)| {
                    id == &bill.document_id
                        && *amount == bill.amount_minor.minor_units()
                        && Some(*date) == settlement_date
                        && *reversed == return_date
                })
        {
            return Err(bad(
                "payment journal allocations differ from the frozen instruction",
            ));
        }
    }
    Ok(())
}

fn verify_journal(
    company: &Company<'_>,
    record: &Disbursement,
    posting: Option<TransactionId>,
    evidence: Option<&ProviderEvidence>,
    direction: DebitOrCredit,
) -> Result<Option<chrono::NaiveDate>, CliError> {
    let Some(posting) = posting else {
        return Ok(None);
    };
    let evidence = evidence.ok_or_else(|| bad("payment journal has no provider evidence"))?;
    let (txn, rows) = get_transaction(company, posting)?;
    let matches = |account: &str, side| {
        rows.iter().any(|row| {
            row.account_code == account
                && row.direction == side
                && row.amount == record.amount_minor
        })
    };
    if txn.date < evidence.on
        || txn.currency != record.currency
        || rows.len() != 2
        || !matches(record.payable_account.as_str(), direction)
        || !matches(record.terms.cash_account.as_str(), direction.opposite())
    {
        return Err(bad(
            "payment journal disagrees with authenticated provider evidence",
        ));
    }
    Ok(Some(txn.date))
}

fn bad(message: &str) -> CliError {
    CliError::Database(message.into())
}
