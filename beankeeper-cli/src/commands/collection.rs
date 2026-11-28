//! Shared CLI and MCP requests for invoice collection.

mod host;
pub(crate) mod wire;

use std::{fmt, str::FromStr};

use beankeeper::types::{AccountCode, Money, PositiveMoney};
use clap::{Args, Subcommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use self::host::StripeHost;
use super::reference::TransactionReference;
use crate::{
    cli::{Cli, OutputFormat, require_company, resolve_format},
    db::{
        Actor, Db,
        amounts::PositiveMinorUnits,
        audit::payment_collection::{self as store, CollectionOutcome, CollectionRequest},
        companies::CompanySlug,
        documents::DocumentId,
    },
    error::CliError,
    operation::Operation,
    output::json,
    payments::stripe::CollectionId,
    request::{CalendarDateArg, LedgerRequest},
};

/// Commands for one Stripe invoice collection workflow.
#[derive(Clone, Debug, Subcommand)]
pub enum CollectionCommand {
    /// Create a hosted checkout for a persisted invoice and request key.
    CreateLink(CheckoutArgs),
    /// Authenticate and apply one delivery from the private Stripe spool.
    Apply { delivery: DeliveryName },
    /// Inspect a checkout and recorded delivery outcomes.
    Show { collection: CollectionId },
    /// List recent collection exceptions requiring review.
    Exceptions,
    /// Post a reconciled deposit and its fees against a verified payout.
    ReconcilePayout {
        /// Selected payment IDs, explicit fees, and posting accounts as JSON.
        #[arg(long, value_parser = parse_payout)]
        manifest: store::payout::PayoutManifest,
    },
    /// Reopen the receivable against a verified refund on an open accounting date.
    ReconcileRefund {
        /// Refund reference and booking date as JSON.
        #[arg(long, value_parser = parse_refund)]
        terms: store::refund::RefundSettlement,
    },
    /// List dispute balance movements and the original receipt, when matched.
    Disputes,
    /// List verified refunds and their journal links.
    Refunds,
    /// List verified Stripe payouts and their reconciliation status.
    Payouts,
}

/// Terms fixed at checkout creation.
#[derive(Clone, Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutArgs {
    /// Issued invoice to collect.
    #[arg(long)]
    invoice: DocumentId,
    /// Gross amount in the invoice currency's minor units.
    #[arg(long)]
    #[schemars(with = "i64", range(min = 1))]
    amount_minor: PositiveMinorUnits,
    /// A stable caller-issued key for this exact request.
    #[arg(long)]
    #[schemars(with = "String")]
    request_key: TransactionReference,
    /// Asset to receive the gross payment.
    #[arg(long)]
    #[schemars(with = "String")]
    clearing_account: AccountCode,
    /// Evaluate the open balance on this date.
    #[arg(long)]
    as_of: CalendarDateArg,
}

/// Name of two Stripe spool artifacts, `name.json` and `name.sig`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(with = "String")]
pub struct DeliveryName(String);

impl FromStr for DeliveryName {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 80
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(CliError::Validation(
                "delivery name must contain 1 to 80 letters, digits, hyphens or underscores".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for DeliveryName {
    type Error = CliError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl CollectionCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::CreateLink(_) => Operation::CollectionCreateLink,
            Self::Apply { .. } => Operation::CollectionApply,
            Self::Show { .. } => Operation::CollectionShow,
            Self::Exceptions => Operation::CollectionExceptions,
            Self::ReconcilePayout { .. } => Operation::CollectionReconcilePayout,
            Self::Payouts => Operation::CollectionPayouts,
            Self::ReconcileRefund { .. } => Operation::CollectionReconcileRefund,
            Self::Refunds => Operation::CollectionRefunds,
            Self::Disputes => Operation::CollectionDisputes,
        }
    }
}

/// Typed target for an invoice collection operation.
pub struct PaymentCollectionRequest {
    company: CompanySlug,
    command: CollectionCommand,
}

impl PaymentCollectionRequest {
    pub(crate) const fn operation(&self) -> Operation {
        self.command.operation()
    }

    pub(crate) const fn calls_provider(&self) -> bool {
        matches!(self.command, CollectionCommand::CreateLink(_))
    }

