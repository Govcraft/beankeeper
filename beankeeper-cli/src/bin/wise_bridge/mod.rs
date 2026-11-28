mod provider;
mod store;
#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    env, fmt,
    io::Read,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use beankeeper::types::{AccountCode, Currency, Sha256Hex};
use beankeeper_cli::{
    db::{amounts::PositiveMinorUnits, companies::CompanySlug, counterparties::CounterpartyId},
    disbursement::{DestinationFingerprint, DisbursementId, ProviderTransferId},
};
use chrono::{NaiveDate, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use self::{provider::Wise, store::Store};

const CONFIG_LIMIT: u64 = 64 * 1024;
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub(crate) enum Error {
    Configuration,
    Authorization,
    Input,
    Missing,
    Mismatch,
    Pending,
    Provider,
    Storage,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration => "invalid Wise bridge configuration",
            Self::Authorization => "invalid host authorization",
            Self::Input => "invalid domestic USD payment request",
            Self::Missing => "payment or recipient is not known to the Wise bridge",
            Self::Mismatch => "Wise details differ from the reviewed payment",
            Self::Pending => "Wise payment remains uncertain; inspect the existing transfer",
            Self::Provider => "Wise response unavailable or unsupported",
            Self::Storage => "Wise bridge state is unavailable",
        })
    }
}

impl std::error::Error for Error {}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Authorization => StatusCode::UNAUTHORIZED,
            Self::Missing => StatusCode::NOT_FOUND,
            Self::Input => StatusCode::BAD_REQUEST,
            Self::Mismatch => StatusCode::CONFLICT,
            Self::Pending | Self::Provider => StatusCode::BAD_GATEWAY,
            Self::Configuration | Self::Storage => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Environment {
    Sandbox,
    Live,
}

impl Environment {
    fn configured(mode: Option<&str>, live_acknowledgement: Option<&str>) -> Result<Self> {
        match mode {
            None | Some("sandbox") => Ok(Self::Sandbox),
            Some("live") if live_acknowledgement == Some("yes") => Ok(Self::Live),
            _ => Err(Error::Configuration),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Live => "live",
        }
    }

    const fn origin(self) -> &'static str {
        match self {
            Self::Sandbox => "https://api.wise-sandbox.com/2026Q4/",
            Self::Live => "https://api.wise.com/2026Q4/",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "i64", into = "i64")]
struct WiseId(std::num::NonZeroI64);

impl TryFrom<i64> for WiseId {
    type Error = Error;

    fn try_from(value: i64) -> Result<Self> {
        std::num::NonZeroI64::new(value)
            .filter(|id| id.get() > 0)
            .map(Self)
            .ok_or(Error::Input)
    }
}

impl From<WiseId> for i64 {
    fn from(value: WiseId) -> Self {
        value.0.get()
    }
}

impl fmt::Display for WiseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    company: CompanySlug,
    cash_account: AccountCode,
    profile_id: WiseId,
    balance_id: WiseId,
    recipients: BTreeMap<CounterpartyId, WiseId>,
}

