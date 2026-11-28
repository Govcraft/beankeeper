//! Authenticate and normalize Stripe Checkout webhooks at the host boundary.

use std::{fmt, str::FromStr};

use beankeeper::types::{Currency, Money, PositiveMoney};
use chrono::{DateTime, Utc};
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use ring::hmac;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

const MAX_EVENT_BYTES: usize = 1_048_576;
const MAX_SIGNATURE_HEADER_BYTES: usize = 4096;
const SIGNATURE_TOLERANCE_SECONDS: u64 = 300;
const MAX_SIGNATURES: usize = 8;

/// A signed event that could not be trusted or understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StripeError {
    /// Delivery exceeded the supported size.
    TooLarge,
    /// The signature is missing, malformed, stale, or invalid.
    Signature,
    /// The verified envelope belongs to a different account or environment.
    WrongEndpoint,
    /// The signed data did not match the supported event schema.
    Payload,
}

impl fmt::Display for StripeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLarge => "Stripe delivery exceeds the supported size",
            Self::Signature => "Stripe webhook signature could not be verified",
            Self::WrongEndpoint => "Stripe event belongs to a different account or environment",
            Self::Payload => "signed Stripe event has invalid or unsupported fields",
        })
    }
}

impl std::error::Error for StripeError {}

macro_rules! stripe_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, schemars::JsonSchema)]
        #[serde(try_from = "String")]
        #[schemars(with = "String")]
        pub struct $name(Box<str>);

        impl FromStr for $name {
            type Err = StripeError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let suffix = value.strip_prefix($prefix).ok_or(StripeError::Payload)?;
                if suffix.is_empty()
                    || suffix.len() > 192
                    || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(StripeError::Payload);
                }
                Ok(Self(value.into()))
            }
        }

        impl TryFrom<String> for $name {
            type Error = StripeError;
            fn try_from(value: String) -> Result<Self, Self::Error> { value.parse() }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

stripe_id!(
    /// Stripe account that owns the Checkout or payout.
    StripeAccountId,
    "acct_"
);
stripe_id!(
    /// Stripe webhook event identity.
    StripeEventId,
    "evt_"
);
stripe_id!(
    /// Stripe hosted Checkout Session identity.
    StripeSessionId,
    "cs_"
);
stripe_id!(
    /// Stripe `PaymentIntent` identity used to link distinct delivery events.
    StripePaymentIntentId,
    "pi_"
);
stripe_id!(
    /// Stripe payout identity from the balance movements report.
    StripePayoutId,
    "po_"
);

stripe_id!(
    /// Stripe refund object ID, independent of webhook retries.
    StripeRefundId,
    "re_"
);

stripe_id!(
    /// Stripe dispute object ID shared by its balance movements.
    StripeDisputeId,
    "dp_"
);

/// Stable identity of one invoice collection request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(try_from = "String")]
#[schemars(with = "String")]
pub struct CollectionId(MagicTypeId);

impl CollectionId {
    pub(crate) fn generate() -> Self {
        Self("collect".create_type_id::<V7>())
    }
}

impl FromStr for CollectionId {
    type Err = StripeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = value
            .parse::<MagicTypeId>()
            .map_err(|_| StripeError::Payload)?;
        if id.prefix_str().as_deref() != Ok("collect") {
            return Err(StripeError::Payload);
        }
        Ok(Self(id))
    }
}

impl TryFrom<String> for CollectionId {
    type Error = StripeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl serde::Serialize for CollectionId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for CollectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Whether a configured endpoint receives test or live events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripeMode {
    /// Stripe sandbox mode.
    Sandbox,
    /// Stripe live mode.
    Live,
}

impl StripeMode {
    pub(crate) const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// The Stripe origin expected by a trusted host.
pub enum StripeEndpoint {
    /// A standard account receives events without the top-level account field.
    Account {
        /// Account bound to the configured signing secret.
        account: StripeAccountId,
        /// Stripe environment bound to that endpoint.
        mode: StripeMode,
    },
    /// Stripe Connect events must identify the configured account explicitly.
    Connect {
        /// Connected account authorized for this ledger.
        account: StripeAccountId,
        /// Stripe environment bound to that endpoint.
        mode: StripeMode,
    },
}

impl StripeEndpoint {
    fn validate(&self, actual: Option<&str>, live: bool) -> Result<StripeAccountId, StripeError> {
        match self {
            Self::Account { account, mode } if mode.is_live() == live && actual.is_none() => {
                Ok(account.clone())
            }
            Self::Connect { account, mode }
                if mode.is_live() == live && actual == Some(&account.0) =>
            {
                Ok(account.clone())
            }
            _ => Err(StripeError::WrongEndpoint),
        }
    }
}

/// A provider movement authenticated against a configured webhook secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedStripeEvent {
    id: StripeEventId,
    account: StripeAccountId,
    mode: StripeMode,
    created: DateTime<Utc>,
    kind: StripeEvent,
    body_sha256: String,
}

impl VerifiedStripeEvent {
    /// Stable provider event ID for idempotent storage.
    #[must_use]
    pub const fn id(&self) -> &StripeEventId {
        &self.id
    }

