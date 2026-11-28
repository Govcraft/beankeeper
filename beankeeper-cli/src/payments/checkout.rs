//! Stripe-hosted Checkout is created from a previously persisted intent.

use std::{fmt, io::Read, time::Duration};

use beankeeper::types::Currency;
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::{
    collection::CollectionTerms,
    stripe::{StripeAccountId, StripeMode, StripeSessionId},
};

const CHECKOUT_API: &str = "https://api.stripe.com/v1/checkout/sessions";
const STRIPE_VERSION: &str = "2026-11-18.endive";
const MAX_RESPONSE_BYTES: u64 = 1_048_576;

/// Return destination bound to the host's payment flow.
#[derive(Debug, Clone)]
pub struct CheckoutReturnUrl(Url);

impl std::str::FromStr for CheckoutReturnUrl {
    type Err = CheckoutError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_| CheckoutError::BadReturnUrl)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(CheckoutError::BadReturnUrl);
        }
        Ok(Self(url))
    }
}

/// Configuration from a trusted host. Secrets are not accepted through a ledger tool.
pub struct CheckoutConfig {
    /// Stripe restricted API key.
    pub restricted_key: SecretString,
    /// Account owning all checkout requests on this client.
    pub account: StripeAccountId,
    /// Stripe sandbox or live mode.
    pub mode: StripeMode,
    /// Set true when the account is a connected account.
    pub connected_account: bool,
    /// Integration identifier registered for these requests.
    pub integration_identifier: String,
    /// Fixed redirect selected by the host.
    pub return_url: CheckoutReturnUrl,
}

/// An attached link supplied by Stripe.
#[derive(Debug, Clone)]
pub struct CheckoutLink {
    /// Stable provider session that a later paid event must name.
    pub session: StripeSessionId,
    /// Hosted payment page.
    pub url: Url,
}

/// Hosted payment provider boundary.
pub trait CheckoutProvider {
    /// Request a hosted link. Retries must use the same persisted terms.
    ///
    /// # Errors
    /// Returns failure if request terms are invalid, or the provider failed or
    /// supplied inconsistent data.
    fn create(&self, terms: &CollectionTerms) -> Result<CheckoutLink, CheckoutError>;
}

/// A checkout link could not be created or proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckoutError {
    /// Redirect must be an absolute HTTPS URL without user information.
    BadReturnUrl,
    /// Key scope, integration name or account settings did not pass validation.
    Configuration,
    /// Stripe wire amounts are limited to the initial currency allowlist.
    UnsupportedCurrency,
    /// The response could not be obtained. Retry only with the original terms and key.
    InFlight,
    /// Stripe rejected the request.
    Rejected(u16),
    /// The response does not describe the frozen attempt.
    Response,
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadReturnUrl => {
                f.write_str("Checkout return destination must use HTTPS without credentials")
            }
            Self::Configuration => {
                f.write_str("Stripe Checkout configuration is invalid for this account or mode")
            }
            Self::UnsupportedCurrency => {
                f.write_str("Stripe Checkout supports USD, GBP and EUR in this adapter")
            }
            Self::InFlight => f.write_str(
                "Checkout result is uncertain; verify the session or retry the frozen request",
            ),
            Self::Rejected(code) => write!(
                f,
                "Stripe rejected the Checkout request with HTTP status {code}"
            ),
            Self::Response => {
                f.write_str("Stripe Checkout response did not match the frozen request")
            }
        }
    }
}

impl std::error::Error for CheckoutError {}

/// Factory for Stripe-hosted Checkout sessions with per-client restricted credentials.
pub struct StripeCheckout {
    config: CheckoutConfig,
    http: reqwest::blocking::Client,
}

impl StripeCheckout {
    /// Validate the origin and make an HTTPS-only client with bounded deadlines.
    ///
    /// # Errors
    /// Returns configuration failure for mismatched keys or invalid identifiers.
    pub fn new(config: CheckoutConfig) -> Result<Self, CheckoutError> {
        let prefix = match config.mode {
            StripeMode::Sandbox => "rk_test_",
            StripeMode::Live => "rk_live_",
        };
        let key = config.restricted_key.expose_secret();
        if !key.starts_with(prefix)
            || config.integration_identifier.is_empty()
            || config.integration_identifier.len() > 100
            || !config
                .integration_identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(CheckoutError::Configuration);
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CheckoutError::Configuration)?;
        Ok(Self { config, http })
    }
}

impl CheckoutProvider for StripeCheckout {
    fn create(&self, terms: &CollectionTerms) -> Result<CheckoutLink, CheckoutError> {
        if terms.account != self.config.account || terms.mode != self.config.mode {
            return Err(CheckoutError::Configuration);
        }
        let currency = stripe_currency(terms.expected.money().currency())?;
        let gross = terms.expected.money().amount().minor_units().to_string();
        let source = terms.id.to_string();
        let name = format!("Invoice {}", terms.invoice);
        let params = [
            ("mode", "payment"),
            ("success_url", self.config.return_url.0.as_str()),
            ("client_reference_id", source.as_str()),
            (
                "metadata[integration_identifier]",
                self.config.integration_identifier.as_str(),
            ),
            ("line_items[0][price_data][currency]", currency),
            ("line_items[0][price_data][unit_amount]", gross.as_str()),
            (
                "line_items[0][price_data][product_data][name]",
                name.as_str(),
            ),
            ("line_items[0][quantity]", "1"),
        ];
        let mut request = self
            .http
            .post(CHECKOUT_API)
            .basic_auth(
                self.config.restricted_key.expose_secret(),
                Option::<&str>::None,
            )
            .header("Stripe-Version", STRIPE_VERSION)
            .header("Idempotency-Key", &source)
            .form(&params);
        if self.config.connected_account {
            request = request.header("Stripe-Account", self.config.account.to_string());
        }
        let response = request.send().map_err(|_| CheckoutError::InFlight)?;
        let status = response.status();
        if !status.is_success() {
            if status.is_server_error() || matches!(status.as_u16(), 409 | 429) {
                return Err(CheckoutError::InFlight);
            }
            return Err(CheckoutError::Rejected(status.as_u16()));
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| CheckoutError::InFlight)?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(CheckoutError::Response);
        }
        decode_checkout(&body, terms, &source, currency, self.config.mode)
    }
}

#[derive(Deserialize)]
struct Response {
    id: String,
    url: String,
    client_reference_id: String,
    currency: String,
    amount_total: i64,
    livemode: bool,
}

fn decode_checkout(
    body: &[u8],
    terms: &CollectionTerms,
    source: &str,
    currency: &str,
    mode: StripeMode,
) -> Result<CheckoutLink, CheckoutError> {
    let response: Response = serde_json::from_slice(body).map_err(|_| CheckoutError::Response)?;
    let url = Url::parse(&response.url).map_err(|_| CheckoutError::Response)?;
    if response.client_reference_id != source
        || response.currency != currency
        || i128::from(response.amount_total) != terms.expected.money().amount().minor_units()
        || response.livemode != matches!(mode, StripeMode::Live)
        || url.scheme() != "https"
        || url.host_str() != Some("checkout.stripe.com")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(CheckoutError::Response);
    }
    Ok(CheckoutLink {
        session: response.id.parse().map_err(|_| CheckoutError::Response)?,
        url,
    })
}

fn stripe_currency(currency: Currency) -> Result<&'static str, CheckoutError> {
    match currency {
        Currency::USD => Ok("usd"),
        Currency::GBP => Ok("gbp"),
        Currency::EUR => Ok("eur"),
        _ => Err(CheckoutError::UnsupportedCurrency),
    }
}
