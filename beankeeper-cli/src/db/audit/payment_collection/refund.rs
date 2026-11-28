//! Retain verified refund deliveries and reopen the invoice by the posted amount.

use beankeeper::types::{Amount, Currency, DebitOrCredit};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Actor, AuditEntity, CollectionOutcome, Db, ReadConn, record, validation};
use crate::{
    db::{
        self, PostEntryParams, PostResult, PostTransactionParams,
        amounts::PositiveMinorUnits,
        audit::savepoint::AuditedTx,
        companies::{Company, CompanySlug},
        documents::{DocumentId, find_document},
        ids::TransactionId,
    },
    error::CliError,
    identity::{Granted, Post},
    payments::stripe::{StripeEvent, StripeRefundId, VerifiedStripeEvent},
    request::CalendarDateArg,
};

pub(super) fn ingest(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
) -> Result<CollectionOutcome, CliError> {
    let StripeEvent::RefundSucceeded {
        refund,
        payment,
        amount,
    } = event.payload()
    else {
        return Ok(CollectionOutcome::Unrelated);
    };
    Company::bind(tx.read(), company.clone())?;
    let key = refund.to_string();
    let provider = event.account().to_string();
    let live = event.mode().is_live();
    let payment = payment.to_string();
    let occurred = event.created().date_naive();
    let currency = amount.money().currency();
    let minor: PositiveMinorUnits = amount.money().amount().try_into()?;
    if let Some(old) = evidence(tx.read(), company, refund)? {
        if old.provider == provider
            && old.live == live
            && old.payment == payment
            && old.currency == currency
            && old.amount == minor
        {
            return Ok(CollectionOutcome::Duplicate);
        }
        return Err(validation(
            "verified refund conflicts with the recorded terms",
        ));
    }
    let delivery = event.id().to_string();
    record(
        tx,
        actor,
        company,
        AuditEntity::StripeRefund,
        &key,
        &json!({
            "provider":provider, "live":live,"payment":payment, "occurred":occurred, "currency":currency, "amount":minor, "delivery":delivery,
        }),
    )?;
    tx.insert("INSERT INTO stripe_refunds(company_slug,refund,account,live,payment,occurred,currency,amount,delivery)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![company, key, provider, live, payment, occurred, currency, minor, delivery])?;
    Ok(CollectionOutcome::RefundRecorded(refund.clone()))
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefundSettlement {
    pub refund: StripeRefundId,
    /// An open accounting date on or after both the payment and refund.
    pub booked_on: CalendarDateArg,
}

#[derive(Serialize)]
pub struct RefundPosting {
    transaction: TransactionId,
    existing: bool,
}

pub(crate) fn reconcile(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    terms: &RefundSettlement,
) -> Result<RefundPosting, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_stripe_refund_reconcile", |tx, actor| {
            reconcile_in_scope(tx, actor, company, terms)
        })
    })
}