    /// Provider account authenticated by the endpoint.
    #[must_use]
    pub const fn account(&self) -> &StripeAccountId {
        &self.account
    }

    /// Stripe environment authenticated at delivery.
    #[must_use]
    pub const fn mode(&self) -> StripeMode {
        self.mode
    }

    /// Provider-reported event creation date and time.
    #[must_use]
    pub const fn created(&self) -> DateTime<Utc> {
        self.created
    }

    /// Digest of the exact body authenticated by the provider signature.
    #[must_use]
    pub fn body_sha256(&self) -> &str {
        &self.body_sha256
    }

    /// Authenticated payment or payout evidence.
    #[must_use]
    pub const fn payload(&self) -> &StripeEvent {
        &self.kind
    }
}

/// Direction of a dispute-related principal movement confirmed by Stripe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeMovement {
    /// Funds were withheld from the processor balance.
    Withdrawn,
    /// Funds were returned to the processor balance.
    Reinstated,
}

impl DisputeMovement {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Withdrawn => "withdrawn",
            Self::Reinstated => "reinstated",
        }
    }
}

/// Accounting-relevant information authenticated by a Stripe webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StripeEvent {
    /// A paid Checkout session tied to an existing collection ID.
    CheckoutPaid {
        /// Beankeeper's collection ID stored in `client_reference_id`.
        collection: CollectionId,
        /// Session that was paid.
        session: StripeSessionId,
        /// Stable payment independent of delivery retries.
        payment: StripePaymentIntentId,
        /// Exact gross payment before fees.
        amount: PositiveMoney,
    },
    /// A session definitively expired or ended without a successful payment.
    CheckoutFailed {
        /// Beankeeper's collection ID.
        collection: CollectionId,
        /// Session that failed.
        session: StripeSessionId,
    },
    /// Stripe reports a successful refund tied to a specific payment intent.
    RefundSucceeded {
        /// Stable refund object identity.
        refund: StripeRefundId,
        /// Original payment intent receiving the refund.
        payment: StripePaymentIntentId,
        /// Exact refund before any processor adjustment.
        amount: PositiveMoney,
    },
    /// The processor moved funds against an identified payment in a dispute.
    DisputeFunds {
        /// Provider dispute ID.
        dispute: StripeDisputeId,
        /// Payment at issue.
        payment: StripePaymentIntentId,
        /// Disputed principal, before processor-specific fees or adjustments.
        amount: PositiveMoney,
        /// Whether the processor withdrew or reinstated funds.
        movement: DisputeMovement,
    },
    /// Stripe reports a distributor payout as paid.
    PayoutPaid {
        /// Payout to match against the balance movements and bank statement.
        payout: StripePayoutId,
        /// Exact net payout.
        amount: PositiveMoney,
        /// Stripe's expected bank arrival date.
        arrival: DateTime<Utc>,
    },
}

#[derive(Deserialize)]
struct Envelope {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    account: Option<String>,
    livemode: bool,
    created: i64,
    data: EventData,
}

#[derive(Deserialize)]
struct EventData {
    object: serde_json::Value,
}

#[derive(Deserialize)]
struct Checkout {
    id: String,
    client_reference_id: Option<String>,
    mode: String,
    payment_status: String,
    payment_intent: Option<String>,
    amount_total: Option<i64>,
    currency: Option<String>,
}

#[derive(Deserialize)]
struct Refund {
    id: String,
    payment_intent: Option<String>,
    amount: i64,
    currency: String,
    status: String,
}

#[derive(Deserialize)]
struct Dispute {
    id: String,
    payment_intent: Option<String>,
    amount: i64,
    currency: String,
}

#[derive(Deserialize)]
struct Payout {
    id: String,
    amount: i64,
    currency: String,
    arrival_date: i64,
    status: String,
}

