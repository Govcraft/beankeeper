//! Preserve signed payout evidence and match the exact gross, adjustments and net.

use std::collections::BTreeSet;

use beankeeper::types::{AccountCode, AccountType, Amount, Currency, DebitOrCredit};
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
        ids::TransactionId,
    },
    error::CliError,
    identity::{Granted, Post},
    payments::stripe::{
        StripePaymentIntentId, StripePayoutId, StripeRefundId, VerifiedStripeEvent,
    },
};

#[derive(Serialize)]
pub struct PayoutView {
    payout: StripePayoutId,
    arrive_on: NaiveDate,
    currency: Currency,
    net_minor: PositiveMinorUnits,
    transaction: Option<TransactionId>,
    fees_minor: Option<i64>,
    gross_minor: Option<i64>,
    refunds_minor: Option<i64>,
}

pub(super) fn ingest(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: &VerifiedStripeEvent,
) -> Result<CollectionOutcome, CliError> {
    let crate::payments::stripe::StripeEvent::PayoutPaid {
        payout,
        amount,
        arrival,
    } = event.payload()
    else {
        return Ok(CollectionOutcome::Unrelated);
    };
    Company::bind(tx.read(), company.clone())?;
    let key = payout.to_string();
    let provider = event.account().to_string();
    let date = arrival.date_naive();
    let currency = amount.money().currency();
    let net: PositiveMinorUnits = amount.money().amount().try_into()?;
    if let Some((old_provider, live, old_date, old_currency, old_net)) = tx.read().query_row(
        "SELECT account,live,arrive_on,currency,net FROM stripe_payouts WHERE company_slug=?1 AND payout=?2",
        params![company, key], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, NaiveDate>(2)?, row.get::<_, Currency>(3)?, row.get::<_, PositiveMinorUnits>(4)?)),
    ).optional()? {
        return if (old_provider, live, old_date, old_currency, old_net) == (provider, event.mode().is_live(), date, currency, net) {
            Ok(CollectionOutcome::Duplicate)
        } else {
            Err(validation("a recorded Stripe payout has different evidence"))
        };
    }
    let delivery = event.id().to_string();
    record(
        tx,
        actor,
        company,
        AuditEntity::StripePayout,
        &key,
        &json!({
            "provider":provider, "live":event.mode().is_live(),"date":date, "currency":currency, "net":net, "delivery":delivery,
        }),
    )?;
    tx.insert("INSERT INTO stripe_payouts(company_slug,payout,account,live,arrive_on,currency,net,delivery)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![company, key, provider, event.mode().is_live(), date, currency, net, delivery])?;
    Ok(CollectionOutcome::PayoutRecorded(payout.clone()))
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayoutManifest {
    pub payout: StripePayoutId,
    /// Payment intents included by Stripe in this payout's balance report.
    pub payments: Vec<StripePaymentIntentId>,
    /// Refund IDs included by Stripe in this payout's balance report.
    #[serde(default)]
    pub refunds: Vec<StripeRefundId>,
    /// Processor fees charged against the included receipts.
    #[schemars(range(min = 0))]
    pub fees_minor: i64,
    #[schemars(with = "String")]
    pub bank_account: AccountCode,
    #[schemars(with = "String")]
    pub fee_account: AccountCode,
}

#[derive(Serialize)]
pub struct PayoutPosting {
    transaction: TransactionId,
    existing: bool,
}

pub(crate) fn reconcile(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    manifest: &PayoutManifest,
) -> Result<PayoutPosting, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_stripe_payout_reconcile", |tx, actor| {
            reconcile_in_scope(tx, actor, company, manifest)
        })
    })
}

