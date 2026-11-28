//! Adapter for a trusted local host that owns beneficiary routing and credentials.

use std::{io::Read as _, net::IpAddr, time::Duration};

use reqwest::{
    Url,
    blocking::{Client, RequestBuilder},
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    db::{
        companies::CompanySlug,
        counterparties::CounterpartyId,
        disbursements::{Disbursement, ProviderEvidence, Review},
    },
    disbursement::{DestinationFingerprint, DisbursementId},
    error::CliError,
};

const MAX_RESPONSE: u64 = 16 * 1024;

pub(super) struct Host {
    client: Client,
    endpoint: Url,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    fingerprint: DestinationFingerprint,
}

#[derive(Serialize)]
struct Payment<'a> {
    key: &'a DisbursementId,
    company: &'a CompanySlug,
    cash_account: &'a beankeeper::types::AccountCode,
    vendor: &'a CounterpartyId,
    destination: &'a DestinationFingerprint,
    amount_minor: i64,
    currency: beankeeper::types::Currency,
}

impl Host {
    pub(super) fn configured() -> Result<Self, CliError> {
        let endpoint = std::env::var("BK_DISBURSEMENT_HOST")
            .ok()
            .and_then(|value| Url::parse(&value).ok())
            .filter(local_endpoint)
            .ok_or_else(|| invalid("BK_DISBURSEMENT_HOST must be an HTTP loopback endpoint"))?;
        let token = std::env::var("BK_DISBURSEMENT_HOST_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| invalid("BK_DISBURSEMENT_HOST_TOKEN is required"))?;
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| unavailable())?;
        Ok(Self {
            client,
            endpoint,
            token,
        })
    }

    pub(super) fn destination(
        &self,
        vendor: &CounterpartyId,
    ) -> Result<DestinationFingerprint, CliError> {
        let url = self.endpoint(&["destinations", &vendor.to_string()])?;
        let result: Destination = self
            .read(self.client.get(url), false)?
            .ok_or_else(unavailable)?;
        Ok(result.fingerprint)
    }

    pub(super) fn lookup(
        &self,
        key: &DisbursementId,
    ) -> Result<Option<ProviderEvidence>, CliError> {
        let url = self.endpoint(&["payments", &key.to_string()])?;
        self.read(self.client.get(url), true)
    }

    pub(super) fn submit(
        &self,
        company: &CompanySlug,
        record: &Disbursement,
        review: &Review,
    ) -> Result<ProviderEvidence, CliError> {
        let url = self.endpoint(&["payments"])?;
        let payment = Payment {
            key: &record.id,
            company,
            cash_account: &record.terms.cash_account,
            vendor: &record.vendor,
            destination: &review.destination,
            amount_minor: record.amount_minor.minor_units(),
            currency: record.currency,
        };
        self.read(
            self.client
                .post(url)
                .header("Idempotency-Key", record.id.to_string())
                .json(&payment),
            false,
        )?
        .ok_or_else(unavailable)
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, CliError> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|()| invalid("payment host URL cannot hold endpoint paths"))?
            .pop_if_empty()
            .extend(segments.iter().copied());
        Ok(url)
    }

    fn read<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        accept_missing: bool,
    ) -> Result<Option<T>, CliError> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .map_err(|_| unavailable())?;
        if accept_missing && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE + 1)
            .read_to_end(&mut body)
            .map_err(|_| unavailable())?;
        if body.len() as u64 > MAX_RESPONSE {
            return Err(unavailable());
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| unavailable())
    }
}

fn local_endpoint(url: &Url) -> bool {
    let host = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok());
    host.is_some_and(|ip| ip.is_loopback())
        && matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn invalid(message: &str) -> CliError {
    CliError::Validation(message.into())
}

fn unavailable() -> CliError {
    CliError::General("payment host did not provide a usable response; check the durable disbursement status before retrying".into())
}
