//! Audited invoice collection for hosts holding payment connector credentials.

pub(crate) mod dispute;
pub(crate) mod payout;
pub(crate) mod refund;

use beankeeper::types::{AccountCode, AccountType, Currency, DebitOrCredit, Money, PositiveMoney};
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::Url;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::{Actor, AuditAction, AuditChange, AuditEntity, savepoint::AuditedTx};
use crate::{
    commands::reference::TransactionReference,
    db::{
        self, Db, PostEntryParams, PostResult, PostTransactionParams,
        amounts::PositiveMinorUnits,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        documents::{AllocateItem, DocumentId, DocumentKind, find_document, open_items_as_of},
        ids::TransactionId,
    },
    error::CliError,
    identity::{Granted, Post},
    payments::{
        checkout::{CheckoutLink, CheckoutProvider},
        collection::{CollectionDecision, CollectionState, CollectionTerms, review_payment},
        stripe::{
            CollectionId, StripeAccountId, StripeEvent, StripeMode, StripePaymentIntentId,
            VerifiedStripeEvent,
        },
    },
};

/// Typed terms a host has authorized for checkout creation.
pub struct CollectionRequest {
    /// Company whose invoice will receive the collection.
    pub company: CompanySlug,
    /// Issued invoice to collect.
    pub invoice: DocumentId,
    /// Account that owns the Checkout sessions.
    pub account: StripeAccountId,
    /// Provider environment authorized for this checkout.
    pub mode: StripeMode,
    /// Positive gross amount to collect.
    pub amount: PositiveMoney,
    /// Uniquely identifies the host's exact request within this company.
    pub request_key: TransactionReference,
    /// Asset to receive the gross payment pending processor payout.
    pub clearing_account: AccountCode,
    /// Business date to use when checking the open balance.
    pub as_of: chrono::NaiveDate,
}

/// Result of requesting or recovering a hosted link.
pub struct CollectionLink {
    /// Stable collection identity.
    pub collection: CollectionId,
    /// Attached hosted link returned by the trusted connector.
    pub checkout: CheckoutLink,
}

struct Attempt {
    terms: CollectionTerms,
    clearing: AccountCode,
    reserved_at: DateTime<Utc>,
}

const PROVIDER_RETRY_WINDOW: TimeDelta = TimeDelta::hours(20);

/// Result of applying one authenticated delivery.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum CollectionOutcome {
    /// A receipt and its allocation were recorded.
    Applied(TransactionId),
    /// The provider reported an asynchronous failure.
    Failed,
    /// An already applied payment was delivered again.
    Duplicate,
    /// The delivery needs review before it can affect the ledger.
    Review(String),
    /// Verified payout evidence awaits reconciliation.
    PayoutRecorded(crate::payments::stripe::StripePayoutId),
    /// Verified refund awaits posting and allocation reversal.
    RefundRecorded(crate::payments::stripe::StripeRefundId),
    /// Signed dispute movement awaits accounting review.
    DisputeRecorded(crate::payments::stripe::StripeDisputeId),
    /// A separate reconciliation handler owns this delivery.
    Unrelated,
}

/// Reserve the invoice terms, request a hosted link and attach its exact session.
///
/// Network I/O runs between two audited operations. A retry uses the stable
/// intent and provider key. The host must configure the connector privately.
///
/// # Errors
/// Returns authority, ledger, validation or redacted provider failures.
pub fn create_link(
    db: &Db,
    actor: &Actor,
    request: &CollectionRequest,
    provider: &impl CheckoutProvider,
) -> Result<CollectionLink, CliError> {
    let attempt = Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_collection_reserve", |tx, actor| {
            reserve(tx, actor, request)
        })
    })?;
    let terms = attempt.terms;
    if let Some(checkout) = linked_checkout(db.read(), &terms.id)? {
        return Ok(CollectionLink {
            collection: terms.id,
            checkout,
        });
    }
    if Utc::now().signed_duration_since(attempt.reserved_at) >= PROVIDER_RETRY_WINDOW {
        return Err(validation(
            "checkout outcome must be reconciled with Stripe before retrying",
        ));
    }
    let checkout = provider.create(&terms).map_err(validation)?;
    let checkout = Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_collection_attach", |tx, actor| {
            attach(tx, actor, &request.company, &terms.id, checkout)
        })
    })?;
    Ok(CollectionLink {
        collection: terms.id,
        checkout,
    })
}

