//! Keep confirmed dispute balance movements visible for accounting review.

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use super::{Actor, AuditEntity, CollectionOutcome, ReadConn, record, validation};
use crate::{
    db::{
        amounts::PositiveMinorUnits,
        audit::savepoint::AuditedTx,
        calendar_date,
        companies::{Company, CompanySlug},
        ids::TransactionId,
    },
    error::CliError,
    payments::stripe::{StripeDisputeId, StripeEvent, VerifiedStripeEvent},
};

pub(super) fn ingest(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
) -> Result<CollectionOutcome, CliError> {
    let StripeEvent::DisputeFunds {
        dispute,
        payment,
        amount,
        movement,
    } = event.payload()
    else {
        return Ok(CollectionOutcome::Unrelated);
    };
    Company::bind(tx.read(), company.clone())?;
    let provider = event.account().to_string();
    let live = event.mode().is_live();
    let payment = payment.to_string();
    let currency = amount.money().currency();
    let amount: PositiveMinorUnits = amount.money().amount().try_into()?;
    let occurred = event.created().date_naive();
    let movement = movement.code();
    let id = dispute.to_string();
    if let Some(saved) = tx
        .read()
        .query_row(
            "SELECT account,live,payment,currency,amount FROM stripe_disputes
        WHERE company_slug=?1 AND dispute=?2 AND movement=?3",
            params![company, id, movement],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Currency>(3)?,
                    row.get::<_, PositiveMinorUnits>(4)?,
                ))
            },
        )
        .optional()?
    {
        if saved == (provider, live, payment, currency, amount) {
            return Ok(CollectionOutcome::Duplicate);
        }
        return Err(validation(
            "verified dispute movement conflicts with recorded terms",
        ));
    }
    let delivery = event.id().to_string();
    record(
        tx,
        actor,
        company,
        AuditEntity::StripeDispute,
        &delivery,
        &json!({
            "dispute":id,"provider":provider,"live":live,"movement":movement,"payment":payment,
            "occurred":occurred,"currency":currency,"amount":amount,
        }),
    )?;
    tx.insert("INSERT INTO stripe_disputes(company_slug,dispute,movement,account,live,payment,occurred,currency,amount,delivery)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![company,id,movement,provider,live,payment,occurred,currency,amount,delivery])?;
    Ok(CollectionOutcome::DisputeRecorded(dispute.clone()))
}

#[derive(Serialize)]
pub struct DisputeView {
    dispute: StripeDisputeId,
    movement: String,
    payment: String,
    occurred: NaiveDate,
    currency: Currency,
    amount_minor: PositiveMinorUnits,
    original_receipt: Option<TransactionId>,
    allocation_effect: &'static str,
}

pub(crate) fn report(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Vec<DisputeView>, CliError> {
    let mut stmt = reader.prepare_cached("SELECT d.dispute,d.movement,d.payment,d.occurred,d.currency,d.amount,
        (SELECT e.posting FROM collection_events e JOIN invoice_collections c ON c.id=e.collection
         WHERE e.payment=d.payment AND c.company_slug=d.company_slug AND c.account=d.account AND c.live=d.live AND c.currency=d.currency)
        FROM stripe_disputes d WHERE d.company_slug=?1 ORDER BY d.occurred DESC,d.dispute LIMIT 100")?;
    let rows = stmt.query_map([company], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get(1)?,
            row.get(2)?,
            calendar_date::read(row, 3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    })?;
    rows.map(|row| {
        let (id, movement, payment, occurred, currency, amount_minor, original_receipt) = row?;
        Ok(DisputeView {
            dispute: id.parse().map_err(validation)?,
            movement,
            payment,
            occurred,
            currency,
            amount_minor,
            original_receipt,
            allocation_effect: "unchanged_pending_review",
        })
    })
    .collect()
}
