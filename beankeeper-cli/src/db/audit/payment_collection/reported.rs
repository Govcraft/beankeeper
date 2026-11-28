//! Stripe facts supplied by an agent and comparison with authenticated deliveries.

use beankeeper::types::{AccountCode, Currency, Money, PositiveMoney};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Actor, AuditEntity, CollectionOutcome, ReadConn, record, validation};
use crate::{
    db::{
        amounts::PositiveMinorUnits,
        audit::savepoint::AuditedTx,
        companies::{Company, CompanySlug},
        documents::DocumentId,
        ids::TransactionId,
    },
    error::CliError,
    payments::stripe::{
        CollectionId, DisputeMovement, StripeAccountId, StripeDisputeId, StripeMode,
        StripePaymentIntentId, StripePayoutId, StripeRefundId, StripeSessionId,
        VerifiedStripeEvent,
    },
    request::CalendarDateArg,
};

/// A Stripe object read by the agent, with the accounting scope stated explicitly.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum ReportedEvidence {
    /// A succeeded payment allocated to a Beankeeper invoice.
    Payment {
        facts: Facts,
        payment: StripePaymentIntentId,
        target: PaymentTarget,
    },
    /// A paid payout. Supply its balance-report fees before selecting movements.
    Payout {
        facts: Facts,
        payout: StripePayoutId,
        #[schemars(range(min = 0))]
        fees_minor: i64,
    },
    /// A succeeded refund against a known payment.
    Refund {
        facts: Facts,
        refund: StripeRefundId,
        payment: StripePaymentIntentId,
    },
    /// One actual principal movement from a dispute's balance transactions.
    Dispute {
        facts: Facts,
        dispute: StripeDisputeId,
        payment: StripePaymentIntentId,
        movement: DisputeMovement,
    },
}

/// The immutable facts normalized from Stripe's reads or authenticated deliveries.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    /// API account owning the object.
    pub account: StripeAccountId,
    /// True for live data, false for Stripe sandbox.
    pub live: bool,
    /// Date of payment, refund, dispute movement, or bank arrival for payouts.
    pub date: CalendarDateArg,
    /// ISO currency of this movement.
    #[schemars(with = "String")]
    pub currency: Currency,
    /// Positive amount in minor units before fees, or net amount for payouts.
    #[schemars(with = "i64", range(min = 1))]
    pub amount_minor: PositiveMinorUnits,
}

impl Facts {
    pub(super) fn signed(
        event: &VerifiedStripeEvent,
        date: NaiveDate,
        amount: PositiveMoney,
    ) -> Result<Self, CliError> {
        Ok(Self {
            account: event.account().clone(),
            live: event.mode().is_live(),
            date: date.into(),
            currency: amount.money().currency(),
            amount_minor: amount.money().amount().try_into()?,
        })
    }

    pub(super) fn amount(&self) -> Result<PositiveMoney, CliError> {
        PositiveMoney::new(Money::new(self.amount_minor.into(), self.currency)).map_err(validation)
    }

    pub(super) const fn mode(&self) -> StripeMode {
        if self.live {
            StripeMode::Live
        } else {
            StripeMode::Sandbox
        }
    }
}

/// The explicit owner of the payment in the current set of books.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum PaymentTarget {
    /// A Checkout session already associated with frozen Beankeeper terms.
    Checkout {
        collection: CollectionId,
        session: StripeSessionId,
    },
    /// A payment collected outside a Beankeeper-managed Checkout session.
    Invoice {
        invoice: DocumentId,
        #[schemars(with = "String")]
        clearing_account: AccountCode,
    },
}

