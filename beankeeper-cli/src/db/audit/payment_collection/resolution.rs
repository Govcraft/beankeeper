//! Resolve an authenticated payment whose original date was closed.

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};

use crate::{
    db::{
        Db,
        audit::{
            Actor,
            grant::{Granted, Post},
            savepoint::AuditedTx,
        },
        companies::{Company, CompanySlug},
        ids::TransactionId,
        periods,
    },
    error::CliError,
    payments::{
        collection::{CollectionDecision, review_payment},
        stripe::{CollectionId, StripeEvent, VerifiedStripeEvent},
    },
};

use super::{
    CLOSED_PERIOD_REVIEW, CollectionOutcome, EventRecord, apply_payment, attempt, invoice_amount,
    state, store_event, validation,
};

pub(crate) fn resolve(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    event: &VerifiedStripeEvent,
    booked_on: NaiveDate,
) -> Result<CollectionOutcome, CliError> {
    let StripeEvent::CheckoutPaid { collection, .. } = event.payload() else {
        return Err(validation(
            "receipt resolution requires a paid Checkout delivery",
        ));
    };
    if booked_on < event.created().date_naive() {
        return Err(validation(
            "receipt resolution date precedes the provider event",
        ));
    }
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_collection_resolve", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            apply(tx, actor, &company, collection, event, booked_on)
        })
    })
}

fn apply(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    collection: &CollectionId,
    event: &VerifiedStripeEvent,
    booked_on: NaiveDate,
) -> Result<CollectionOutcome, CliError> {
    let existing = review_evidence(company, collection, event)?;
    if let Some((posting, date)) = existing {
        return if date == booked_on {
            Ok(CollectionOutcome::Applied(posting))
        } else {
            Err(validation(
                "receipt was already resolved on a different date",
            ))
        };
    }
    let pending = attempt(company.read(), company.slug(), collection)?;
    let status = state(company.read(), collection)?;
    let open = invoice_amount(company, &pending.terms.invoice, booked_on)?;
    let CollectionDecision::ApplyReceipt { payment, amount } =
        review_payment(&pending.terms, &status, open, event)
    else {
        return Err(validation(
            "verified receipt no longer matches the checkout and open invoice balance",
        ));
    };
    periods::ensure_open(company.read(), company.slug(), booked_on)?;
    let (outcome, payment) = apply_payment(
        tx,
        actor,
        company,
        &pending,
        booked_on,
        (payment, amount, super::Source::Signed),
    )?;
    store_event(
        tx,
        actor,
        company.slug(),
        collection,
        event.into(),
        EventRecord {
            outcome: &outcome,
            payment: payment.as_ref(),
            booked_on: Some(booked_on),
        },
    )?;
    Ok(outcome)
}

fn review_evidence(
    company: &Company<'_>,
    collection: &CollectionId,
    event: &VerifiedStripeEvent,
) -> Result<Option<(TransactionId, NaiveDate)>, CliError> {
    let recorded: Option<(Option<TransactionId>, Option<NaiveDate>)> = company
        .read()
        .query_row(
            "SELECT latest.posting,latest.booked_on FROM collection_events original
         JOIN invoice_collections c ON c.id=original.collection
         LEFT JOIN collection_events latest ON latest.account=original.account
           AND latest.event=original.event AND latest.resolved=1
         WHERE c.company_slug=?1 AND c.id=?2 AND c.live=?3 AND c.account=?4
           AND original.account=?4 AND original.event=?5 AND original.resolved=0
           AND original.status='review' AND original.reason=?6
           AND original.body_sha256=?7 AND original.occurred_on=?8",
            params![
                company.slug(),
                collection.to_string(),
                event.mode().is_live(),
                event.account().to_string(),
                event.id().to_string(),
                CLOSED_PERIOD_REVIEW,
                event.body_sha256(),
                event.created().date_naive(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match recorded {
        Some((Some(posting), Some(date))) => Ok(Some((posting, date))),
        Some((None, None)) => Ok(None),
        Some(_) => Err(CliError::Database(
            "incomplete receipt resolution record".into(),
        )),
        None => Err(validation(
            "delivery does not match a recorded receipt blocked solely by a closed period",
        )),
    }
}