impl Config {
    fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|_| Error::Configuration)?;
        let mut data = Vec::new();
        file.take(CONFIG_LIMIT + 1)
            .read_to_end(&mut data)
            .map_err(|_| Error::Configuration)?;
        if data.len() as u64 > CONFIG_LIMIT {
            return Err(Error::Configuration);
        }
        serde_json::from_slice(&data).map_err(|_| Error::Configuration)
    }

    fn recipient(&self, vendor: &CounterpartyId) -> Result<WiseId> {
        self.recipients.get(vendor).copied().ok_or(Error::Missing)
    }

    fn fingerprint(
        &self,
        environment: Environment,
        vendor: &CounterpartyId,
        recipient: WiseId,
    ) -> Result<DestinationFingerprint> {
        // Wise recipient details are immutable. A replacement recipient gets a new ID.
        let scope = serde_json::to_vec(&(
            "wise-usd-aba-v1",
            environment,
            &self.company,
            &self.cash_account,
            self.profile_id,
            self.balance_id,
            vendor,
            recipient,
        ))
        .map_err(|_| Error::Configuration)?;
        Ok(DestinationFingerprint(Sha256Hex::from_digest(
            Sha256::digest(scope).into(),
        )))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payment {
    key: DisbursementId,
    vendor: CounterpartyId,
    company: CompanySlug,
    cash_account: AccountCode,
    destination: DestinationFingerprint,
    currency: Currency,
    amount_minor: PositiveMinorUnits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Accepted,
    Settled,
    Failed,
    Returned,
}

#[derive(Serialize)]
struct Evidence {
    key: DisbursementId,
    company: CompanySlug,
    cash_account: AccountCode,
    destination: DestinationFingerprint,
    currency: Currency,
    amount_minor: PositiveMinorUnits,
    order_id: ProviderTransferId,
    status: Status,
    on: NaiveDate,
    settled_on: Option<NaiveDate>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Mark {
    status: Status,
    on: NaiveDate,
    settled_on: Option<NaiveDate>,
}

#[derive(Serialize)]
struct Destination {
    fingerprint: DestinationFingerprint,
}

#[derive(Serialize, Deserialize)]
struct Order {
    environment: Environment,
    payment: Payment,
    recipient: WiseId,
    profile: WiseId,
    balance: WiseId,
    quote: String,
    created: chrono::DateTime<Utc>,
    transfer: Option<WiseId>,
    funding_attempted: bool,
    funding_receipt: Option<WiseId>,
    mark: Option<Mark>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Reservation {
    Rejected(Box<Rejection>),
    Order(Box<Order>),
}

impl Reservation {
    fn payment(&self) -> &Payment {
        match self {
            Self::Rejected(rejection) => &rejection.payment,
            Self::Order(order) => &order.payment,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Rejection {
    environment: Environment,
    payment: Payment,
    rejected_on: NaiveDate,
}

impl Rejection {
    fn evidence(self) -> Result<Evidence> {
        let order_id = format!(
            "wise-{}-rejected-{}",
            self.environment.name(),
            self.payment.key,
        )
        .parse()
        .map_err(|_| Error::Storage)?;
        Ok(self.payment.evidence(
            order_id,
            Mark {
                status: Status::Failed,
                on: self.rejected_on,
                settled_on: None,
            },
        ))
    }
}

impl Payment {
    fn evidence(self, order_id: ProviderTransferId, mark: Mark) -> Evidence {
        Evidence {
            key: self.key,
            company: self.company,
            cash_account: self.cash_account,
            destination: self.destination,
            currency: self.currency,
            amount_minor: self.amount_minor,
            order_id,
            status: mark.status,
            on: mark.on,
            settled_on: mark.settled_on,
        }
    }
}

trait Provider: Send + Sync + 'static {
    fn check_recipient(&self, profile: WiseId, recipient: WiseId) -> Result<()>;
    fn quote(
        &self,
        config: &Config,
        recipient: WiseId,
        amount: PositiveMinorUnits,
    ) -> Result<String>;
    fn create(&self, order: &Order) -> Result<provider::Transfer>;
    fn find(&self, order: &Order) -> Result<Option<provider::Transfer>>;
    fn fund(&self, order: &Order, transfer: WiseId) -> Result<WiseId>;
}

struct Bridge<P> {
    environment: Environment,
    config: Config,
    token: SecretString,
    provider: P,
    store: Mutex<Store>,
}

impl<P: Provider> Bridge<P> {
    fn destination(&self, vendor: &CounterpartyId) -> Result<Destination> {
        let recipient = self.config.recipient(vendor)?;
        self.provider
            .check_recipient(self.config.profile_id, recipient)?;
        Ok(Destination {
            fingerprint: self
                .config
                .fingerprint(self.environment, vendor, recipient)?,
        })
    }

    fn submit(&self, payment: Payment) -> Result<Evidence> {
        let stored = self.store.lock().map_err(|_| Error::Storage)?;
        if let Some(reservation) = stored.read(&payment.key)? {
            if &payment != reservation.payment() {
                return Err(Error::Mismatch);
            }
            return self.evidence(&stored, reservation);
        }
        let recipient_and_quote = self.prepare_order(&payment);
        let (recipient, quote) = match recipient_and_quote {
            Ok(prepared) => prepared,
            Err(error) => {
                // No transfer create or funding request happened during preflight.
                let rejection = Rejection {
                    environment: self.environment,
                    payment,
                    rejected_on: Utc::now().date_naive(),
                };
                stored.insert(&rejection.payment.key, &rejection)?;
                eprintln!(
                    "Wise payment {} declined before creation: {error}",
                    rejection.payment.key
                );
                return rejection.evidence();
            }
        };
        let order = Order {
            environment: self.environment,
            payment,
            recipient,
            profile: self.config.profile_id,
            balance: self.config.balance_id,
            quote,
            created: Utc::now(),
            transfer: None,
            funding_attempted: false,
            funding_receipt: None,
            mark: None,
        };
        // INSERT is durable before create. A competing process cannot claim the key.
        stored.insert(&order.payment.key, &order)?;
        let transfer = self.provider.create(&order)?;
        transfer.verify(&order)?;
        let mut order = order;
        order.transfer = Some(transfer.id);
        if transfer.status == "incoming_payment_waiting" {
            // Store the nonretryable funding attempt before the only funding POST.
            order.funding_attempted = true;
            stored.save(&order)?;
            order.funding_receipt = Some(self.provider.fund(&order, transfer.id)?);
            stored.save(&order)?;
            return self.lookup_order(&stored, order);
        }
        Self::record(&stored, order, &transfer)
    }

    fn prepare_order(&self, payment: &Payment) -> Result<(WiseId, String)> {
        if payment.currency != Currency::USD
            || payment.company != self.config.company
            || payment.cash_account != self.config.cash_account
        {
            return Err(Error::Input);
        }
        let recipient = self.config.recipient(&payment.vendor)?;
        if payment.destination
            != self
                .config
                .fingerprint(self.environment, &payment.vendor, recipient)?
        {
            return Err(Error::Mismatch);
        }
        self.provider
            .check_recipient(self.config.profile_id, recipient)?;
        let quote = self
            .provider
            .quote(&self.config, recipient, payment.amount_minor)?;
        Ok((recipient, quote))
    }

    fn lookup(&self, key: &DisbursementId) -> Result<Evidence> {
        let store = self.store.lock().map_err(|_| Error::Storage)?;
        let reservation = store.read(key)?.ok_or(Error::Missing)?;
        self.evidence(&store, reservation)
    }

    fn evidence(&self, store: &Store, reservation: Reservation) -> Result<Evidence> {
        match reservation {
            Reservation::Rejected(rejection) => rejection.evidence(),
            Reservation::Order(order) => self.lookup_order(store, *order),
        }
    }

    fn lookup_order(&self, store: &Store, order: Order) -> Result<Evidence> {
        let transfer = self.provider.find(&order)?.ok_or(Error::Pending)?;
        Self::record(store, order, &transfer)
    }

    fn record(store: &Store, mut order: Order, transfer: &provider::Transfer) -> Result<Evidence> {
        transfer.verify(&order)?;
        order.transfer = Some(transfer.id);
        let observed = project_status(
            &transfer.status,
            order.mark,
            order.funding_attempted,
            order.funding_receipt.is_some(),
            Utc::now().date_naive(),
        );
        if let Ok(mark) = observed {
            order.mark = Some(mark);
        }
        store.save(&order)?;
        let mark = observed?;
        let order_id = format!("wise-{}-{}", order.environment.name(), transfer.id)
            .parse()
            .map_err(|_| Error::Provider)?;
        Ok(order.payment.evidence(order_id, mark))
    }

    fn require_token(&self, headers: &HeaderMap) -> Result<()> {
        let supplied = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Error::Authorization)?;
        let expected = self.token.expose_secret().as_bytes();
        if supplied.len() != expected.len()
            || supplied
                .bytes()
                .zip(expected)
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                != 0
        {
            return Err(Error::Authorization);
        }
        Ok(())
    }
}

fn project_status(
    raw: &str,
    previous: Option<Mark>,
    attempted: bool,
    authenticated_funding: bool,
    today: NaiveDate,
) -> Result<Mark> {
    let previous_status = previous.map(|mark| mark.status);
    let next = match raw {
        "incoming_payment_waiting" if attempted => return Err(Error::Pending),
        "incoming_payment_waiting"
        | "incoming_payment_initiated"
        | "processing"
        | "funds_converted"
            if !matches!(
                previous_status,
                Some(Status::Settled | Status::Returned | Status::Failed)
            ) =>
        {
            Status::Accepted
        }
        "outgoing_payment_sent"
            if authenticated_funding
                && !matches!(previous_status, Some(Status::Returned | Status::Failed)) =>
        {
            Status::Settled
        }
        "cancelled"
            if !attempted
                && !matches!(previous_status, Some(Status::Settled | Status::Returned)) =>
        {
            Status::Failed
        }
        "funds_refunded"
            if authenticated_funding
                && matches!(previous_status, Some(Status::Settled | Status::Returned)) =>
        {
            Status::Returned
        }
        _ => return Err(Error::Pending),
    };
    if let Some(previous) = previous {
        if previous.status == next {
            return Ok(previous);
        }
        if today < previous.on {
            return Err(Error::Pending);
        }
    }
    Ok(Mark {
        status: next,
        on: today,
        settled_on: previous
            .filter(|_| next == Status::Returned)
            .map(|mark| mark.on),
    })
}

async fn blocking<T: Serialize + Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Response> {
    let payload = tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Storage)??;
    let body = serde_json::to_vec(&payload).map_err(|_| Error::Storage)?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(body),
    )
        .into_response())
}

fn router<P: Provider>(bridge: Arc<Bridge<P>>) -> Router {
    Router::new()
        .route(
            "/destinations/{vendor}",
            get(
                |State(bridge): State<Arc<Bridge<P>>>,
                 RoutePath(vendor): RoutePath<String>,
                 headers: HeaderMap| async move {
                    bridge.require_token(&headers)?;
                    let vendor = vendor.parse().map_err(|_| Error::Input)?;
                    blocking(move || bridge.destination(&vendor)).await
                },
            ),
        )
        .route(
            "/payments",
            post(
                |State(bridge): State<Arc<Bridge<P>>>, headers: HeaderMap, body: Bytes| async move {
                    bridge.require_token(&headers)?;
                    let payment: Payment =
                        serde_json::from_slice(&body).map_err(|_| Error::Input)?;
                    if headers
                        .get("Idempotency-Key")
                        .and_then(|key| key.to_str().ok())
                        != Some(payment.key.to_string().as_str())
                    {
                        return Err(Error::Mismatch);
                    }
                    blocking(move || bridge.submit(payment)).await
                },
            ),
        )
        .route(
            "/payments/{key}",
            get(
                |State(bridge): State<Arc<Bridge<P>>>,
                 RoutePath(key): RoutePath<String>,
                 headers: HeaderMap| async move {
                    bridge.require_token(&headers)?;
                    let key = key.parse().map_err(|_| Error::Input)?;
                    blocking(move || bridge.lookup(&key)).await
                },
            ),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
        .with_state(bridge)
}

fn secret(name: &str) -> Result<SecretString> {
    let value = env::var(name).map_err(|_| Error::Configuration)?;
    if !(16..=4096).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::Configuration);
    }
    Ok(value.into())
}