fn reconcile_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    manifest: &PayoutManifest,
) -> Result<PayoutPosting, CliError> {
    let scope = Company::bind(tx.read(), company.clone())?;
    let key = manifest.payout.to_string();
    let selection = Selection::try_from(manifest)?;
    let terms = selection.snapshot(manifest);
    let encoded_terms = terms.to_string();
    let evidence = payout_evidence(tx.read(), company, &manifest.payout)?;
    if let Some((transaction, snapshot)) = tx.read().query_row(
        "SELECT posting,manifest FROM stripe_payout_postings WHERE company_slug=?1 AND payout=?2",
        params![company, key], |row| Ok((row.get::<_, TransactionId>(0)?, row.get::<_, String>(1)?)),
    ).optional()? {
        if snapshot == encoded_terms {
            return Ok(PayoutPosting { transaction, existing: true });
        }
        return Err(validation("payout is already reconciled using different terms"));
    }
    let (gross, clearing) = group_receipts(tx.read(), company, &evidence, &selection.payments)?;
    let refunded = group_refunds(tx.read(), company, &evidence, &clearing, &selection.refunds)?;
    let net = gross
        .checked_sub(refunded)
        .and_then(|total| total.checked_sub(Amount::from(manifest.fees_minor)))
        .ok_or_else(|| validation("payout totals exceed supported range"))?;
    if net != Amount::from(evidence.net) {
        return Err(validation(
            "Stripe payout does not equal the selected gross receipts less fees and refunds",
        ));
    }
    validate_account(&scope, &manifest.bank_account, AccountType::Asset)?;
    validate_account(&scope, &manifest.fee_account, AccountType::Expense)?;
    if clearing == manifest.bank_account.as_str() {
        return Err(validation(
            "the bank deposit and processor clearing accounts must differ",
        ));
    }
    let entries = payout_entries(
        manifest,
        &evidence,
        gross
            .checked_sub(refunded)
            .ok_or_else(|| validation("payout refunds exceed gross receipts"))?,
        clearing,
    )?;
    let reference = format!("stripe-payout:{key}");
    let posting = PostTransactionParams {
        company_slug: company,
        description: "Stripe payout and processor fees",
        metadata: None,
        currency: evidence.currency,
        date: evidence.date,
        entries: &entries,
        correlate: None,
        reference: Some(&reference),
        on_conflict: db::ConflictStrategy::Error,
        actor,
    };
    let PostResult::Created(transaction) = super::super::post_in_audited_scope(tx, &posting)?
    else {
        return Err(CliError::Database(
            "Stripe payout was unexpectedly skipped".into(),
        ));
    };
    record(
        tx,
        actor,
        company,
        AuditEntity::StripePayoutPosting,
        &key,
        &json!({"posting":transaction, "gross": gross.minor_units(), "refunds":refunded.minor_units(), "terms": terms}),
    )?;
    tx.insert(
        "INSERT INTO stripe_payout_postings(company_slug,payout,posting,gross,fees,refunds,manifest)
        VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            company,
            key,
            transaction,
            PositiveMinorUnits::try_from(gross)?,
            manifest.fees_minor,
            i64::try_from(refunded.minor_units()).map_err(validation)?,
            encoded_terms
        ],
    )?;
    for payment in selection.payments {
        tx.insert(
            "INSERT INTO stripe_payout_payments(company_slug,payout,payment) VALUES(?1,?2,?3)",
            params![company, key, payment],
        )?;
    }
    for refund in selection.refunds {
        tx.insert(
            "INSERT INTO stripe_payout_refunds(company_slug,payout,refund) VALUES(?1,?2,?3)",
            params![company, key, refund],
        )?;
    }
    Ok(PayoutPosting {
        transaction,
        existing: false,
    })
}

struct Selection {
    payments: BTreeSet<String>,
    refunds: BTreeSet<String>,
}

impl TryFrom<&PayoutManifest> for Selection {
    type Error = CliError;

    fn try_from(manifest: &PayoutManifest) -> Result<Self, Self::Error> {
        let payments = manifest
            .payments
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        if payments.is_empty()
            || payments.len() > 1000
            || payments.len() != manifest.payments.len()
            || manifest.fees_minor < 0
        {
            return Err(validation(
                "payout requires 1 to 1,000 distinct payments and nonnegative fees",
            ));
        }
        let refunds = manifest
            .refunds
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        if refunds.len() > 1000 || refunds.len() != manifest.refunds.len() {
            return Err(validation(
                "payout accepts at most 1,000 distinct refund IDs",
            ));
        }
        Ok(Self { payments, refunds })
    }
}

impl Selection {
    fn snapshot(&self, manifest: &PayoutManifest) -> serde_json::Value {
        json!({"payments":self.payments, "refunds":self.refunds, "fees":manifest.fees_minor,
          "bank_account":manifest.bank_account, "fee_account":manifest.fee_account})
    }
}

fn validate_account(
    scope: &Company<'_>,
    code: &AccountCode,
    kind: AccountType,
) -> Result<(), CliError> {
    if db::accounts::get_active_account(scope, code.as_str())?.account_type != kind {
        return Err(validation(format!(
            "payout account {code} must be a {kind} account"
        )));
    }
    Ok(())
}

fn payout_entries(
    manifest: &PayoutManifest,
    evidence: &Evidence,
    gross: Amount,
    clearing: String,
) -> Result<Vec<PostEntryParams>, CliError> {
    let entry = |account_code, direction, amount| PostEntryParams {
        account_code,
        direction,
        amount,
        memo: None,
        tax_category: None,
    };
    let mut entries = vec![
        entry(
            manifest.bank_account.to_string(),
            DebitOrCredit::Debit,
            evidence.net,
        ),
        entry(clearing, DebitOrCredit::Credit, gross.try_into()?),
    ];
    if manifest.fees_minor > 0 {
        entries.push(entry(
            manifest.fee_account.to_string(),
            DebitOrCredit::Debit,
            PositiveMinorUnits::try_from(Amount::from(manifest.fees_minor))?,
        ));
    }
    Ok(entries)
}