fn reserve(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    request: &CollectionRequest,
) -> Result<Attempt, CliError> {
    if request.request_key.as_str().len() > 128 {
        return Err(validation(
            "collection request key must be at most 128 bytes",
        ));
    }
    let reader = tx.read();
    let company = Company::bind(reader, request.company.clone())?;
    if let Some(id) = reader
        .query_row(
            "SELECT id FROM invoice_collections WHERE company_slug = ?1 AND request_key = ?2",
            params![request.company, request.request_key.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        let id = id.parse().map_err(validation)?;
        let recorded = attempt(reader, &request.company, &id)?;
        if recorded.terms.invoice == request.invoice
            && recorded.terms.account == request.account
            && recorded.terms.mode == request.mode
            && recorded.terms.expected == request.amount
            && recorded.clearing == request.clearing_account
        {
            return Ok(recorded);
        }
        return Err(CliError::Validation(
            "collection request key already identifies different terms".into(),
        ));
    }
    let open = invoice_amount(&company, &request.invoice, request.as_of)?;
    if open.currency() != request.amount.money().currency()
        || open.amount() < request.amount.money().amount()
    {
        return Err(CliError::Validation(
            "collection exceeds open invoice amount or uses a different currency".into(),
        ));
    }
    ensure_collection_capacity(reader, request, open)?;
    let reserved_at = Utc::now();
    let reserved_at_text = reserved_at.to_rfc3339();
    let issued = find_document(&company, &request.invoice)?
        .issued
        .ok_or_else(|| validation("an invoice must be issued before checkout"))?;
    if issued.control_account == request.clearing_account {
        return Err(validation(
            "processor clearing must differ from the invoice control account",
        ));
    }
    let clearing = db::accounts::get_active_account(&company, request.clearing_account.as_str())?;
    if clearing.account_type != AccountType::Asset {
        return Err(CliError::Validation(
            "processor clearing account must be an asset".into(),
        ));
    }
    let id = CollectionId::generate();
    let currency = open.currency();
    let amount: PositiveMinorUnits = request.amount.money().amount().try_into()?;
    let source = id.to_string();
    let account = request.account.to_string();
    record(
        tx,
        actor,
        &request.company,
        AuditEntity::InvoiceCollection,
        &source,
        &json!({
            "invoice": request.invoice, "account": account, "amount": amount,
            "live": request.mode.is_live(),
            "currency": currency, "request_key": request.request_key.as_str(),
            "clearing_account": request.clearing_account, "reserved_at": reserved_at_text,
        }),
    )?;
    tx.insert("INSERT INTO invoice_collections(id,company_slug,invoice,account,amount,currency,request_key,clearing_account,reserved_at,live)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![source, request.company, request.invoice, account, amount, currency,
            request.request_key.as_str(), request.clearing_account.as_str(), reserved_at_text, request.mode.is_live()])?;
    Ok(Attempt {
        terms: CollectionTerms {
            id,
            invoice: request.invoice.clone(),
            account: request.account.clone(),
            mode: request.mode,
            expected: request.amount,
        },
        clearing: request.clearing_account.clone(),
        reserved_at,
    })
}

