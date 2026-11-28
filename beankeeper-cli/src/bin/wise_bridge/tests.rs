use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::*;

#[derive(Default)]
struct FakeState {
    creates: AtomicUsize,
    funds: AtomicUsize,
    lose_create: bool,
    lose_funding: bool,
    status: Mutex<String>,
}

#[derive(Clone)]
struct Fake(Arc<FakeState>);

impl Fake {
    fn new(lose_create: bool, lose_funding: bool) -> Self {
        Self(Arc::new(FakeState {
            lose_create,
            lose_funding,
            status: Mutex::new("incoming_payment_waiting".into()),
            ..FakeState::default()
        }))
    }

    fn status(&self, value: &str) -> Result<()> {
        *self.0.status.lock().map_err(|_| Error::Storage)? = value.into();
        Ok(())
    }
}

const QUOTE: &str = "11144c35-9fe8-4c32-b7fd-d05c2a7734bf";

impl Provider for Fake {
    fn check_recipient(&self, _: WiseId, _: WiseId) -> Result<()> {
        Ok(())
    }

    fn quote(&self, _: &Config, _: WiseId, _: PositiveMinorUnits) -> Result<String> {
        Ok(QUOTE.into())
    }

    fn create(&self, order: &Order) -> Result<provider::Transfer> {
        self.0.creates.fetch_add(1, Ordering::SeqCst);
        if self.0.lose_create {
            return Err(Error::Provider);
        }
        response(order, "incoming_payment_waiting")
    }

    fn find(&self, order: &Order) -> Result<Option<provider::Transfer>> {
        let status = self.0.status.lock().map_err(|_| Error::Storage)?;
        response(order, &status).map(Some)
    }

    fn fund(&self, _: &Order, _: WiseId) -> Result<WiseId> {
        self.0.funds.fetch_add(1, Ordering::SeqCst);
        self.status("outgoing_payment_sent")?;
        if self.0.lose_funding {
            return Err(Error::Provider);
        }
        123_456.try_into()
    }
}

fn response(order: &Order, status: &str) -> Result<provider::Transfer> {
    serde_json::from_str(&json!({
        "id":101, "status":status, "business":order.profile,
        "targetAccount":order.recipient, "customerTransactionId":provider::key(&order.payment.key)?,
        "quoteUuid":order.quote, "sourceCurrency":"USD", "targetCurrency":"USD",
        "sourceValue":42, "targetValue":42, "type":"standard"
    }).to_string())
    .map_err(|_| Error::Provider)
}

fn fixture(p: &Path, fake: Fake) -> Result<(Bridge<Fake>, Payment)> {
    let vendor: CounterpartyId = "party_01j00000000000000000000000"
        .parse()
        .map_err(|_| Error::Input)?;
    let recipient = 2.try_into()?;
    let config = Config {
        company: "acme".parse().map_err(|_| Error::Input)?,
        cash_account: "1000".parse().map_err(|_| Error::Input)?,
        profile_id: 1.try_into()?,
        balance_id: 3.try_into()?,
        recipients: BTreeMap::from([(vendor.clone(), recipient)]),
    };
    let payment = Payment {
        key: "disburse_01j00000000000000000000001"
            .parse()
            .map_err(|_| Error::Input)?,
        destination: config.fingerprint(Environment::Sandbox, &vendor, recipient)?,
        company: config.company.clone(),
        cash_account: config.cash_account.clone(),
        vendor,
        currency: Currency::USD,
        amount_minor: 4200.try_into().map_err(|_| Error::Input)?,
    };
    let bridge = Bridge {
        environment: Environment::Sandbox,
        config,
        token: "synthetic-fixture-token".into(),
        provider: fake,
        store: Mutex::new(Store::open(p, Environment::Sandbox)?),
    };
    Ok((bridge, payment))
}

#[test]
fn durable_lookup_ignores_duplicate_submit_and_retains_return_history()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("bridge.db");
    let fake = Fake::new(false, false);
    let (bridge, payment) = fixture(&path, fake.clone())?;
    let mut changed = payment.clone();
    changed.destination = DestinationFingerprint(Sha256Hex::from_digest([7; 32]));
    assert!(matches!(bridge.submit(changed), Err(Error::Mismatch)));
    assert_eq!(fake.0.creates.load(Ordering::SeqCst), 0);
    let settled = bridge.submit(payment.clone())?;
    assert_eq!(settled.status, Status::Settled);
    drop(bridge);
    let (bridge, _) = fixture(&path, fake.clone())?;
    assert_eq!(bridge.submit(payment.clone())?.order_id, settled.order_id);
    fake.status("funds_refunded")?;
    let returned = bridge.lookup(&payment.key)?;
    assert_eq!(returned.status, Status::Returned);
    assert_eq!(returned.settled_on, Some(settled.on));
    assert_eq!(fake.0.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fake.0.funds.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn ambiguous_create_or_funding_never_reposts_after_restart()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for lose_create in [true, false] {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("bridge.db");
        let fake = Fake::new(lose_create, !lose_create);
        let (bridge, payment) = fixture(&path, fake.clone())?;
        assert!(matches!(
            bridge.submit(payment.clone()),
            Err(Error::Provider)
        ));
        drop(bridge);
        let (bridge, _) = fixture(&path, fake.clone())?;
        if lose_create {
            assert_eq!(bridge.lookup(&payment.key)?.status, Status::Accepted);
        } else {
            assert!(matches!(bridge.lookup(&payment.key), Err(Error::Pending)));
        }
        let _result = bridge.submit(payment);
        assert_eq!(fake.0.creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.0.funds.load(Ordering::SeqCst),
            usize::from(!lose_create)
        );
    }
    Ok(())
}

#[test]
fn host_authentication_rejects_incorrect_token()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (bridge, _) = fixture(&temp.path().join("state.db"), Fake::new(false, false))?;
    let mut headers = HeaderMap::new();
    assert!(matches!(
        bridge.require_token(&headers),
        Err(Error::Authorization)
    ));
    headers.insert(
        header::AUTHORIZATION,
        "Bearer synthetic-fixture-token".parse()?,
    );
    bridge.require_token(&headers)?;
    Ok(())
}

#[test]
fn live_requires_explicit_acknowledgement_and_separate_state()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        Environment::configured(None, Some("yes"))?,
        Environment::Sandbox
    );
    assert!(matches!(
        Environment::configured(Some("live"), None),
        Err(Error::Configuration)
    ));
    assert_eq!(
        Environment::configured(Some("live"), Some("yes"))?,
        Environment::Live
    );
    assert_eq!(Environment::Live.origin(), "https://api.wise.com/2026Q4/");
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("state.db");
    let (bridge, payment) = fixture(&path, Fake::new(false, false))?;
    let recipient = bridge.config.recipient(&payment.vendor)?;
    let live = bridge
        .config
        .fingerprint(Environment::Live, &payment.vendor, recipient)?;
    assert_ne!(live, payment.destination);
    drop(bridge);
    assert!(matches!(
        Store::open(&path, Environment::Live),
        Err(Error::Configuration)
    ));
    Ok(())
}
