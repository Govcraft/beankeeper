//! MCP inputs for collection commands.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{db::companies::CompanySlug, mcp::input_values::Text, payments::stripe::CollectionId};

use crate::request::CalendarDateArg;

use super::{CheckoutArgs, CollectionCommand, DeliveryName, PaymentCollectionRequest};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionCheckout {
    company: Text<CompanySlug>,
    terms: CheckoutArgs,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionDelivery {
    company: Text<CompanySlug>,
    delivery: DeliveryName,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionShow {
    company: Text<CompanySlug>,
    collection: CollectionId,
}

impl From<CollectionCheckout> for PaymentCollectionRequest {
    fn from(input: CollectionCheckout) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::CreateLink(input.terms),
        }
    }
}

impl From<CollectionDelivery> for PaymentCollectionRequest {
    fn from(input: CollectionDelivery) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::Apply {
                delivery: input.delivery,
            },
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionReceiptResolution {
    company: Text<CompanySlug>,
    delivery: DeliveryName,
    booked_on: CalendarDateArg,
}

impl From<CollectionReceiptResolution> for PaymentCollectionRequest {
    fn from(input: CollectionReceiptResolution) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::ResolveReceipt {
                delivery: input.delivery,
                booked_on: input.booked_on,
            },
        }
    }
}

impl From<CollectionShow> for PaymentCollectionRequest {
    fn from(input: CollectionShow) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::Show {
                collection: input.collection,
            },
        }
    }
}

impl PaymentCollectionRequest {
    pub(crate) const fn exceptions(company: CompanySlug) -> Self {
        Self {
            company,
            command: CollectionCommand::Exceptions,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionPayout {
    company: Text<CompanySlug>,
    manifest: super::store::payout::PayoutManifest,
}

impl From<CollectionPayout> for PaymentCollectionRequest {
    fn from(input: CollectionPayout) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::ReconcilePayout {
                manifest: input.manifest,
            },
        }
    }
}

impl PaymentCollectionRequest {
    pub(crate) const fn payouts(company: CompanySlug) -> Self {
        Self {
            company,
            command: CollectionCommand::Payouts,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionRefund {
    company: Text<CompanySlug>,
    terms: super::store::refund::RefundSettlement,
}

impl From<CollectionRefund> for PaymentCollectionRequest {
    fn from(input: CollectionRefund) -> Self {
        Self {
            company: input.company.into_inner(),
            command: CollectionCommand::ReconcileRefund { terms: input.terms },
        }
    }
}

impl PaymentCollectionRequest {
    pub(crate) const fn refunds(company: CompanySlug) -> Self {
        Self {
            company,
            command: CollectionCommand::Refunds,
        }
    }
}

impl PaymentCollectionRequest {
    pub(crate) const fn disputes(company: CompanySlug) -> Self {
        Self {
            company,
            command: CollectionCommand::Disputes,
        }
    }
}