fn ensure_collection_capacity(
    reader: ReadConn<'_>,
    request: &CollectionRequest,
    open: Money,
) -> Result<(), CliError> {
    let reserved: i64 = reader.query_row(
        "SELECT COALESCE(SUM(c.amount), 0) FROM invoice_collections c
         WHERE c.company_slug=?1 AND c.invoice=?2 AND NOT EXISTS (
            SELECT 1 FROM collection_events e WHERE e.collection=c.id AND e.status IN ('applied', 'failed'))",
        params![request.company, request.invoice],
        |row| row.get(0),
    )?;
    let available = open
        .checked_sub(Money::from_minor(i128::from(reserved), open.currency()))
        .map_err(validation)?;
    if available.amount() < request.amount.money().amount() {
        return Err(validation(
            "existing checkout requests reserve part of this invoice balance",
        ));
    }
    Ok(())
}

fn linked_checkout(
    reader: ReadConn<'_>,
    id: &CollectionId,
) -> Result<Option<CheckoutLink>, CliError> {
    reader
        .query_row(
            "SELECT session, url FROM collection_links WHERE collection=?1",
            [id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(session, url)| {
            Ok(CheckoutLink {
                session: session.parse().map_err(validation)?,
                url: Url::parse(&url).map_err(validation)?,
            })
        })
        .transpose()
}

fn attach(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    id: &CollectionId,
    checkout: CheckoutLink,
) -> Result<CheckoutLink, CliError> {
    attempt(tx.read(), company, id)?;
    if let Some(current) = linked_checkout(tx.read(), id)? {
        if checkout.session == current.session && checkout.url == current.url {
            return Ok(current);
        }
        return Err(CliError::Validation(
            "provider returned a second checkout session for this collection".into(),
        ));
    }
    let source = id.to_string();
    let session = checkout.session.to_string();
    record(
        tx,
        actor,
        company,
        AuditEntity::CollectionLink,
        &source,
        &json!({"session":session, "url_sha256":url_digest(checkout.url.as_str())}),
    )?;
    tx.insert(
        "INSERT INTO collection_links(collection,session,url) VALUES (?1,?2,?3)",
        params![source, session, checkout.url.as_str()],
    )?;
    Ok(checkout)
}

fn invoice_amount(
    company: &Company<'_>,
    id: &DocumentId,
    as_of: chrono::NaiveDate,
) -> Result<Money, CliError> {
    open_items_as_of(company, as_of)?
        .into_iter()
        .find(|item| &item.document.id == id && item.document.kind == DocumentKind::Invoice)
        .map(|item| Money::from_minor(i128::from(item.open_minor), item.document.currency))
        .ok_or_else(|| {
            CliError::NotFound("issued invoice was not found as of the requested date".into())
        })
}

fn attempt(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: &CollectionId,
) -> Result<Attempt, CliError> {
    let row = reader
        .query_row(
            "SELECT invoice, account, amount, currency, clearing_account, reserved_at, live FROM invoice_collections
         WHERE id = ?1 AND company_slug = ?2",
            params![id.to_string(), company],
            |row| {
                Ok((
                    row.get::<_, DocumentId>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, PositiveMinorUnits>(2)?,
                    row.get::<_, Currency>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound("collection was not found in this company".into()))?;
    Ok(Attempt {
        terms: CollectionTerms {
            id: id.clone(),
            invoice: row.0,
            account: row.1.parse().map_err(validation)?,
            mode: if row.6 {
                StripeMode::Live
            } else {
                StripeMode::Sandbox
            },
            expected: PositiveMoney::new(Money::new(row.2.into(), row.3)).map_err(validation)?,
        },
        clearing: row.4.parse().map_err(validation)?,
        reserved_at: row.5.parse().map_err(validation)?,
    })
}

/// Record a verified checkout delivery under the host's ledger posting authority.
///
/// A paid event posts the gross receipt and applies it to the invoice in the
/// same transaction. Invalid or closed-period events are recorded for review.
///
/// # Errors
/// Returns ledger authorization, storage, or validation failures.
pub fn ingest_checkout(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
) -> Result<CollectionOutcome, CliError> {
    let (StripeEvent::CheckoutPaid { collection: id, .. }
    | StripeEvent::CheckoutFailed { collection: id, .. }) = event.payload()
    else {
        return Granted::<Post>::with_db(db, actor, |grant| {
            grant.mutate("sp_stripe_payout_ingest", |tx, actor| {
                match event.payload() {
                    StripeEvent::RefundSucceeded { .. } => {
                        refund::ingest(tx, actor, company, event)
                    }
                    StripeEvent::DisputeFunds { .. } => dispute::ingest(tx, actor, company, event),
                    _ => payout::ingest(tx, actor, company, event),
                }
            })
        });
    };
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_collection_ingest", |tx, actor| {
            if let Some(outcome) = known_event(tx.read(), company, event)? {
                return Ok(outcome);
            }
            let attempt = attempt(tx.read(), company, id)?;
            let state = state(tx.read(), id)?;
            if state == CollectionState::Requested {
                return Err(validation(
                    "checkout session must be attached before applying this delivery",
                ));
            }
            let company = Company::bind(tx.read(), company.clone())?;
            let date = event.created().date_naive();
            let open = invoice_amount(&company, &attempt.terms.invoice, date)?;
            let (outcome, payment) = match review_payment(&attempt.terms, &state, open, event) {
                CollectionDecision::ApplyReceipt { payment, amount } => {
                    apply_payment(tx, actor, &company, &attempt, date, (payment, amount))?
                }
                CollectionDecision::MarkFailed => (CollectionOutcome::Failed, None),
                CollectionDecision::AlreadyApplied => (CollectionOutcome::Duplicate, None),
                CollectionDecision::Review(reason) => {
                    (CollectionOutcome::Review(reason.to_string()), None)
                }
                CollectionDecision::Unrelated => return Ok(CollectionOutcome::Unrelated),
            };
            store_event(
                tx,
                actor,
                company.slug(),
                id,
                event,
                (&outcome, payment.as_ref()),
            )?;
            Ok(outcome)
        })
    })
}