pub(crate) fn run() -> Result<()> {
    let mode = env::var("BK_WISE_BRIDGE_ENVIRONMENT");
    let live_acknowledgement = env::var("BK_WISE_BRIDGE_ENABLE_LIVE");
    let mode = match mode {
        Ok(ref value) => Some(value.as_str()),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => return Err(Error::Configuration),
    };
    let environment = Environment::configured(mode, live_acknowledgement.as_deref().ok())?;
    let config_path = env::var_os("BK_WISE_BRIDGE_CONFIG").ok_or(Error::Configuration)?;
    let store_path = env::var_os("BK_WISE_BRIDGE_STATE").ok_or(Error::Configuration)?;
    let listen: SocketAddr = env::var("BK_WISE_BRIDGE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8791".into())
        .parse()
        .map_err(|_| Error::Configuration)?;
    if !listen.ip().is_loopback() {
        return Err(Error::Configuration);
    }
    let config = Config::read(Path::new(&config_path))?;
    let store = Store::open(Path::new(&store_path), environment)?;
    let provider = Wise::new(environment, secret("BK_WISE_BRIDGE_TOKEN")?)?;
    provider.check_source(&config)?;
    let bridge = Arc::new(Bridge {
        environment,
        config,
        token: secret("BK_DISBURSEMENT_HOST_TOKEN")?,
        provider,
        store: Mutex::new(store),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| Error::Configuration)?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|_| Error::Configuration)?;
        axum::serve(listener, router(bridge))
            .await
            .map_err(|_| Error::Configuration)
    })
}
