//! Authorized payment transitions. The host is always contacted outside a savepoint.

mod accounting;
mod provider;
mod verify;

use std::collections::BTreeSet;

use beankeeper::types::{AccountCode, Currency, Sha256Hex};
use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, params};
use secrecy::SecretString;
use sha2::{Digest, Sha256};

use crate::{
    db::{
        Db,
        accounts::get_account,
        amounts::PositiveMinorUnits,
        companies::{Company, CompanySlug},
        connection::{ReadConn, open_existing_keyed},
        counterparties::{CounterpartyId, find_counterparty},
        disbursements::{
            Disbursement, ProviderEvidence, ProviderStatus, Review, Terms, read, reserved,
        },
        documents::{CapacityFor, DocumentKind, find_document, peak_applied},
        ids::TransactionId,
        schema::DISBURSEMENT_SCHEMA_VERSION,
    },
    disbursement::{
        ApprovalFingerprint, DestinationFingerprint, DisbursementId, TransferEvent, TransferState,
        transition,
    },
    error::CliError,
    identity::Granted,
};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links,
    grant::{DisbursementApprove, DisbursementDispatch, DisbursementDraft},
    savepoint::AuditedTx,
    setup::books::find_profile,
};
use provider::Host;

pub(in crate::db) use verify::verify;

pub(crate) fn draft(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    mut terms: Terms,
) -> Result<Disbursement, CliError> {
    Granted::<DisbursementDraft>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_draft", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            terms.bills.sort_by(|a, b| a.document_id.cmp(&b.document_id));
            let scope = validate_terms(&company, &terms, None)?;
            let record = Disbursement {
                id: DisbursementId::generate(), vendor: scope.vendor, author: signer(actor)?.into(),
                terms, payable_account: scope.payable, currency: scope.currency, amount_minor: scope.total,
                state: TransferState::Draft, review: None, confirmed: None, returned: None, observed: None, rejection: None,
                settlement_posting: None, return_posting: None,
            };
            persist(tx, actor, &company, None, &record)?;
            for bill in &record.terms.bills {
                tx.connection().execute(
                    "INSERT INTO disbursement_bills(company_slug,disbursement_id,document_id,amount_minor)
                     VALUES(?1,?2,?3,?4)",
                    params![slug, record.id, bill.document_id, bill.amount_minor],
                )?;
            }
            Ok(record)
        })
    })
}

pub(crate) fn preview(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
) -> Result<DestinationFingerprint, CliError> {
    let record = Granted::<DisbursementApprove>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_read", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let record = read(&company, id)?;
            require_independent(&record, actor)?;
            Ok(record)
        })
    })?;
    Host::configured()?.destination(&record.vendor)
}

pub(crate) fn approve(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    reviewed_destination: &DestinationFingerprint,
) -> Result<Disbursement, CliError> {
    let destination = preview(db, actor, slug, id)?;
    if &destination != reviewed_destination {
        return Err(invalid("beneficiary changed since it was reviewed"));
    }
    Granted::<DisbursementApprove>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_approve", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let before = read(&company, id)?;
            let signer = require_independent(&before, actor)?;
            let fingerprint = fingerprint(&company, &before, &destination)?;
            let Some(current) = &before.review else {
                validate_terms(&company, &before.terms, Some(&before.id))?;
                let mut after = before.clone();
                after.state = apply(&before.state, TransferEvent::Approve(fingerprint.clone()))?;
                after.review = Some(Review { reviewer: signer.into(), destination, fingerprint });
                persist(tx, actor, &company, Some(&before), &after)?;
                return Ok(after);
            };
            if let TransferState::Approved(approval) = &before.state
                && &fingerprint == approval
                && current.reviewer == signer
            {
                return Ok(before);
            }
            Err(invalid("the disbursement is already reviewed; revoke a pending approval before reviewing again"))
        })
    })
}

pub(crate) fn revoke(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
) -> Result<Disbursement, CliError> {
    decide(db, actor, slug, id, None)
}

pub(crate) fn reject(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    reason: &str,
) -> Result<Disbursement, CliError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 400 || reason.chars().any(char::is_control) {
        return Err(invalid(
            "provide a rejection reason of 1 to 400 printable bytes",
        ));
    }
    decide(db, actor, slug, id, Some(reason))
}