fn state(reader: ReadConn<'_>, id: &CollectionId) -> Result<CollectionState, CliError> {
    let Some(link) = linked_checkout(reader, id)? else {
        return Ok(CollectionState::Requested);
    };
    let payment = reader
        .query_row(
            "SELECT payment FROM collection_events WHERE collection=?1 AND status='applied'",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(payment) = payment {
        return Ok(CollectionState::Applied {
            session: link.session,
            payment: payment.parse().map_err(validation)?,
        });
    }
    let failed: bool = reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM collection_events WHERE collection=?1 AND status='failed')",
        [id.to_string()],
        |row| row.get(0),
    )?;
    Ok(if failed {
        CollectionState::Failed(link.session)
    } else {
        CollectionState::Open(link.session)
    })
}

fn apply_payment(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    attempt: &Attempt,
    date: chrono::NaiveDate,
    (payment, amount): (StripePaymentIntentId, PositiveMoney),
) -> Result<(CollectionOutcome, Option<StripePaymentIntentId>), CliError> {
    if let Err(error) = db::periods::ensure_open(tx.read(), company.slug(), date) {
        return if matches!(error, CliError::Validation(_)) {
            Ok((
                CollectionOutcome::Review("receipt date is in a closed period".into()),
                None,
            ))
        } else {
            Err(error)
        };
    }
    let record = find_document(company, &attempt.terms.invoice)?;
    let issued = record
        .issued
        .ok_or_else(|| CliError::Database("collection invoice is no longer issued".into()))?;
    let amount_minor: PositiveMinorUnits = amount.money().amount().try_into()?;
    let entry = |code: &AccountCode, direction| PostEntryParams {
        account_code: code.to_string(),
        direction,
        amount: amount_minor,
        memo: None,
        tax_category: None,
    };
    let entries = [
        entry(&attempt.clearing, DebitOrCredit::Debit),
        entry(&issued.control_account, DebitOrCredit::Credit),
    ];
    let reference = format!("stripe-payment:{payment}");
    let posting = PostTransactionParams {
        company_slug: company.slug(),
        description: "Stripe invoice payment",
        metadata: None,
        currency: amount.money().currency(),
        date,
        entries: &entries,
        correlate: None,
        reference: Some(&reference),
        on_conflict: db::ConflictStrategy::Error,
        actor,
    };
    let result = super::post_in_audited_scope(tx, &posting)?;
    let PostResult::Created(transaction) = result else {
        return Err(CliError::Database(
            "Stripe payment posting was unexpectedly skipped".into(),
        ));
    };
    super::documents::apply_verified_collection(
        tx,
        actor,
        company,
        transaction,
        date,
        vec![AllocateItem {
            document_id: attempt.terms.invoice.clone(),
            amount_minor,
        }],
    )?;
    Ok((CollectionOutcome::Applied(transaction), Some(payment)))
}