    pub(crate) fn execute(
        self,
        db: &Db,
        actor: &Actor,
    ) -> Result<PaymentCollectionResponse, CliError> {
        let operation = self.operation();
        let data = match self.command {
            CollectionCommand::CreateLink(args) => create(db, actor, &self.company, args)?,
            CollectionCommand::Apply { delivery } => {
                let host = StripeHost::for_company(&self.company)?;
                let outcome = match host.delivery(&delivery)? {
                    Some(event) => store::ingest_checkout(db, actor, &self.company, &event)?,
                    None => CollectionOutcome::Unrelated,
                };
                CollectionData::Apply(outcome)
            }
            CollectionCommand::Show { collection } => {
                CollectionData::Show(store::show(db.read(), &self.company, &collection)?)
            }
            CollectionCommand::ReconcilePayout { manifest } => CollectionData::PayoutPosting(
                store::payout::reconcile(db, actor, &self.company, &manifest)?,
            ),
            CollectionCommand::ReconcileRefund { terms } => CollectionData::RefundPosting(
                store::refund::reconcile(db, actor, &self.company, &terms)?,
            ),
            CollectionCommand::Disputes => {
                CollectionData::Disputes(store::dispute::report(db.read(), &self.company)?)
            }
            CollectionCommand::Refunds => {
                CollectionData::Refunds(store::refund::report(db.read(), &self.company)?)
            }
            CollectionCommand::Payouts => {
                CollectionData::Payouts(store::payout::report(db.read(), &self.company)?)
            }
            CollectionCommand::Exceptions => {
                CollectionData::Exceptions(store::exceptions(db.read(), &self.company)?)
            }
        };
        Ok(PaymentCollectionResponse {
            operation,
            company: self.company,
            data,
        })
    }
}

fn create(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    args: CheckoutArgs,
) -> Result<CollectionData, CliError> {
    let host = StripeHost::for_company(company)?;
    let currency = store::invoice_currency(db.read(), company, &args.invoice, args.as_of.into())?;
    let request = CollectionRequest {
        company: company.clone(),
        invoice: args.invoice,
        account: host.account.clone(),
        mode: host.mode,
        amount: PositiveMoney::new(Money::new(args.amount_minor.into(), currency))
            .map_err(|error| CliError::Validation(error.to_string()))?,
        request_key: args.request_key,
        clearing_account: args.clearing_account,
        as_of: args.as_of.into(),
    };
    let link = store::create_link(db, actor, &request, &host.checkout()?)?;
    Ok(CollectionData::Checkout {
        collection: link.collection,
        session: link.checkout.session.to_string(),
        url: link.checkout.url.to_string(),
    })
}

#[derive(Serialize)]
#[serde(untagged)]
enum CollectionData {
    Checkout {
        collection: CollectionId,
        session: String,
        url: String,
    },
    Apply(CollectionOutcome),
    Show(store::CollectionView),
    Exceptions(Vec<store::CollectionException>),
    PayoutPosting(store::payout::PayoutPosting),
    Payouts(Vec<store::payout::PayoutView>),
    RefundPosting(store::refund::RefundPosting),
    Refunds(Vec<store::refund::RefundView>),
    Disputes(Vec<store::dispute::DisputeView>),
}

/// Sanitized collection results and provider references.
#[derive(Serialize)]
#[serde(transparent)]
pub struct PaymentCollectionResponse {
    #[serde(skip)]
    operation: Operation,
    #[serde(skip)]
    company: CompanySlug,
    data: CollectionData,
}

impl PaymentCollectionResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        if resolve_format(None, cli) == OutputFormat::Json {
            println!(
                "{}",
                json::envelope_ok(
                    cli.meta(self.operation, Some(self.company.as_str())),
                    &self.data
                )?
            );
        } else if !cli.verbosity.quiet {
            let text = serde_json::to_string_pretty(&self.data)
                .map_err(|error| CliError::General(error.to_string()))?;
            println!("{text}");
        }
        Ok(())
    }
}

impl fmt::Display for DeliveryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub(crate) fn ledger_request(
    cli: &Cli,
    command: &CollectionCommand,
) -> Result<LedgerRequest, CliError> {
    Ok(LedgerRequest::PaymentCollection(PaymentCollectionRequest {
        company: require_company(cli)?.clone(),
        command: command.clone(),
    }))
}

fn parse_payout(raw: &str) -> Result<store::payout::PayoutManifest, CliError> {
    serde_json::from_str(raw)
        .map_err(|error| CliError::Validation(format!("invalid payout manifest: {error}")))
}

fn parse_refund(raw: &str) -> Result<store::refund::RefundSettlement, CliError> {
    serde_json::from_str(raw)
        .map_err(|error| CliError::Validation(format!("invalid refund terms: {error}")))
}
