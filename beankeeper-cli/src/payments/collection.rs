//! Decide how trusted payment evidence affects a frozen collection attempt.

use std::fmt;

use beankeeper::types::{Money, PositiveMoney};

use crate::db::documents::DocumentId;

use super::stripe::{
    CollectionId, StripeAccountId, StripeEvent, StripeMode, StripePaymentIntentId, StripeSessionId,
    VerifiedStripeEvent,
};

/// Immutable terms captured before asking the provider to create a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionTerms {
    /// Stable checkout intent and provider idempotency key.
    pub id: CollectionId,
    /// Invoice that may receive the payment.
    pub invoice: DocumentId,
    /// Provider account expected to receive the payment.
    pub account: StripeAccountId,
    /// Account environment bound to this checkout.
    pub mode: StripeMode,
    /// Amount and currency shown to the customer.
    pub expected: PositiveMoney,
}

/// Current provider state of one collection, with impossible combinations excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionState {
    /// Terms are saved and a hosted Session is in the process of being requested.
    Requested,
    /// Hosted link is ready.
    Open(StripeSessionId),
    /// The provider reported a failed asynchronous attempt.
    Failed(StripeSessionId),
    /// A settled payment was applied outside a managed Checkout session.
    AppliedWithoutSession,
    /// An authenticated payment was applied.
    Applied {
        /// Hosted session used by the payment.
        session: StripeSessionId,
        /// Provider payment already applied to the invoice.
        payment: StripePaymentIntentId,
    },
}

/// What the ledger should do after evaluating a trusted delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionDecision {
    /// Apply a receipt and its document allocation together.
    ApplyReceipt {
        /// Payment identity keyed separately from the webhook delivery ID.
        payment: StripePaymentIntentId,
        /// Gross amount to record against processor clearing.
        amount: PositiveMoney,
    },
    /// Mark an asynchronous attempt as failed with its audit evidence.
    MarkFailed,
    /// The underlying payment has already been applied.
    AlreadyApplied,
    /// Review evidence explicitly before any monetary posting.
    Review(CollectionException),
    /// A different event handler owns this type of evidence.
    Unrelated,
}

/// Precise reason a trusted delivery cannot automatically affect the invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CollectionException {
    /// Event was delivered for another collection or provider account.
    WrongCollection,
    /// The event names a different session or a second payment.
    WrongSession,
    /// The verified payment differs from the frozen amount or currency.
    PaymentMismatch,
    /// The invoice currency or outstanding balance changed.
    InvoiceChanged,
    /// Link creation must be reconciled before applying the event.
    UnattachedSession,
}

impl fmt::Display for CollectionException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongCollection => "payment belongs to a different collection or Stripe account",
            Self::WrongSession => "payment names a different Checkout session or payment",
            Self::PaymentMismatch => {
                "payment amount or currency differs from the frozen collection"
            }
            Self::InvoiceChanged => "the invoice balance or currency has changed",
            Self::UnattachedSession => {
                "retrieve and attach the hosted session before applying payment"
            }
        })
    }
}

impl std::error::Error for CollectionException {}

/// Verify an event against the recorded intent and current invoice balance.
///
/// A closed period, ledger authorization and retry rules must also be checked
/// by the atomic writer that applies this decision.
#[must_use]
pub fn review_payment(
    terms: &CollectionTerms,
    state: &CollectionState,
    outstanding: Money,
    event: &VerifiedStripeEvent,
) -> CollectionDecision {
    let (collection, session, paid) = match event.payload() {
        StripeEvent::CheckoutPaid {
            collection,
            session,
            payment,
            amount,
        } => (collection, session, Some((payment, *amount))),
        StripeEvent::CheckoutFailed {
            collection,
            session,
        } => (collection, session, None),
        _ => return CollectionDecision::Unrelated,
    };
    review_observation(
        terms,
        state,
        outstanding,
        (collection, session, paid),
        (event.account(), event.mode()),
    )
}

pub(crate) fn review_observation(
    terms: &CollectionTerms,
    state: &CollectionState,
    outstanding: Money,
    (collection, session, paid): (
        &CollectionId,
        &StripeSessionId,
        Option<(&StripePaymentIntentId, PositiveMoney)>,
    ),
    (account, mode): (&StripeAccountId, StripeMode),
) -> CollectionDecision {
    let review = CollectionDecision::Review;
    if &terms.id != collection || &terms.account != account || terms.mode != mode {
        return review(CollectionException::WrongCollection);
    }
    match state {
        CollectionState::AppliedWithoutSession => return review(CollectionException::WrongSession),
        CollectionState::Requested => return review(CollectionException::UnattachedSession),
        CollectionState::Open(expected) | CollectionState::Failed(expected) => {
            if expected != session {
                return review(CollectionException::WrongSession);
            }
        }
        CollectionState::Applied {
            session: expected,
            payment: applied,
        } => {
            return if expected == session && paid.is_none_or(|(payment, _)| payment == applied) {
                CollectionDecision::AlreadyApplied
            } else {
                review(CollectionException::WrongSession)
            };
        }
    }
    let Some((payment, amount)) = paid else {
        return CollectionDecision::MarkFailed;
    };
    if amount != terms.expected {
        return review(CollectionException::PaymentMismatch);
    }
    if outstanding.currency() != amount.money().currency()
        || outstanding.amount() < amount.money().amount()
    {
        return review(CollectionException::InvoiceChanged);
    }
    CollectionDecision::ApplyReceipt {
        payment: payment.clone(),
        amount,
    }
}