fn decide(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    reason: Option<&str>,
) -> Result<Disbursement, CliError> {
    Granted::<DisbursementApprove>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_revoke", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let before = read(&company, id)?;
            require_independent(&before, actor)?;
            if matches!(before.state, TransferState::Rejected)
                && before.rejection.as_deref() == reason
            {
                return Ok(before);
            }
            if attempt_exists(tx.read(), id)? {
                return Err(invalid(
                    "the instruction may already have been sent; reconcile its provider status",
                ));
            }
            let mut after = before.clone();
            after.state = apply(
                &before.state,
                reason.map_or(TransferEvent::RevokeApproval, |_| TransferEvent::Reject),
            )?;
            after.review = None;
            after.rejection = reason.map(str::to_owned);
            persist(tx, actor, &company, Some(&before), &after)?;
            Ok(after)
        })
    })
}

pub(crate) fn dispatch(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
) -> Result<Disbursement, CliError> {
    let (record, refresh_destination) =
        Granted::<DisbursementDispatch>::with_db(db, actor, |grant| {
            grant.mutate("sp_disbursement_inspect", |tx, _| {
                let company = Company::bind(tx.read(), slug.clone())?;
                let record = read(&company, id)?;
                if record.review.is_none() {
                    return Err(invalid("disbursement requires independent approval"));
                }
                let refresh = matches!(record.state, TransferState::Approved(_))
                    && !attempt_exists(tx.read(), id)?;
                Ok((record, refresh))
            })
        })?;
    let host = Host::configured()?;
    let destination = refresh_destination
        .then(|| host.destination(&record.vendor))
        .transpose()?;
    let (record, submit) = begin_dispatch(db, actor, slug, id, destination.as_ref())?;
    let review = record
        .review
        .as_ref()
        .ok_or_else(|| invalid("approval was revoked"))?;
    let evidence = if submit {
        Some(host.submit(&record, review)?)
    } else {
        host.lookup(id)?
    };
    evidence.map_or(Ok(record), |event| observe(db, actor, slug, id, &event))
}

fn begin_dispatch(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    destination: Option<&DestinationFingerprint>,
) -> Result<(Disbursement, bool), CliError> {
    Granted::<DisbursementDispatch>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_reserve", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let before = read(&company, id)?;
            let review = before.review.as_ref().ok_or_else(|| invalid("approval was revoked"))?;
            let TransferState::Approved(_) = before.state else {
                return Ok((before, false));
            };
            let first = !attempt_exists(tx.read(), id)?;
            if first {
                let current = destination.ok_or_else(|| invalid("retry dispatch to validate the beneficiary"))?;
                if &review.destination != current || fingerprint(&company, &before, current)? != review.fingerprint {
                    return Err(invalid("beneficiary or approved payment terms changed; revoke the approval and review again"));
                }
                validate_terms(&company, &before.terms, Some(&before.id))?;
                tx.connection().execute("INSERT INTO disbursement_attempts(id) VALUES(?1)", [id])?;
            }
            let mut after = before.clone();
            after.state = apply(&before.state, TransferEvent::ReserveSubmission(review.fingerprint.clone()))?;
            persist(tx, actor, &company, Some(&before), &after)?;
            Ok((after, first))
        })
    })
}