/// Authenticate the raw webhook bytes, then normalize a supported event.
///
/// Unrelated events and completed sessions that are still unpaid return
/// `None`. The returned object can only be constructed after verification.
///
/// # Errors
/// Rejects invalid signatures, event sizes, endpoint mismatches and invalid
/// required fields. Errors never include keys or the raw event body.
pub fn verify_webhook(
    body: &[u8],
    signature_header: &str,
    signing_secret: &SecretString,
    endpoint: &StripeEndpoint,
    now: DateTime<Utc>,
) -> Result<Option<VerifiedStripeEvent>, StripeError> {
    if body.len() > MAX_EVENT_BYTES || signature_header.len() > MAX_SIGNATURE_HEADER_BYTES {
        return Err(StripeError::TooLarge);
    }
    authenticate(body, signature_header, signing_secret, now)?;
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| StripeError::Payload)?;
    let account = endpoint.validate(envelope.account.as_deref(), envelope.livemode)?;
    let Some(kind) = normalize(&envelope.event_type, envelope.data.object)? else {
        return Ok(None);
    };
    Ok(Some(VerifiedStripeEvent {
        id: envelope.id.parse()?,
        account,
        mode: if envelope.livemode {
            StripeMode::Live
        } else {
            StripeMode::Sandbox
        },
        created: timestamp(envelope.created)?,
        kind,
        body_sha256: hex::encode(ring::digest::digest(&ring::digest::SHA256, body).as_ref()),
    }))
}

fn authenticate(
    body: &[u8],
    header: &str,
    secret: &SecretString,
    now: DateTime<Utc>,
) -> Result<(), StripeError> {
    let mut signing_time = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", raw)) if signing_time.is_none() => {
                let seconds: i64 = raw.parse().map_err(|_| StripeError::Signature)?;
                if seconds.to_string() != raw
                    || now.timestamp().abs_diff(seconds) > SIGNATURE_TOLERANCE_SECONDS
                {
                    return Err(StripeError::Signature);
                }
                signing_time = Some(raw);
            }
            Some(("t", _)) => return Err(StripeError::Signature),
            Some(("v1", raw)) => {
                if signatures.len() >= MAX_SIGNATURES {
                    return Err(StripeError::Signature);
                }
                let mut decoded = [0; 32];
                if hex::decode_to_slice(raw, &mut decoded).is_ok() {
                    signatures.push(decoded);
                }
            }
            _ => {}
        }
    }
    let time = signing_time.ok_or(StripeError::Signature)?;
    let mut message = Vec::with_capacity(time.len() + 1 + body.len());
    message.extend_from_slice(time.as_bytes());
    message.push(b'.');
    message.extend_from_slice(body);
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.expose_secret().as_bytes());
    if signatures
        .iter()
        .any(|signature| hmac::verify(&key, &message, signature).is_ok())
    {
        Ok(())
    } else {
        Err(StripeError::Signature)
    }
}

fn normalize(event: &str, object: serde_json::Value) -> Result<Option<StripeEvent>, StripeError> {
    match event {
        "checkout.session.completed"
        | "checkout.session.async_payment_succeeded"
        | "checkout.session.async_payment_failed"
        | "checkout.session.expired" => {
            let checkout: Checkout =
                serde_json::from_value(object).map_err(|_| StripeError::Payload)?;
            if checkout.mode != "payment" {
                return Ok(None);
            }
            checkout_event(event, checkout)
        }
        "refund.created" | "refund.updated" => {
            let refund: Refund =
                serde_json::from_value(object).map_err(|_| StripeError::Payload)?;
            let Some(payment) = refund
                .payment_intent
                .filter(|_| refund.status == "succeeded")
            else {
                return Ok(None);
            };
            Ok(Some(StripeEvent::RefundSucceeded {
                refund: refund.id.parse()?,
                payment: payment.parse()?,
                amount: positive(refund.amount, &refund.currency)?,
            }))
        }
        "charge.dispute.funds_withdrawn" | "charge.dispute.funds_reinstated" => {
            let dispute: Dispute =
                serde_json::from_value(object).map_err(|_| StripeError::Payload)?;
            let Some(payment) = dispute.payment_intent else {
                return Ok(None);
            };
            let movement = if event == "charge.dispute.funds_withdrawn" {
                DisputeMovement::Withdrawn
            } else {
                DisputeMovement::Reinstated
            };
            Ok(Some(StripeEvent::DisputeFunds {
                dispute: dispute.id.parse()?,
                payment: payment.parse()?,
                amount: positive(dispute.amount, &dispute.currency)?,
                movement,
            }))
        }
        "payout.paid" => {
            let payout: Payout =
                serde_json::from_value(object).map_err(|_| StripeError::Payload)?;
            if payout.status != "paid" {
                return Err(StripeError::Payload);
            }
            Ok(Some(StripeEvent::PayoutPaid {
                payout: payout.id.parse()?,
                amount: positive(payout.amount, &payout.currency)?,
                arrival: timestamp(payout.arrival_date)?,
            }))
        }
        _ => Ok(None),
    }
}