fn known_event(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
) -> Result<Option<CollectionOutcome>, CliError> {
    reader
        .query_row(
            "SELECT e.status, e.reason, e.posting FROM collection_events e
             JOIN invoice_collections c ON c.id=e.collection
             WHERE e.account=?1 AND e.event=?2 AND c.company_slug=?3",
            params![event.account().to_string(), event.id().to_string(), company],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<TransactionId>>(2)?,
                ))
            },
        )
        .optional()?
        .map(
            |(state, reason, posting)| match (state.as_str(), reason, posting) {
                ("applied", None, Some(id)) => Ok(CollectionOutcome::Applied(id)),
                ("failed", None, None) => Ok(CollectionOutcome::Failed),
                ("duplicate", None, None) => Ok(CollectionOutcome::Duplicate),
                ("review", Some(reason), None) => Ok(CollectionOutcome::Review(reason)),
                _ => Err(CliError::Database(
                    "invalid recorded collection event".into(),
                )),
            },
        )
        .transpose()
}

fn store_event(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    id: &CollectionId,
    event: &VerifiedStripeEvent,
    (outcome, payment): (&CollectionOutcome, Option<&StripePaymentIntentId>),
) -> Result<(), CliError> {
    let (status, reason, posting) = match outcome {
        CollectionOutcome::Applied(id) => ("applied", None, Some(*id)),
        CollectionOutcome::Failed => ("failed", None, None),
        CollectionOutcome::Duplicate => ("duplicate", None, None),
        CollectionOutcome::Review(reason) => ("review", Some(reason.as_str()), None),
        CollectionOutcome::Unrelated
        | CollectionOutcome::PayoutRecorded(_)
        | CollectionOutcome::RefundRecorded(_)
        | CollectionOutcome::DisputeRecorded(_) => {
            return Err(CliError::Database(
                "cannot store unrelated collection event".into(),
            ));
        }
    };
    let account = event.account().to_string();
    let event_id = event.id().to_string();
    let collection = id.to_string();
    let payment = payment.map(ToString::to_string);
    record(
        tx,
        actor,
        company,
        AuditEntity::CollectionEvent,
        &event_id,
        &json!({
            "account": account, "collection": collection, "status": status,
            "reason": reason, "payment": payment, "posting": posting,
        }),
    )?;
    tx.insert(
        "INSERT INTO collection_events(account,event,collection,status,reason,payment,posting)
        VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            account, event_id, collection, status, reason, payment, posting
        ],
    )?;
    Ok(())
}

fn record(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    entity: AuditEntity,
    id: &str,
    value: &serde_json::Value,
) -> Result<(), CliError> {
    super::record(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity,
            entity_id: id,
            action: AuditAction::Create,
            snapshots: (None, Some(&value.to_string())),
        },
    )
}