fn observe(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    event: &ProviderEvidence,
) -> Result<Disbursement, CliError> {
    Granted::<DisbursementDispatch>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_evidence", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let before = read(&company, id)?;
            let review = before
                .review
                .as_ref()
                .ok_or_else(|| invalid("approval was revoked"))?;
            if event.key != before.id
                || event.currency != before.currency
                || event.amount_minor != before.amount_minor
                || event.destination != review.destination
            {
                return Err(invalid("host evidence does not match the approved payment"));
            }
            if before.observed.as_ref() == Some(event) {
                return Ok(before);
            }
            if before
                .observed
                .as_ref()
                .is_some_and(|previous| event.on < previous.on)
            {
                return Err(invalid(
                    "host event date precedes the previously observed payment status",
                ));
            }
            let mut after = before.clone();
            if event.status == ProviderStatus::Returned && after.confirmed.is_none() {
                let on = event
                    .settled_on
                    .filter(|on| on <= &event.on)
                    .ok_or_else(|| {
                        invalid("return evidence must include the original settlement date")
                    })?;
                let confirmation = ProviderEvidence {
                    status: ProviderStatus::Settled,
                    on,
                    settled_on: None,
                    ..event.clone()
                };
                after.state = apply(&after.state, confirmation.event())?;
                after.confirmed = Some(confirmation);
            }
            after.state = apply(&after.state, event.event())?;
            match event.status {
                ProviderStatus::Settled if after.confirmed.is_none() => {
                    after.confirmed = Some(event.clone());
                }
                ProviderStatus::Returned if after.returned.is_none() => {
                    after.returned = Some(event.clone());
                }
                _ => {}
            }
            after.observed = Some(event.clone());
            persist(tx, actor, &company, Some(&before), &after)?;
            Ok(after)
        })
    })
}

pub(crate) fn reconcile(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    id: &DisbursementId,
    date: NaiveDate,
) -> Result<Disbursement, CliError> {
    Granted::<DisbursementDispatch>::with_db(db, actor, |grant| {
        grant.mutate("sp_disbursement_journal", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            accounting::reconcile(tx, actor, &company, id, date)
        })
    })
}

fn signer(actor: &Actor) -> Result<&str, CliError> {
    actor
        .agent_uri()
        .ok_or_else(|| CliError::Unauthorized("disbursements require a signed identity".into()))
}

fn require_independent<'a>(record: &Disbursement, actor: &'a Actor) -> Result<&'a str, CliError> {
    let signer = signer(actor)?;
    if signer == record.author {
        return Err(CliError::Forbidden(
            "the drafter cannot review this disbursement".into(),
        ));
    }
    Ok(signer)
}

fn fingerprint(
    company: &Company<'_>,
    record: &Disbursement,
    destination: &DestinationFingerprint,
) -> Result<ApprovalFingerprint, CliError> {
    let terms = serde_json::to_vec(&(
        company.slug(),
        &record.id,
        &record.vendor,
        &record.terms,
        &record.payable_account,
        record.currency,
        record.amount_minor,
        destination,
    ))
    .map_err(|err| CliError::General(format!("could not fingerprint payment terms: {err}")))?;
    Ok(ApprovalFingerprint(Sha256Hex::from_digest(
        Sha256::digest(terms).into(),
    )))
}

fn apply(before: &TransferState, event: TransferEvent) -> Result<TransferState, CliError> {
    transition(before, event)
        .map(|effect| effect.state)
        .map_err(|err| invalid(err.to_string()))
}

fn attempt_exists(reader: ReadConn<'_>, id: &DisbursementId) -> Result<bool, CliError> {
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM disbursement_attempts WHERE id=?1)",
        [id],
        |row| row.get(0),
    )?)
}

struct BillScope {
    vendor: CounterpartyId,
    payable: AccountCode,
    currency: Currency,
    total: PositiveMinorUnits,
}