struct Evidence {
    account: String,
    live: bool,
    date: NaiveDate,
    currency: Currency,
    net: PositiveMinorUnits,
}

fn payout_evidence(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    payout: &StripePayoutId,
) -> Result<Evidence, CliError> {
    reader.query_row("SELECT account,live,arrive_on,currency,net FROM stripe_payouts WHERE company_slug=?1 AND payout=?2",
        params![company, payout.to_string()], |row| Ok(Evidence { account:row.get(0)?, live:row.get(1)?, date:row.get(2)?, currency:row.get(3)?, net:row.get(4)? }))
        .optional()?.ok_or_else(|| CliError::NotFound("no signed evidence has been recorded for this Stripe payout".into()))
}

fn group_receipts(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    evidence: &Evidence,
    payments: &BTreeSet<String>,
) -> Result<(Amount, String), CliError> {
    let mut total = Amount::ZERO;
    let mut clearing = None;
    let mut read = reader.prepare_cached("SELECT c.clearing_account,c.amount FROM collection_events e
      JOIN invoice_collections c ON c.id=e.collection JOIN transactions t ON t.id=e.posting
      WHERE c.company_slug=?1 AND c.account=?2 AND c.live=?3 AND c.currency=?4 AND e.payment=?5
        AND t.date <= ?6 AND NOT EXISTS(SELECT 1 FROM stripe_payout_payments used WHERE used.payment=e.payment)")?;
    for payment in payments {
        let (account, amount) = read.query_row(params![company, evidence.account, evidence.live, evidence.currency, payment, evidence.date],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, PositiveMinorUnits>(1)?))).optional()?
            .ok_or_else(|| validation(format!("payment {payment} is missing, already reconciled or belongs to a different processor scope")))?;
        if clearing.as_ref().is_some_and(|known| *known != account) {
            return Err(validation(
                "selected receipts use different clearing accounts",
            ));
        }
        clearing = Some(account);
        total = total
            .checked_add(amount.into())
            .ok_or_else(|| validation("payout gross total exceeds supported range"))?;
    }
    Ok((
        total,
        clearing.ok_or_else(|| validation("payout requires receipts"))?,
    ))
}

fn group_refunds(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    evidence: &Evidence,
    clearing: &str,
    refunds: &BTreeSet<String>,
) -> Result<Amount, CliError> {
    let mut read = reader.prepare_cached(
        "SELECT r.amount FROM stripe_refund_postings r
        JOIN stripe_refunds f USING(company_slug,refund)
        JOIN collection_events e ON e.posting=r.original_payment
        JOIN invoice_collections c ON c.id=e.collection
        WHERE r.company_slug=?1 AND f.account=?2 AND f.live=?3 AND f.currency=?4
        AND r.refund=?5 AND f.occurred<=?6 AND c.clearing_account=?7
        AND NOT EXISTS(SELECT 1 FROM stripe_payout_refunds m WHERE m.refund=r.refund)",
    )?;
    refunds.iter().try_fold(Amount::ZERO, |total, refund| {
        let amount: PositiveMinorUnits = read
            .query_row(
                params![
                    company,
                    evidence.account,
                    evidence.live,
                    evidence.currency,
                    refund,
                    evidence.date,
                    clearing
                ],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                validation(format!(
                    "refund {refund} is missing, already reconciled, or out of scope"
                ))
            })?;
        total
            .checked_add(amount.into())
            .ok_or_else(|| validation("payout refunds exceed supported range"))
    })
}

pub(crate) fn report(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Vec<PayoutView>, CliError> {
    let mut statement = reader.prepare_cached("SELECT p.payout,p.arrive_on,p.currency,p.net,r.posting,r.fees,r.gross,r.refunds
        FROM stripe_payouts p LEFT JOIN stripe_payout_postings r ON p.company_slug=r.company_slug AND p.payout=r.payout
        WHERE p.company_slug=?1 ORDER BY p.arrive_on DESC,p.payout LIMIT 100")?;
    let rows = statement.query_map([company], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        ))
    })?;
    rows.map(|row| {
        let (
            key,
            arrive_on,
            currency,
            net_minor,
            transaction,
            fees_minor,
            gross_minor,
            refunds_minor,
        ) = row?;
        Ok(PayoutView {
            payout: key.parse().map_err(validation)?,
            arrive_on,
            currency,
            net_minor,
            transaction,
            fees_minor,
            gross_minor,
            refunds_minor,
        })
    })
    .collect()
}