fn checkout_event(event: &str, checkout: Checkout) -> Result<Option<StripeEvent>, StripeError> {
    let Some(reference) = checkout.client_reference_id else {
        return Ok(None);
    };
    if !reference.starts_with("collect_") {
        return Ok(None);
    }
    if matches!(
        event,
        "checkout.session.async_payment_failed" | "checkout.session.expired"
    ) {
        return Ok(Some(StripeEvent::CheckoutFailed {
            collection: reference.parse()?,
            session: checkout.id.parse()?,
        }));
    }
    if checkout.payment_status != "paid" {
        return Ok(None);
    }
    Ok(Some(StripeEvent::CheckoutPaid {
        collection: reference.parse()?,
        session: checkout.id.parse()?,
        payment: checkout
            .payment_intent
            .ok_or(StripeError::Payload)?
            .parse()?,
        amount: positive(
            checkout.amount_total.ok_or(StripeError::Payload)?,
            checkout.currency.as_deref().ok_or(StripeError::Payload)?,
        )?,
    }))
}

fn positive(value: i64, code: &str) -> Result<PositiveMoney, StripeError> {
    // Stripe has special wire scales for some ISO currencies. The initial
    // adapter accepts only its supported normal two-decimal book currencies.
    let currency = match code {
        "usd" => Currency::USD,
        "gbp" => Currency::GBP,
        "eur" => Currency::EUR,
        _ => return Err(StripeError::Payload),
    };
    PositiveMoney::new(Money::from_minor(i128::from(value), currency))
        .map_err(|_| StripeError::Payload)
}

fn timestamp(value: i64) -> Result<DateTime<Utc>, StripeError> {
    DateTime::from_timestamp(value, 0).ok_or(StripeError::Payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(secret: &SecretString, body: &[u8], time: i64) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.expose_secret().as_bytes());
        let mut context = hmac::Context::with_key(&key);
        context.update(format!("{time}.").as_bytes());
        context.update(body);
        format!("t={time},v1={}", hex::encode(context.sign().as_ref()))
    }

    #[test]
    fn verified_paid_session_must_match_the_endpoint_and_exact_raw_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = timestamp(1_790_000_000)?;
        let key: SecretString = "fixture-secret-never-a-real-stripe-key".into();
        let account = "acct_example".parse()?;
        let endpoint = StripeEndpoint::Connect {
            account,
            mode: StripeMode::Sandbox,
        };
        let event = serde_json::json!({
            "id":"evt_example", "type":"checkout.session.async_payment_succeeded",
            "account":"acct_example", "livemode":false, "created":now.timestamp(),
            "data":{"object":{
                "id":"cs_test_example", "client_reference_id":"collect".create_type_id::<V7>().to_string(),
                "mode":"payment", "payment_status":"paid", "payment_intent":"pi_example",
                "amount_total":8101, "currency":"usd"
            }}
        });

        let body = serde_json::to_vec(&event)?;
        let header = signed(&key, &body, now.timestamp());
        let verified =
            verify_webhook(&body, &header, &key, &endpoint, now)?.ok_or("paid fixture ignored")?;
        let StripeEvent::CheckoutPaid { amount, .. } = verified.payload() else {
            return Err("unexpected event".into());
        };
        assert_eq!(amount.money(), Money::usd(8101));
        let pretty = serde_json::to_vec_pretty(&event)?;
        assert_eq!(
            verify_webhook(&pretty, &header, &key, &endpoint, now),
            Err(StripeError::Signature)
        );
        let stale = signed(&key, &body, now.timestamp() - 301);
        assert_eq!(
            verify_webhook(&body, &stale, &key, &endpoint, now),
            Err(StripeError::Signature)
        );

        let other = StripeEndpoint::Connect {
            account: "acct_unrelated".parse()?,
            mode: StripeMode::Sandbox,
        };
        assert_eq!(
            verify_webhook(&body, &header, &key, &other, now),
            Err(StripeError::WrongEndpoint)
        );
        Ok(())
    }

    #[test]
    fn signed_checkout_completion_does_not_prove_an_async_payment()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = timestamp(1_790_000_000)?;
        let key: SecretString = "another-fixture-secret".into();
        let endpoint = StripeEndpoint::Account {
            account: "acct_standard".parse()?,
            mode: StripeMode::Sandbox,
        };
        let event = serde_json::json!({
            "id":"evt_async", "type":"checkout.session.completed",
            "livemode":false, "created":now.timestamp(),
            "data":{"object":{
                "id":"cs_test_pending", "client_reference_id":"collect".create_type_id::<V7>().to_string(),
                "mode":"payment", "payment_status":"unpaid",
                "amount_total":300, "currency":"usd"
            }}
        });
        let body = serde_json::to_vec(&event)?;
        let header = signed(&key, &body, now.timestamp());
        assert!(verify_webhook(&body, &header, &key, &endpoint, now)?.is_none());
        Ok(())
    }
}
