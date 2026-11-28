//! Host-side provider transport. The ledger never receives provider secrets.

use std::{
    io::Read,
    time::{Duration, Instant},
};

use beankeeper::types::Currency;
use chrono::NaiveDate;
use reqwest::blocking::Client;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::{
    FeedConnectionId,
    model::{
        FeedDelta, FeedError, FeedTransaction, ProviderAccountKey, ProviderTransactionKey,
        RemovedTransaction, SignedMinor, SyncCursor,
    },
};
use crate::db::companies::CompanySlug;

const MAX_PAGES: usize = 100;
const MAX_ROWS: usize = 20_000;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct Page {
    pub(super) delta: FeedDelta,
    pub(super) cursor: SyncCursor,
    pub(super) has_more: bool,
}

pub(super) trait BankFeedProvider {
    fn sync_page(&self, cursor: Option<&SyncCursor>) -> Result<Page, FeedError>;
}

pub(super) fn fetch_complete(
    provider: &impl BankFeedProvider,
    committed_cursor: Option<&SyncCursor>,
) -> Result<(SyncCursor, FeedDelta), FeedError> {
    let started = Instant::now();
    match fetch_window(provider, committed_cursor, started) {
        Err(FeedError::PaginationChanged) => fetch_window(provider, committed_cursor, started),
        result => result,
    }
}

fn fetch_window(
    provider: &impl BankFeedProvider,
    committed_cursor: Option<&SyncCursor>,
    started: Instant,
) -> Result<(SyncCursor, FeedDelta), FeedError> {
    let mut cursor = committed_cursor.cloned();
    let mut combined = FeedDelta::default();
    for _ in 0..MAX_PAGES {
        if started.elapsed() > Duration::from_secs(120) {
            return Err(FeedError::PaginationLimit);
        }
        let mut page = provider.sync_page(cursor.as_ref())?;
        if combined.count().saturating_add(page.delta.count()) > MAX_ROWS {
            return Err(FeedError::PaginationLimit);
        }
        combined.rows.append(&mut page.delta.rows);
        combined.removed.append(&mut page.delta.removed);
        if !page.has_more {
            return Ok((page.cursor, combined));
        }
        if cursor.as_ref() == Some(&page.cursor) {
            return Err(FeedError::InvalidResponse);
        }
        cursor = Some(page.cursor);
    }
    Err(FeedError::PaginationLimit)
}

/// The set of HTTPS endpoints allowed to receive Plaid credentials.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum PlaidEnvironment {
    #[default]
    Sandbox,
    Production,
}

impl PlaidEnvironment {
    const fn endpoint(self) -> &'static str {
        match self {
            Self::Sandbox => "https://sandbox.plaid.com/transactions/sync",
            Self::Production => "https://production.plaid.com/transactions/sync",
        }
    }
}

pub(super) struct PlaidProvider {
    client: Client,
    endpoint: &'static str,
    account: ProviderAccountKey,
    credentials: PlaidCredentials,
}

/// Validated Plaid credentials. Secret fields cannot be serialized or formatted.
pub struct PlaidCredentials {
    client_id: SecretString,
    secret: SecretString,
    access_token: SecretString,
}

/// The trusted host's credential source for one authorized feed mapping.
///
/// Returning an error prevents falling back to process environment credentials.
pub trait PlaidCredentialSource: Send + Sync + 'static {
    /// Retrieve credentials for this company and feed.
    ///
    /// # Errors
    /// Reports a missing or unavailable private host credential.
    fn credentials(
        &self,
        company: &CompanySlug,
        feed: &FeedConnectionId,
    ) -> Result<PlaidCredentials, FeedError>;
}

impl TryFrom<&SecretString> for PlaidCredentials {
    type Error = FeedError;

    fn try_from(value: &SecretString) -> Result<Self, FeedError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input<'a> {
            client_id: &'a str,
            secret: &'a str,
            access_token: &'a str,
        }
        let input: Input<'_> = serde_json::from_str(value.expose_secret())
            .map_err(|_| FeedError::InvalidCredentials)?;
        Ok(Self {
            client_id: validate_secret(input.client_id)?,
            secret: validate_secret(input.secret)?,
            access_token: validate_secret(input.access_token)?,
        })
    }
}

fn validate_secret(value: &str) -> Result<SecretString, FeedError> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_whitespace) {
        return Err(FeedError::InvalidCredentials);
    }
    Ok(value.into())
}

impl PlaidCredentials {
    pub(super) fn from_environment() -> Result<Self, FeedError> {
        fn host_secret(key: &str) -> Result<SecretString, FeedError> {
            let value = std::env::var(key).map_err(|_| FeedError::MissingCredentials)?;
            let value: SecretString = value.into();
            validate_secret(value.expose_secret())
        }
        Ok(Self {
            client_id: host_secret("BK_PLAID_CLIENT_ID")?,
            secret: host_secret("BK_PLAID_SECRET")?,
            access_token: host_secret("BK_PLAID_ACCESS_TOKEN")?,
        })
    }
}

