//! Keep confirmed dispute balance movements visible for accounting review.

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use super::{
    Actor, AuditEntity, CollectionOutcome, ReadConn, record,
    reported::{self, Facts, Origin, Source},
    validation,
};
use crate::{
    db::{
        amounts::PositiveMinorUnits,
        audit::savepoint::AuditedTx,
        calendar_date,
        companies::{Company, CompanySlug},
        ids::TransactionId,
    },
    error::CliError,
    payments::stripe::{
        DisputeMovement, StripeDisputeId, StripeEvent, StripePaymentIntentId, VerifiedStripeEvent,
    },
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
    let facts = Facts::signed(event, event.created().date_naive(), *amount)?;
    record_evidence(
        tx,
        actor,
        company,
        (dispute, payment, *movement, &facts),
        Origin::Signed(event),
    )
}

pub(super) fn record_evidence(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    (dispute, payment, movement, facts): (
        &StripeDisputeId,
        &StripePaymentIntentId,
        DisputeMovement,
        &Facts,
    ),
    origin: Origin<'_>,
) -> Result<CollectionOutcome, CliError> {
    Company::bind(tx.read(), company.clone())?;
    let provider = facts.account.to_string();
    let payment = payment.to_string();
    let occurred = NaiveDate::from(facts.date);
    let movement = movement.code();
    let id = dispute.to_string();
    let key = format!("{id}:{movement}");
    if let Some((old_provider, live, old_payment, currency, amount, old_date, old_source)) = tx
        .read()
        .query_row(
            "SELECT account,live,payment,currency,amount,occurred,source FROM stripe_disputes
             WHERE company_slug=?1 AND dispute=?2 AND movement=?3",
            params![company, id, movement],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Currency>(3)?,
                    row.get::<_, PositiveMinorUnits>(4)?,
                    calendar_date::read(row, 5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?
    {
        let old_source = Source::from_row(&old_source)?;
        let agrees = (old_provider, live, old_payment, currency, amount)
            == (
                provider,
                facts.live,
                payment,
                facts.currency,
                facts.amount_minor,
            )
            && (origin.source() == Source::Signed && old_source == Source::Signed
                || old_date == occurred);
        return reported::compare(tx, actor, company, &key, (old_source, origin, agrees));
    }
    if origin.source() == Source::AgentReported {
        let known: bool = tx.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM collection_events e JOIN invoice_collections c ON c.id=e.collection
                WHERE e.payment=?1 AND c.company_slug=?2 AND c.account=?3 AND c.live=?4 AND c.currency=?5
                  AND e.occurred_on<=?6 AND c.amount>=?7)",
            params![payment,company,provider,facts.live,facts.currency,occurred,facts.amount_minor],
            |row| row.get(0),
        )?;
        if !known {
            return Err(validation(
                "reported dispute must match a receipt in the same processor scope",
            ));
        }
    }
    let delivery = origin.delivery(&key);
    record(
        tx,
        actor,
        company,
        AuditEntity::StripeDispute,
        &delivery,
        &json!({
            "dispute":id, "provider":provider, "live":facts.live, "movement":movement,
            "payment":payment, "occurred":occurred, "currency":facts.currency,
            "amount":facts.amount_minor, "source":origin.source(),
        }),
    )?;
    tx.insert("INSERT INTO stripe_disputes(company_slug,dispute,movement,account,live,payment,occurred,currency,amount,delivery,source)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![company,id,movement,provider,facts.live,payment,occurred,facts.currency,facts.amount_minor,delivery,origin.source().code()])?;
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
    source: Source,
    original_source: Source,
}

pub(crate) fn report(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Vec<DisputeView>, CliError> {
    let mut stmt = reader.prepare_cached("SELECT d.dispute,d.movement,d.payment,d.occurred,d.currency,d.amount,
        (SELECT e.posting FROM collection_events e JOIN invoice_collections c ON c.id=e.collection
         WHERE e.payment=d.payment AND c.company_slug=d.company_slug AND c.account=d.account AND c.live=d.live AND c.currency=d.currency),d.source
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
            row.get::<_, String>(7)?,
        ))
    })?;
    rows.map(|row| {
        let (
            id,
            movement,
            payment,
            occurred,
            currency,
            amount_minor,
            original_receipt,
            original_source,
        ) = row?;
        let original_source = Source::from_row(&original_source)?;
        let key = format!("{id}:{movement}");
        Ok(DisputeView {
            dispute: id.parse().map_err(validation)?,
            movement,
            payment,
            occurred,
            currency,
            amount_minor,
            original_receipt,
            source: original_source.effective(reader, company, &key)?,
            original_source,
            allocation_effect: "unchanged_pending_review",
        })
    })
    .collect()
}