fn validate_terms(
    company: &Company<'_>,
    terms: &Terms,
    reserve_for: Option<&DisbursementId>,
) -> Result<BillScope, CliError> {
    if terms.bills.is_empty() || terms.bills.len() > 100 {
        return Err(invalid("provide one to 100 issued bill allocations"));
    }
    let cash = get_account(company, terms.cash_account.as_str())?;
    if !cash.cash_class.is_cash() {
        return Err(invalid("disbursements require a classified cash account"));
    }
    let profile = find_profile(company.read(), company.slug())?;
    let mut scope: Option<(CounterpartyId, AccountCode, Currency)> = None;
    let mut bill_ids = BTreeSet::new();
    let mut total: i128 = 0;
    for allocation in &terms.bills {
        if !bill_ids.insert(&allocation.document_id) {
            return Err(invalid("each bill can be allocated only once"));
        }
        let bill = find_document(company, &allocation.document_id)?
            .issued
            .filter(|bill| bill.kind == DocumentKind::Bill)
            .ok_or_else(|| invalid("disbursements require issued vendor bills"))?;
        let functional = profile
            .as_ref()
            .map_or(Currency::USD, |book| book.settings.base_currency);
        if bill.currency != functional {
            return Err(invalid(
                "vendor disbursements require the company's functional currency",
            ));
        }
        let group = (bill.counterparty.id, bill.control_account, bill.currency);
        if scope.as_ref().is_some_and(|first| first != &group) {
            return Err(invalid(
                "all bills must use the same vendor, payable account, and currency",
            ));
        }
        scope.get_or_insert(group);
        let already = peak_applied(
            company,
            CapacityFor::Document(&bill.id),
            bill.issue_date.max(Utc::now().date_naive()),
        )?;
        let reserved = if reserve_for.is_some() {
            reserved(company, &bill.id, reserve_for)?
        } else {
            0
        };
        let asked = i128::from(allocation.amount_minor);
        if asked + already + reserved > i128::from(bill.total_minor) {
            return Err(invalid(
                "a payment exceeds an available bill balance after existing reservations",
            ));
        }
        total += asked;
    }
    let (vendor, payable, currency) =
        scope.ok_or_else(|| invalid("at least one bill is required"))?;
    if find_counterparty(company, &vendor)?.is_archived() {
        return Err(invalid("the vendor is archived"));
    }
    let total = i64::try_from(total)
        .map_err(|_| invalid("payment amount exceeds supported minor units"))?
        .try_into()?;
    Ok(BillScope {
        vendor,
        payable,
        currency,
        total,
    })
}

fn persist(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    before: Option<&Disbursement>,
    after: &Disbursement,
) -> Result<(), CliError> {
    let encode = |record: &Disbursement| {
        serde_json::to_string(record)
            .map_err(|err| CliError::General(format!("could not encode payment record: {err}")))
    };
    let previous = before.map(encode).transpose()?;
    let details = encode(after)?;
    let receipt = append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company.slug()),
            entity: AuditEntity::Disbursement,
            entity_id: &after.id.to_string(),
            action: if before.is_some() {
                AuditAction::StatusChange
            } else {
                AuditAction::Create
            },
            snapshots: (previous.as_deref(), Some(&details)),
        },
        AuditLinks::None,
    )?;
    tx.connection().execute(
        "INSERT INTO disbursements(company_slug,id,vendor_id,state,settlement_posting,return_posting,details,receipt)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(company_slug,id) DO UPDATE SET state=excluded.state,
           settlement_posting=excluded.settlement_posting, return_posting=excluded.return_posting,
           details=excluded.details,receipt=excluded.receipt",
        params![company.slug(), after.id, after.vendor, after.status(), after.settlement_posting, after.return_posting, details, receipt],
    )?;
    Ok(())
}

pub(super) fn guard_restore(live: &Connection, incoming: &Connection) -> Result<(), CliError> {
    live.execute(
        "INSERT OR IGNORE INTO disbursement_attempts SELECT id FROM disbursements",
        [],
    )?;
    let mut retained = incoming
        .prepare("SELECT id FROM disbursements UNION SELECT id FROM disbursement_attempts")?;
    let ids = retained.query_map([], |row| row.get::<_, DisbursementId>(0))?;
    let mut insert =
        live.prepare_cached("INSERT OR IGNORE INTO disbursement_attempts(id) VALUES(?1)")?;
    for id in ids {
        insert.execute([id?])?;
    }
    Ok(())
}

pub(crate) fn guard_recovered_file(
    path: &std::path::Path,
    passphrase: Option<&SecretString>,
    version: crate::db::SchemaVersion,
) -> Result<(), CliError> {
    if version >= DISBURSEMENT_SCHEMA_VERSION {
        let conn = open_existing_keyed(path, passphrase)?;
        conn.execute(
            "INSERT OR IGNORE INTO disbursement_attempts SELECT id FROM disbursements",
            [],
        )?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
    }
    Ok(())
}

pub(super) fn guard_posting(reader: ReadConn<'_>, id: TransactionId) -> Result<(), CliError> {
    if reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM disbursements WHERE settlement_posting=?1 OR return_posting=?1)", [id], |row| row.get(0),
    )? {
        return Err(invalid("provider disbursement journals can only be corrected using provider evidence"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}