impl PlaidProvider {
    pub(super) fn new(
        environment: PlaidEnvironment,
        account: ProviderAccountKey,
        credentials: PlaidCredentials,
    ) -> Result<Self, FeedError> {
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| FeedError::Transport)?;
        Ok(Self {
            client,
            endpoint: environment.endpoint(),
            account,
            credentials,
        })
    }
}

#[derive(Serialize)]
struct PlaidRequest<'a> {
    access_token: &'a str,
    account_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<&'a str>,
    count: u16,
}

impl BankFeedProvider for PlaidProvider {
    fn sync_page(&self, cursor: Option<&SyncCursor>) -> Result<Page, FeedError> {
        let request = PlaidRequest {
            access_token: self.credentials.access_token.expose_secret(),
            account_id: self.account.as_str(),
            cursor: cursor.map(SyncCursor::as_str),
            count: 500,
        };
        let response = self
            .client
            .post(self.endpoint)
            .header(
                "PLAID-CLIENT-ID",
                self.credentials.client_id.expose_secret(),
            )
            .header("PLAID-SECRET", self.credentials.secret.expose_secret())
            .json(&request)
            .send()
            .map_err(|_| FeedError::Transport)?;
        let status = response.status();
        let mut bytes = Vec::new();
        let length = response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| FeedError::Transport)?;
        if u64::try_from(length).map_or(true, |length| length > MAX_RESPONSE_BYTES) {
            return Err(FeedError::PaginationLimit);
        }
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(FeedError::Credentials);
            }
            return Err(plaid_error(&bytes));
        }
        parse_page(&bytes, &self.account)
    }
}

fn plaid_error(bytes: &[u8]) -> FeedError {
    #[derive(Deserialize)]
    struct Response<'a> {
        error_code: &'a str,
    }
    match serde_json::from_slice::<Response<'_>>(bytes)
        .ok()
        .map(|response| response.error_code)
    {
        Some("TRANSACTIONS_SYNC_MUTATION_DURING_PAGINATION") => FeedError::PaginationChanged,
        Some(
            "ITEM_LOGIN_REQUIRED" | "INVALID_ACCESS_TOKEN" | "INVALID_API_KEYS" | "ITEM_NOT_FOUND",
        ) => FeedError::Credentials,
        _ => FeedError::Transport,
    }
}

#[derive(Deserialize)]
struct PlaidRow {
    account_id: ProviderAccountKey,
    transaction_id: ProviderTransactionKey,
    pending_transaction_id: Option<ProviderTransactionKey>,
    date: NaiveDate,
    name: String,
    amount: Box<RawValue>,
    iso_currency_code: Option<String>,
    pending: bool,
}

impl TryFrom<PlaidRow> for FeedTransaction {
    type Error = FeedError;

    fn try_from(row: PlaidRow) -> Result<Self, Self::Error> {
        let currency: Currency = row
            .iso_currency_code
            .as_deref()
            .ok_or(FeedError::InvalidMapping)?
            .parse()
            .map_err(|_| FeedError::InvalidMapping)?;
        let description = row.name.trim();
        if description.is_empty()
            || description.len() > 1024
            || description.chars().any(char::is_control)
        {
            return Err(FeedError::InvalidResponse);
        }
        Ok(Self {
            account: row.account_id,
            key: row.transaction_id,
            pending_key: row.pending_transaction_id,
            date: row.date,
            description: description.to_owned(),
            amount: normalize_amount(row.amount.get(), currency)?,
            currency,
            pending: row.pending,
        })
    }
}

fn normalize_amount(value: &str, currency: Currency) -> Result<SignedMinor, FeedError> {
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent
                .parse::<i32>()
                .map_err(|_| FeedError::InvalidAmount)?,
        ),
        None => (value, 0),
    };
    let decimal = Decimal::from_str_exact(mantissa)
        .map_err(|_| FeedError::InvalidAmount)?
        .normalize();
    let input_scale = i32::try_from(decimal.scale()).map_err(|_| FeedError::InvalidAmount)?;
    let extra_scale = i32::from(currency.minor_units())
        .checked_add(exponent)
        .and_then(|scale| scale.checked_sub(input_scale))
        .and_then(|scale| u32::try_from(scale).ok())
        .ok_or(FeedError::InvalidAmount)?;
    let minor = 10_i128
        .checked_pow(extra_scale)
        .and_then(|factor| decimal.mantissa().checked_mul(factor))
        .and_then(i128::checked_neg)
        .and_then(|reversed| i64::try_from(reversed).ok())
        .ok_or(FeedError::InvalidAmount)?;
    SignedMinor::new(minor)
}

