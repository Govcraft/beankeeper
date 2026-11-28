//! Payments observed by an agent use the same audited journal and allocation as Checkout.

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::{
    Actor, AuditedTx, CollectionDecision, CollectionOutcome, CollectionRequest, Company,
    CompanySlug, EventRecord, ReadConn, ReceiptDelivery, StripeEvent, StripePaymentIntentId,
    VerifiedStripeEvent, apply_payment, attempt, invoice_amount, linked_checkout,
    reported::{self, Facts, Origin, PaymentTarget, PaymentView, Source},
    reserve, review_observation, state, store_event, url_digest, validation,
};
use crate::{
    db::{documents::DocumentId, ids::TransactionId},
    error::CliError,
    payments::stripe::{CollectionId, StripeSessionId},
};

pub(super) fn ingest_reported_payment(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    facts: &Facts,
    payment: &StripePaymentIntentId,
    target: &PaymentTarget,
) -> Result<CollectionOutcome, CliError> {
    let date = NaiveDate::from(facts.date);
    let amount = facts.amount()?;
    if let Some(saved) = reported_receipt(tx.read(), company.slug(), payment)? {
        if &saved.facts != facts || !saved.matches_target(target) {
            return Err(validation("Stripe payment conflicts with recorded terms"));
        }
        return Ok(CollectionOutcome::PaymentReported {
            collection: saved.collection,
            transaction: saved.posting,
        });
    }
    let attempt = match target {
        PaymentTarget::Invoice {
            invoice,
            clearing_account,
        } => {
            let reserved = reserve(
                tx,
                actor,
                &CollectionRequest {
                    company: company.slug().clone(),
                    invoice: invoice.clone(),
                    account: facts.account.clone(),
                    mode: facts.mode(),
                    amount,
                    request_key: format!("stripe-payment:{}", url_digest(&payment.to_string()))
                        .parse()?,
                    clearing_account: clearing_account.clone(),
                    as_of: date,
                },
            )?;
            if state(tx.read(), &reserved.terms.id)? != super::CollectionState::Requested {
                return Err(validation(
                    "existing collection cannot be used for an external payment",
                ));
            }
            reserved
        }
        PaymentTarget::Checkout {
            collection,
            session,
        } => {
            let attempt = attempt(tx.read(), company.slug(), collection)?;
            let outstanding = invoice_amount(company, &attempt.terms.invoice, date)?;
            let decision = review_observation(
                &attempt.terms,
                &state(tx.read(), collection)?,
                outstanding,
                (collection, session, Some((payment, amount))),
                (&facts.account, facts.mode()),
            );
            match decision {
                CollectionDecision::ApplyReceipt { .. } => attempt,
                CollectionDecision::Review(reason) => return Err(validation(reason)),
                _ => {
                    return Err(validation(
                        "Checkout already has a payment or has no applicable receipt",
                    ));
                }
            }
        }
    };
    let (outcome, recorded) = apply_payment(
        tx,
        actor,
        company,
        &attempt,
        date,
        (payment.clone(), amount, Source::AgentReported),
    )?;
    let CollectionOutcome::Applied(transaction) = outcome else {
        return Err(validation(super::CLOSED_PERIOD_REVIEW));
    };
    let event_id = format!("agent:{payment}");
    let digest = url_digest(&json!({"facts":facts,"target":target,"payment":payment}).to_string());
    store_event(
        tx,
        actor,
        company.slug(),
        &attempt.terms.id,
        ReceiptDelivery {
            id: event_id,
            account: &facts.account,
            digest: &digest,
            date,
            source: Source::AgentReported,
        },
        EventRecord::original(&outcome, recorded.as_ref()),
    )?;
    Ok(CollectionOutcome::PaymentReported {
        collection: attempt.terms.id,
        transaction,
    })
}

pub(super) fn compare_reported_payment(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
    payment: &StripePaymentIntentId,
    amount: beankeeper::types::PositiveMoney,
) -> Result<Option<CollectionOutcome>, CliError> {
    let Some(saved) = reported_receipt(tx.read(), company, payment)? else {
        return Ok(None);
    };
    if saved.source == Source::Signed {
        return Ok(None);
    }
    let facts = Facts::signed(event, event.created().date_naive(), amount)?;
    let checkout_agrees = match (event.payload(), &saved.session) {
        (
            StripeEvent::CheckoutPaid {
                collection,
                session,
                ..
            },
            expected,
        ) => collection == &saved.collection && expected.as_ref() == Some(session),
        (StripeEvent::PaymentSucceeded { .. }, _) => true,
        _ => false,
    };
    reported::compare(
        tx,
        actor,
        company,
        &payment.to_string(),
        (
            saved.source,
            Origin::Signed(event),
            saved.facts == facts && checkout_agrees,
        ),
    )
    .map(Some)
}

struct Receipt {
    collection: CollectionId,
    facts: Facts,
    invoice: DocumentId,
    clearing: String,
    session: Option<StripeSessionId>,
    posting: TransactionId,
    source: Source,
}

impl Receipt {
    fn matches_target(&self, target: &PaymentTarget) -> bool {
        match target {
            PaymentTarget::Checkout {
                collection,
                session,
            } => collection == &self.collection && self.session.as_ref() == Some(session),
            PaymentTarget::Invoice {
                invoice,
                clearing_account,
            } => {
                invoice == &self.invoice
                    && clearing_account.as_str() == self.clearing
                    && self.session.is_none()
            }
        }
    }
}

fn reported_receipt(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    payment: &StripePaymentIntentId,
) -> Result<Option<Receipt>, CliError> {
    reader
        .query_row(
            "SELECT e.collection,c.account,c.live,e.occurred_on,c.currency,c.amount,c.invoice,
                c.clearing_account,e.posting,e.source
         FROM collection_events e JOIN invoice_collections c ON c.id=e.collection
         WHERE e.payment=?1 AND c.company_slug=?2",
            params![payment.to_string(), company],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                    crate::db::calendar_date::read(row, 3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                id,
                account,
                live,
                date,
                currency,
                amount_minor,
                invoice,
                clearing,
                posting,
                source,
            )| {
                let collection = id.parse().map_err(validation)?;
                let session = linked_checkout(reader, &collection)?.map(|link| link.session);
                Ok(Receipt {
                    collection,
                    facts: Facts {
                        account: account.parse().map_err(validation)?,
                        live,
                        date: date.into(),
                        currency,
                        amount_minor,
                    },
                    invoice,
                    clearing,
                    session,
                    posting,
                    source: Source::from_row(&source)?,
                })
            },
        )
        .transpose()
}

pub(super) fn payment_status(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    collection: &CollectionId,
) -> Result<Option<PaymentView>, CliError> {
    reader
        .query_row(
            "SELECT e.payment,e.posting,e.source FROM collection_events e
         JOIN invoice_collections c ON c.id=e.collection
         WHERE e.collection=?1 AND c.company_slug=?2 AND e.status='applied'",
            params![collection.to_string(), company],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(key, posting, original_source)| {
            let payment = key.parse().map_err(validation)?;
            let original_source = Source::from_row(&original_source)?;
            Ok(PaymentView {
                payment,
                posting,
                source: original_source.effective(reader, company, &key)?,
                original_source,
            })
        })
        .transpose()
}

impl<'a> From<&'a VerifiedStripeEvent> for ReceiptDelivery<'a> {
    fn from(event: &'a VerifiedStripeEvent) -> Self {
        Self {
            id: event.id().to_string(),
            account: event.account(),
            digest: event.body_sha256(),
            date: event.created().date_naive(),
            source: Source::Signed,
        }
    }
}
