use std::{io::Read, time::Duration};

use beankeeper::types::Currency;
use beankeeper_cli::{db::amounts::PositiveMinorUnits, disbursement::DisbursementId};
use chrono::Duration as Days;
use mti::prelude::{MagicTypeId, MagicTypeIdExt};
use reqwest::{
    Url,
    blocking::{Client, RequestBuilder},
    redirect::Policy,
};
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{json, value::RawValue};

use super::{Config, Environment, Error, Order, Provider, Result, WiseId};

const MAX_PAYLOAD: u64 = 256 * 1024;
const PAGE_SIZE: u8 = 25;
const MAX_PAGES: u8 = 4;

pub(super) struct Wise {
    client: Client,
    origin: Url,
    token: SecretString,
}

#[derive(Deserialize)]
struct Profile {
    id: WiseId,
    #[serde(rename = "type")]
    kind: String,
    address: ProfileAddress,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileAddress {
    country_iso2_code: String,
}

#[derive(Deserialize)]
struct Balance {
    id: WiseId,
    currency: Currency,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Recipient {
    id: WiseId,
    #[serde(rename = "profileId")]
    profile: WiseId,
    country: String,
    active: bool,
    currency: Currency,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quote {
    id: String,
    profile: WiseId,
    target_account: Option<WiseId>,
    source_currency: Currency,
    target_currency: Currency,
    source_amount: Box<RawValue>,
    target_amount: Box<RawValue>,
    payment_options: Vec<QuoteOption>,
    pay_out: String,
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteOption {
    pay_in: String,
    pay_out: String,
    disabled: bool,
    source_amount: Box<RawValue>,
    target_amount: Box<RawValue>,
    source_currency: Currency,
    target_currency: Currency,
    price: QuotePrice,
}

#[derive(Deserialize)]
struct QuotePrice {
    total: QuotePriceTotal,
}

#[derive(Deserialize)]
struct QuotePriceTotal {
    value: QuotePriceValue,
}

#[derive(Deserialize)]
struct QuotePriceValue {
    amount: Box<RawValue>,
    currency: Currency,
}

impl Quote {
    fn approve(
        self,
        profile: WiseId,
        recipient: WiseId,
        amount: PositiveMinorUnits,
    ) -> Result<String> {
        let mut options = self.payment_options.iter().filter(|option| {
            !option.disabled && option.pay_in == "BALANCE" && option.pay_out == "BANK_TRANSFER"
        });
        let option = options.next().ok_or(Error::Mismatch)?;
        let price = &option.price.total.value;
        if options.next().is_some()
            || self.profile != profile
            || self
                .target_account
                .is_some_and(|account| account != recipient)
            || self.status != "PENDING"
            || self.pay_out != "BANK_TRANSFER"
            || option.source_currency != Currency::USD
            || option.target_currency != Currency::USD
            || cents(&option.source_amount)? != amount.minor_units()
            || cents(&option.target_amount)? != amount.minor_units()
            || self.source_currency != Currency::USD
            || self.target_currency != Currency::USD
            || price.currency != Currency::USD
            || cents(&self.source_amount)? != amount.minor_units()
            || cents(&self.target_amount)? != amount.minor_units()
            || cents(&price.amount)? != 0
            || !valid_uuid(&self.id)
        {
            return Err(Error::Mismatch);
        }
        Ok(self.id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Transfer {
    pub(super) id: WiseId,
    pub(super) status: String,
    business: WiseId,
    source_account: Option<WiseId>,
    target_account: WiseId,
    customer_transaction_id: String,
    quote_uuid: String,
    source_currency: Currency,
    target_currency: Currency,
    source_value: Box<RawValue>,
    target_value: Box<RawValue>,
    #[serde(rename = "type")]
    kind: String,
}

impl Transfer {
    pub(super) fn verify(&self, order: &Order) -> Result<()> {
        let paid = order.payment.amount_minor.minor_units();
        let source = cents(&self.source_value)?;
        if self.business != order.profile
            || self.source_account.is_some()
            || self.target_account != order.recipient
            || self.customer_transaction_id != key(&order.payment.key)?
            || self.quote_uuid != order.quote
            || self.kind != "standard"
            || self.source_currency != Currency::USD
            || self.target_currency != Currency::USD
            || !(source == paid
                || (source == 0
                    && !order.funding_attempted
                    && matches!(
                        self.status.as_str(),
                        "incoming_payment_waiting" | "cancelled"
                    )))
            || cents(&self.target_value)? != paid
            || order.transfer.is_some_and(|id| id != self.id)
        {
            return Err(Error::Mismatch);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Requirement {
    fields: Vec<RequirementField>,
}

#[derive(Deserialize)]
struct RequirementField {
    group: Vec<RequirementInput>,
}

#[derive(Deserialize)]
struct RequirementInput {
    required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Funding {
    status: String,
    error_code: Option<String>,
    balance_transaction_id: Option<WiseId>,
    #[serde(rename = "type")]
    kind: String,
}

impl Wise {
    pub(super) fn new(environment: Environment, token: SecretString) -> Result<Self> {
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| Error::Configuration)?;
        Ok(Self {
            client,
            token,
            origin: Url::parse(environment.origin()).map_err(|_| Error::Configuration)?,
        })
    }

    pub(super) fn check_source(&self, config: &Config) -> Result<()> {
        let profile = config.profile_id.to_string();
        let fetched: Profile = self.get(&["profiles", &profile])?;
        let balance: Balance = self.get(&[
            "profiles",
            &profile,
            "balances",
            &config.balance_id.to_string(),
        ])?;
        if fetched.id != config.profile_id
            || fetched.kind != "BUSINESS"
            || !fetched.address.country_iso2_code.eq_ignore_ascii_case("US")
            || balance.id != config.balance_id
            || balance.currency != Currency::USD
            || balance.kind != "STANDARD"
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|()| Error::Configuration)?
            .pop_if_empty()
            .extend(segments.iter().copied());
        Ok(url)
    }

    fn get<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T> {
        self.read(self.client.get(self.endpoint(segments)?))
    }

    fn read<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        let response = request
            .bearer_auth(self.token.expose_secret())
            .send()
            .map_err(|_| Error::Provider)?;
        if !response.status().is_success() {
            return Err(Error::Provider);
        }
        let mut body = Vec::new();
        response
            .take(MAX_PAYLOAD + 1)
            .read_to_end(&mut body)
            .map_err(|_| Error::Provider)?;
        if body.len() as u64 > MAX_PAYLOAD {
            return Err(Error::Provider);
        }
        serde_json::from_slice(&body).map_err(|_| Error::Provider)
    }
}

impl Provider for Wise {
    fn check_recipient(&self, profile: WiseId, recipient: WiseId) -> Result<()> {
        let fetched: Recipient = self.get(&["accounts", &recipient.to_string()])?;
        if fetched.id != recipient
            || fetched.profile != profile
            || !fetched.active
            || fetched.currency != Currency::USD
            || !fetched.kind.eq_ignore_ascii_case("aba")
            || !fetched.country.eq_ignore_ascii_case("US")
        {
            return Err(Error::Mismatch);
        }
        Ok(())
    }

    fn quote(
        &self,
        config: &Config,
        recipient: WiseId,
        amount: PositiveMinorUnits,
    ) -> Result<String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct QuoteRequest<'a> {
            source_currency: Currency,
            target_currency: Currency,
            target_amount: &'a RawValue,
            target_account: WiseId,
            preferred_pay_in: &'static str,
            pay_out: &'static str,
        }
        let decimal = Decimal::new(amount.minor_units(), 2);
        let target_amount = RawValue::from_string(decimal.to_string()).map_err(|_| Error::Input)?;
        let request = self
            .client
            .post(self.endpoint(&["profiles", &config.profile_id.to_string(), "quotes"])?)
            .json(&QuoteRequest {
                source_currency: Currency::USD,
                target_currency: Currency::USD,
                target_amount: &target_amount,
                target_account: recipient,
                preferred_pay_in: "BALANCE",
                pay_out: "BANK_TRANSFER",
            });
        let quote: Quote = self.read(request)?;
        let quote = quote.approve(config.profile_id, recipient, amount)?;
        let requirements: Vec<Requirement> = self.read(
            self.client
                .post(self.endpoint(&["transfer-requirements"])?)
                .json(&json!({"targetAccount":recipient,"quoteUuid":quote,"originatorLegalEntityType":"BUSINESS","details":{}})),
        )?;
        if requirements
            .iter()
            .flat_map(|rule| &rule.fields)
            .flat_map(|field| &field.group)
            .any(|input| input.required)
        {
            return Err(Error::Mismatch);
        }
        Ok(quote)
    }

    fn create(&self, order: &Order) -> Result<Transfer> {
        self.read(
            self.client
                .post(self.endpoint(&["transfers"])?)
                .json(&json!({
                    "targetAccount":order.recipient, "quoteUuid":order.quote,
                    "customerTransactionId":key(&order.payment.key)?, "details":{}
                })),
        )
    }

    fn find(&self, order: &Order) -> Result<Option<Transfer>> {
        if let Some(id) = order.transfer {
            return self.get(&["transfers", &id.to_string()]).map(Some);
        }
        let start = (order.created - Days::minutes(5)).to_rfc3339();
        let end = (order.created + Days::minutes(5)).to_rfc3339();
        let key = key(&order.payment.key)?;
        let mut found = None;
        for page in 0..MAX_PAGES {
            let mut url = self.endpoint(&["transfers"])?;
            url.query_pairs_mut()
                .append_pair("profile", &order.profile.to_string())
                .append_pair("createdDateStart", &start)
                .append_pair("createdDateEnd", &end)
                .append_pair("sourceCurrency", "USD")
                .append_pair("targetCurrency", "USD")
                .append_pair("limit", &PAGE_SIZE.to_string())
                .append_pair("offset", &(page * PAGE_SIZE).to_string());
            let rows: Vec<Box<RawValue>> = self.read(self.client.get(url))?;
            let last_page = rows.len() < usize::from(PAGE_SIZE);
            for row in rows {
                if let Some(transfer) = transfer_for_key(&row, &key)? {
                    if found.is_some() {
                        return Err(Error::Mismatch);
                    }
                    transfer.verify(order)?;
                    found = Some(transfer);
                }
            }
            if last_page {
                return Ok(found);
            }
        }
        Ok(found)
    }

    fn fund(&self, order: &Order, transfer: WiseId) -> Result<WiseId> {
        let report: Funding = self.read(
            self.client
                .post(self.endpoint(&[
                    "profiles",
                    &order.profile.to_string(),
                    "transfers",
                    &transfer.to_string(),
                    "payments",
                ])?)
                .json(&json!({"type":"BALANCE","balanceId":order.balance})),
        )?;
        if report.kind != "BALANCE" || report.error_code.is_some() || report.status != "COMPLETED" {
            return Err(Error::Pending);
        }
        report.balance_transaction_id.ok_or(Error::Pending)
    }
}

fn transfer_for_key(row: &RawValue, key: &str) -> Result<Option<Transfer>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LookupKey<'a> {
        customer_transaction_id: Option<&'a str>,
    }

    let lookup: LookupKey<'_> = serde_json::from_str(row.get()).map_err(|_| Error::Provider)?;
    if lookup.customer_transaction_id == Some(key) {
        serde_json::from_str(row.get())
            .map(Some)
            .map_err(|_| Error::Provider)
    } else {
        Ok(None)
    }
}

fn cents(value: &RawValue) -> Result<i64> {
    let value: Decimal = value.get().parse().map_err(|_| Error::Provider)?;
    let cents = value
        .checked_mul(Decimal::from(100))
        .filter(|value| value.fract().is_zero())
        .ok_or(Error::Mismatch)?;
    i64::try_from(cents).map_err(|_| Error::Mismatch)
}

fn valid_uuid(input: &str) -> bool {
    input.len() == 36
        && input.bytes().enumerate().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == b'-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

pub(super) fn key(id: &DisbursementId) -> Result<String> {
    id.to_string()
        .parse::<MagicTypeId>()
        .and_then(|id| id.uuid_str())
        .map_err(|_| Error::Input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_lookup_checks_key_before_parsing_unrelated_rows() -> Result<()> {
        let row = RawValue::from_string(
            r#"{"customerTransactionId":"other","business":null,"type":"third_party"}"#.into(),
        )
        .map_err(|_| Error::Provider)?;
        assert!(transfer_for_key(&row, "ours")?.is_none());
        assert!(matches!(
            transfer_for_key(&row, "other"),
            Err(Error::Provider)
        ));
        Ok(())
    }

    #[test]
    fn exact_balance_quote_rejects_fees_or_rounded_principal() -> Result<()> {
        let profile: WiseId = 5.try_into()?;
        let recipient: WiseId = 8.try_into()?;
        let amount: PositiveMinorUnits = 4200.try_into().map_err(|_| Error::Input)?;
        for (price, source, allowed) in [
            ("0.0", "42.00", true),
            ("0.01", "42.00", false),
            ("0.0", "42.001", false),
        ] {
            let wire = format!(
                r#"{{
              "id":"11144c35-9fe8-4c32-b7fd-d05c2a7734bf",
              "profile":5,"targetAccount":8,"sourceCurrency":"USD","targetCurrency":"USD",
              "sourceAmount":{source},"targetAmount":42,"status":"PENDING","payOut":"BANK_TRANSFER",
              "paymentOptions":[{{"payIn":"BALANCE","payOut":"BANK_TRANSFER",
                  "disabled":false,"sourceAmount":42,"targetAmount":42,
                  "sourceCurrency":"USD","targetCurrency":"USD",
                  "price":{{"total":{{"value":{{"amount":{price},"currency":"USD"}}}}}}
              }}]
            }}"#
            );
            let quote: Quote = serde_json::from_str(&wire).map_err(|_| Error::Provider)?;
            assert_eq!(quote.approve(profile, recipient, amount).is_ok(), allowed);
        }
        Ok(())
    }
}