fn parse_page(bytes: &[u8], account: &ProviderAccountKey) -> Result<Page, FeedError> {
    #[derive(Deserialize)]
    struct Removal {
        account_id: Option<ProviderAccountKey>,
        transaction_id: ProviderTransactionKey,
    }
    #[derive(Deserialize)]
    struct Response {
        added: Vec<PlaidRow>,
        modified: Vec<PlaidRow>,
        removed: Vec<Removal>,
        next_cursor: SyncCursor,
        has_more: bool,
    }
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| FeedError::InvalidResponse)?;
    Ok(Page {
        delta: FeedDelta {
            rows: response
                .added
                .into_iter()
                .chain(response.modified)
                .filter(|row| &row.account_id == account)
                .map(FeedTransaction::try_from)
                .collect::<Result<_, _>>()?,
            removed: response
                .removed
                .into_iter()
                .map(|row| RemovedTransaction {
                    account: row.account_id,
                    key: row.transaction_id,
                })
                .collect(),
        },
        cursor: response.next_cursor,
        has_more: response.has_more,
    })
}

impl PlaidEnvironment {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }
}

impl std::str::FromStr for PlaidEnvironment {
    type Err = FeedError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sandbox" => Ok(Self::Sandbox),
            "production" => Ok(Self::Production),
            _ => Err(FeedError::InvalidMapping),
        }
    }
}
#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;

    #[test]
    fn authorization_errors_contain_no_provider_details() {
        assert_eq!(
            plaid_error(br#"{"error_code":"ITEM_LOGIN_REQUIRED","error_message":"private info"}"#),
            FeedError::Credentials
        );
        assert!(!FeedError::Credentials.to_string().contains("private info"));
        let invalid = SecretString::from(
            r#"{"client_id":"fixture","secret":"large secret","access_token":"fixture"}"#,
        );
        assert!(matches!(
            PlaidCredentials::try_from(&invalid),
            Err(FeedError::InvalidCredentials)
        ));
        let valid = SecretString::from(
            r#"{"client_id":"fixture","secret":"secret","access_token":"fixture"}"#,
        );
        assert!(PlaidCredentials::try_from(&valid).is_ok());
    }

    #[test]
    fn plaid_outflows_use_exact_decimal_minor_units() {
        assert_eq!(
            normalize_amount("31.10", Currency::USD).map(SignedMinor::get),
            Ok(-3110)
        );
        assert_eq!(
            normalize_amount("-1.25e1", Currency::USD).map(SignedMinor::get),
            Ok(1250)
        );
        assert_eq!(
            normalize_amount("0.001", Currency::USD),
            Err(FeedError::InvalidAmount)
        );
        assert_eq!(
            normalize_amount("92233720368547758.08", Currency::USD),
            Err(FeedError::InvalidAmount)
        );
    }

    #[test]
    fn pagination_restarts_from_committed_cursor() -> Result<(), FeedError> {
        type Step = (Option<&'static str>, Result<Page, FeedError>);
        struct Script(RefCell<VecDeque<Step>>);
        impl BankFeedProvider for Script {
            fn sync_page(&self, cursor: Option<&SyncCursor>) -> Result<Page, FeedError> {
                let (expected, response) = self
                    .0
                    .borrow_mut()
                    .pop_front()
                    .ok_or(FeedError::InvalidResponse)?;
                assert_eq!(expected, cursor.map(SyncCursor::as_str));
                response
            }
        }

        let page = |cursor: &str, has_more| {
            Ok(Page {
                delta: FeedDelta::default(),
                cursor: cursor.parse()?,
                has_more,
            })
        };
        let script = Script(RefCell::new(VecDeque::from([
            (Some("committed"), page("discarded", true)),
            (Some("discarded"), Err(FeedError::PaginationChanged)),
            (Some("committed"), page("final", false)),
        ])));
        let (cursor, delta) = fetch_complete(&script, Some(&"committed".parse()?))?;
        assert_eq!(cursor.as_str(), "final");
        assert_eq!(delta.count(), 0);
        assert!(script.0.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn provider_credentials_and_transport_failures_stay_private() {
        struct FailingProvider(FeedError);
        impl BankFeedProvider for FailingProvider {
            fn sync_page(&self, _cursor: Option<&SyncCursor>) -> Result<Page, FeedError> {
                Err(self.0)
            }
        }

        let expired = plaid_error(
            br#"{"error_code":"ITEM_LOGIN_REQUIRED","error_message":"private context"}"#,
        );
        let transient =
            plaid_error(br#"{"error_code":"INSTITUTION_DOWN","error_message":"private context"}"#);
        assert_eq!(expired, FeedError::Credentials);
        assert_eq!(transient, FeedError::Transport);
        for error in [expired, transient] {
            assert!(
                matches!(fetch_complete(&FailingProvider(error), None), Err(failure) if failure == error)
            );
            assert!(!error.to_string().contains("private context"));
        }
    }
}