impl ReportedEvidence {
    pub(in crate::db::audit) fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
    ) -> Result<CollectionOutcome, CliError> {
        let company = scope.slug();
        match self {
            Self::Payment {
                facts,
                payment,
                target,
            } => super::ingest_reported_payment(tx, actor, scope, &facts, &payment, &target),
            Self::Payout {
                facts,
                payout,
                fees_minor,
            } => {
                if fees_minor < 0 {
                    return Err(validation("reported Stripe fees must be nonnegative"));
                }
                let origin = Origin::Agent {
                    fees_minor: Some(fees_minor),
                };
                super::payout::record_evidence(tx, actor, company, &payout, &facts, origin)
            }
            Self::Refund {
                facts,
                refund,
                payment,
            } => super::refund::record_evidence(
                tx,
                actor,
                company,
                (&refund, &payment, &facts),
                Origin::Agent { fees_minor: None },
            ),
            Self::Dispute {
                facts,
                dispute,
                payment,
                movement,
            } => super::dispute::record_evidence(
                tx,
                actor,
                company,
                (&dispute, &payment, movement, &facts),
                Origin::Agent { fees_minor: None },
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Source {
    Signed,
    AgentReported,
}

impl Source {
    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::AgentReported => "agent_reported",
        }
    }

    pub(super) fn from_row(raw: &str) -> Result<Self, CliError> {
        match raw {
            "signed" => Ok(Self::Signed),
            "agent_reported" => Ok(Self::AgentReported),
            _ => Err(CliError::Database("invalid Stripe evidence source".into())),
        }
    }

    pub(super) fn metadata(self) -> String {
        json!({"stripe_evidence_source":self}).to_string()
    }

    pub(super) fn effective(
        self,
        reader: ReadConn<'_>,
        company: &CompanySlug,
        key: &str,
    ) -> Result<Self, CliError> {
        if self == Self::Signed {
            return Ok(self);
        }
        let confirmed: bool = reader.query_row(
            "SELECT EXISTS(SELECT 1 FROM stripe_evidence_reviews
             WHERE company_slug=?1 AND evidence_key=?2 AND agrees=1)",
            params![company, key],
            |r| r.get(0),
        )?;
        Ok(if confirmed { Self::Signed } else { self })
    }
}

#[derive(Clone, Copy)]
pub(super) enum Origin<'a> {
    Signed(&'a VerifiedStripeEvent),
    Agent { fees_minor: Option<i64> },
}

impl Origin<'_> {
    pub(super) const fn source(self) -> Source {
        match self {
            Self::Signed(_) => Source::Signed,
            Self::Agent { .. } => Source::AgentReported,
        }
    }

    pub(super) fn delivery(self, key: &str) -> String {
        match self {
            Self::Signed(event) => event.id().to_string(),
            Self::Agent { .. } => format!("agent:{key}"),
        }
    }

    pub(super) const fn fees(self) -> Option<i64> {
        match self {
            Self::Signed(_) => None,
            Self::Agent { fees_minor } => fees_minor,
        }
    }
}

pub(super) fn compare(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    key: &str,
    (recorded, origin, agrees): (Source, Origin<'_>, bool),
) -> Result<CollectionOutcome, CliError> {
    match (recorded, origin, agrees) {
        (Source::AgentReported, Origin::Signed(event), agrees) => {
            record_signed_review(tx, actor, company, key, event, agrees)?;
            Ok(if agrees {
                CollectionOutcome::Duplicate
            } else {
                CollectionOutcome::Review(
                    "signed Stripe delivery conflicts with agent-reported evidence".into(),
                )
            })
        }
        (_, _, true) => Ok(CollectionOutcome::Duplicate),
        _ => Err(validation("Stripe evidence conflicts with recorded terms")),
    }
}

fn record_signed_review(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    key: &str,
    event: &VerifiedStripeEvent,
    agrees: bool,
) -> Result<(), CliError> {
    let event_id = event.id().to_string();
    let digest = event.body_sha256();
    if let Some(existing) = tx
        .read()
        .query_row(
            "SELECT body_sha256 FROM stripe_evidence_reviews
         WHERE company_slug=?1 AND evidence_key=?2 AND delivery=?3",
            params![company, key, event_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return if existing == digest {
            Ok(())
        } else {
            Err(validation(
                "authenticated delivery differs from the recorded Stripe event",
            ))
        };
    }
    record(
        tx,
        actor,
        company,
        AuditEntity::StripeEvidenceReview,
        &event_id,
        &json!({"evidence_key":key, "body_sha256":digest, "agrees":agrees, "source":"signed"}),
    )?;
    tx.insert(
        "INSERT INTO stripe_evidence_reviews(company_slug,evidence_key,delivery,body_sha256,agrees)
         VALUES(?1,?2,?3,?4,?5)",
        params![company, key, event_id, digest, agrees],
    )?;
    Ok(())
}

#[derive(Serialize)]
pub(super) struct PaymentView {
    pub payment: StripePaymentIntentId,
    pub posting: TransactionId,
    pub source: Source,
    pub original_source: Source,
}

pub(super) fn ensure_no_conflict(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    key: &str,
) -> Result<(), CliError> {
    let conflict: bool = reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM stripe_evidence_reviews
         WHERE company_slug=?1 AND evidence_key=?2 AND agrees=0)",
        params![company, key],
        |row| row.get(0),
    )?;
    if conflict {
        return Err(validation(
            "Stripe evidence has a signed conflict requiring review",
        ));
    }
    Ok(())
}