fn reconcile_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    terms: &RefundSettlement,
) -> Result<RefundPosting, CliError> {
    let scope = Company::bind(tx.read(), company.clone())?;
    let saved = evidence(tx.read(), company, &terms.refund)?.ok_or_else(|| {
        CliError::NotFound("no verified Stripe refund exists in this company".into())
    })?;
    let date = NaiveDate::from(terms.booked_on);
    if let Some((transaction, original_date)) = tx.read().query_row(
        "SELECT posting,booked_on FROM stripe_refund_postings WHERE company_slug=?1 AND refund=?2",
        params![company, terms.refund.to_string()], |row| Ok((row.get::<_, TransactionId>(0)?, db::calendar_date::read(row, 1)?)),
    ).optional()? {
        if date == original_date {
            return Ok(RefundPosting { transaction, existing: true });
        }
        return Err(validation("refund was already recorded on a different date"));
    }
    let receipt = source_receipt(tx.read(), company, &saved)?;
    if date < saved.occurred || date < receipt.date {
        return Err(validation(
            "refund posting cannot precede the refund or the original receipt",
        ));
    }
    let previously_refunded: i64 = tx.read().query_row(
        "SELECT COALESCE(SUM(amount),0) FROM stripe_refund_postings WHERE company_slug=?1 AND original_payment=?2",
        params![company, receipt.posting], |row| row.get(0),
    )?;
    let cumulative = Amount::from(previously_refunded)
        .checked_add(saved.amount.into())
        .ok_or_else(|| validation("refunds exceed the supported amount"))?;
    if cumulative > Amount::from(receipt.amount) {
        return Err(validation("cumulative refunds exceed the original receipt"));
    }
    let document = find_document(&scope, &receipt.invoice)?
        .issued
        .ok_or_else(|| validation("the invoice is no longer issued"))?;
    let entry = |account_code, direction| PostEntryParams {
        account_code,
        direction,
        amount: saved.amount,
        memo: None,
        tax_category: None,
    };
    let entries = [
        entry(document.control_account.to_string(), DebitOrCredit::Debit),
        entry(receipt.clearing, DebitOrCredit::Credit),
    ];
    let key = terms.refund.to_string();
    let reference = format!("stripe-refund:{key}");
    let posting = PostTransactionParams {
        company_slug: company,
        description: "Stripe refund",
        metadata: None,
        currency: saved.currency,
        date,
        entries: &entries,
        correlate: None,
        reference: Some(&reference),
        on_conflict: db::ConflictStrategy::Error,
        actor,
    };
    let PostResult::Created(transaction) = super::super::post_in_audited_scope(tx, &posting)?
    else {
        return Err(CliError::Database(
            "Stripe refund was unexpectedly skipped".into(),
        ));
    };
    record(
        tx,
        actor,
        company,
        AuditEntity::StripeRefundPosting,
        &key,
        &json!({"posting":transaction,
        "original_payment":receipt.posting,"date":date,"invoice":receipt.invoice,"amount":saved.amount}),
    )?;
    tx.insert("INSERT INTO stripe_refund_postings(company_slug,refund,posting,original_payment,invoice,booked_on,amount)
        VALUES(?1,?2,?3,?4,?5,?6,?7)", params![company, key, transaction, receipt.posting, receipt.invoice, date, saved.amount])?;
    Ok(RefundPosting {
        transaction,
        existing: false,
    })
}

struct Evidence {
    provider: String,
    live: bool,
    payment: String,
    occurred: NaiveDate,
    currency: Currency,
    amount: PositiveMinorUnits,
}

fn evidence(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    refund: &StripeRefundId,
) -> Result<Option<Evidence>, CliError> {
    Ok(reader.query_row("SELECT account,live,payment,occurred,currency,amount FROM stripe_refunds WHERE company_slug=?1 AND refund=?2",
        params![company, refund.to_string()], |row| Ok(Evidence { provider:row.get(0)?,live:row.get(1)?,payment:row.get(2)?,
            occurred:db::calendar_date::read(row,3)?,currency:row.get(4)?,amount:row.get(5)? })).optional()?)
}

struct SourceReceipt {
    posting: TransactionId,
    invoice: DocumentId,
    clearing: String,
    amount: PositiveMinorUnits,
    date: NaiveDate,
}

fn source_receipt(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    evidence: &Evidence,
) -> Result<SourceReceipt, CliError> {
    reader.query_row("SELECT e.posting,c.invoice,c.clearing_account,c.amount,t.date FROM collection_events e
        JOIN invoice_collections c ON c.id=e.collection JOIN transactions t ON t.id=e.posting
        JOIN document_allocations d ON d.source_id=e.posting AND d.company_slug=c.company_slug AND d.document_id=c.invoice
        WHERE c.company_slug=?1 AND c.account=?2 AND c.live=?3 AND e.payment=?4 AND c.currency=?5
          AND d.reversed_on IS NULL",
        params![company,evidence.provider,evidence.live,evidence.payment,evidence.currency], |row| Ok(SourceReceipt {
            posting:row.get(0)?,invoice:row.get(1)?,clearing:row.get(2)?,amount:row.get(3)?,date:db::calendar_date::read(row,4)?,
        })).optional()?.ok_or_else(|| validation("original Stripe payment is missing or differs from the verified refund"))
}

#[derive(Serialize)]
pub struct RefundView {
    refund: StripeRefundId,
    payment: String,
    occurred: NaiveDate,
    currency: Currency,
    amount_minor: PositiveMinorUnits,
    transaction: Option<TransactionId>,
}

pub(crate) fn report(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Vec<RefundView>, CliError> {
    let mut statement = reader.prepare_cached("SELECT f.refund,f.payment,f.occurred,f.currency,f.amount,r.posting
        FROM stripe_refunds f LEFT JOIN stripe_refund_postings r ON r.company_slug=f.company_slug AND r.refund=f.refund
        WHERE f.company_slug=?1 ORDER BY f.occurred DESC,f.refund LIMIT 100")?;
    let rows = statement.query_map([company], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get(1)?,
            db::calendar_date::read(row, 2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    rows.map(|row| {
        let (key, payment, occurred, currency, amount_minor, transaction) = row?;
        Ok(RefundView {
            refund: key.parse().map_err(validation)?,
            payment,
            occurred,
            currency,
            amount_minor,
            transaction,
        })
    })
    .collect()
}