pub(crate) fn invoice_currency(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    invoice: &DocumentId,
    date: chrono::NaiveDate,
) -> Result<Currency, CliError> {
    Ok(invoice_amount(&Company::bind(reader, company.clone())?, invoice, date)?.currency())
}

#[derive(serde::Serialize)]
pub(crate) struct CollectionView {
    collection: CollectionId,
    invoice: DocumentId,
    account: String,
    live: bool,
    currency: Currency,
    amount_minor: i128,
    status: &'static str,
    session: Option<String>,
    exceptions: Vec<CollectionException>,
}

#[derive(serde::Serialize)]
pub(crate) struct CollectionException {
    collection: CollectionId,
    event: String,
    reason: String,
}

pub(crate) fn show(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: &CollectionId,
) -> Result<CollectionView, CliError> {
    let attempt = attempt(reader, company, id)?;
    let (status, session) = match state(reader, id)? {
        CollectionState::Requested => ("requested", None),
        CollectionState::Open(session) => ("open", Some(session.to_string())),
        CollectionState::Failed(session) => ("closed_unpaid", Some(session.to_string())),
        CollectionState::Applied { session, .. } => ("applied", Some(session.to_string())),
    };
    let amount = attempt.terms.expected.money();
    Ok(CollectionView {
        collection: id.clone(),
        invoice: attempt.terms.invoice,
        account: attempt.terms.account.to_string(),
        live: attempt.terms.mode.is_live(),
        currency: amount.currency(),
        amount_minor: amount.amount().minor_units(),
        status,
        session,
        exceptions: read_exceptions(reader, company, Some(id))?,
    })
}

pub(crate) fn exceptions(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Vec<CollectionException>, CliError> {
    read_exceptions(reader, company, None)
}

fn read_exceptions(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: Option<&CollectionId>,
) -> Result<Vec<CollectionException>, CliError> {
    let mut statement = reader.prepare_cached(
        "SELECT e.collection,e.event,e.reason FROM collection_events e
         JOIN invoice_collections c ON c.id=e.collection
         WHERE c.company_slug=?1 AND (?2 IS NULL OR c.id=?2) AND e.status='review'
         ORDER BY e.rowid DESC LIMIT 100",
    )?;
    let rows = statement.query_map(params![company, id.map(ToString::to_string)], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
    })?;
    rows.map(|row| {
        let (collection, event, reason) = row?;
        Ok(CollectionException {
            collection: collection.parse().map_err(validation)?,
            event,
            reason,
        })
    })
    .collect()
}

pub(super) fn protects(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    posting: TransactionId,
) -> Result<bool, CliError> {
    Ok(reader.query_row("SELECT EXISTS (
        SELECT 1 FROM collection_events e JOIN invoice_collections c ON c.id=e.collection WHERE c.company_slug=?1 AND e.posting=?2
        UNION ALL SELECT 1 FROM stripe_refund_postings WHERE company_slug=?1 AND posting=?2
        UNION ALL SELECT 1 FROM stripe_payout_postings WHERE company_slug=?1 AND posting=?2
    )", params![company, posting], |row| row.get(0))?)
}

fn url_digest(url: &str) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, url.as_bytes()))
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let broken: bool =
        reader.query_row(include_str!("payment_collection_verify.sql"), [], |row| {
            row.get(0)
        })?;
    if broken {
        return Err(CliError::Database(
            "payment collection differs from the retained audit evidence".into(),
        ));
    }
    let mut links = reader.prepare_cached(
        "SELECT l.url, json_extract(a.after,'$.url_sha256') FROM collection_links l
         JOIN bookkeeping_audit a ON a.entity='collection_link' AND a.entity_id=l.collection",
    )?;
    let rows = links.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (url, digest) = row?;
        if url_digest(&url) != digest {
            return Err(CliError::Database(
                "checkout URL differs from the retained audit evidence".into(),
            ));
        }
    }
    Ok(())
}

fn validation(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}
