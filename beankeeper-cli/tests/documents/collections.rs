//! Exercise a Stripe collection with mock Checkout and signed webhook receipts.

use std::{cell::Cell, error::Error, fs, path::Path};

use beankeeper::types::{Currency, Money, PositiveMoney};
use beankeeper_cli::{
    db::{
        Actor, Db,
        audit::payment_collection::{CollectionRequest, create_link},
    },
    payments::{
        checkout::{CheckoutError, CheckoutLink, CheckoutProvider},
        collection::CollectionTerms,
        stripe::{CollectionId, StripeMode},
    },
};
use chrono::Utc;
use ring::hmac;
use serde_json::{Value, json};

use super::{Books, TestResult};

const SECRET: &str = "webhook_fixture_secret";

struct CheckoutFixture {
    timeout: Cell<bool>,
    sequence: Cell<u8>,
}

impl CheckoutProvider for CheckoutFixture {
    fn create(&self, _: &CollectionTerms) -> Result<CheckoutLink, CheckoutError> {
        if self.timeout.replace(false) {
            return Err(CheckoutError::InFlight);
        }
        let index = self.sequence.get() + 1;
        self.sequence.set(index);
        Ok(CheckoutLink {
            session: format!("cs_test_{index}")
                .parse()
                .map_err(|_| CheckoutError::Response)?,
            url: format!("https://checkout.stripe.com/c/pay/example{index}")
                .parse()
                .map_err(|_| CheckoutError::Response)?,
        })
    }
}

fn collection(
    books: &Books,
    number: &str,
    amount: i128,
    provider: &CheckoutFixture,
) -> Result<(String, CollectionId), Box<dyn Error>> {
    let invoice = books.draft("invoice", number, 10_000)?;
    books.issue(&invoice, "2026-01-01")?;
    let db = Db::open_existing(&books.path, None)?;
    let actor = Actor::resolve(Some("test-collection"));
    let request = CollectionRequest {
        company: "acme".parse()?,
        invoice: invoice.parse()?,
        account: "acct_example".parse()?,
        mode: StripeMode::Sandbox,
        amount: PositiveMoney::new(Money::from_minor(amount, Currency::USD))?,
        request_key: number.parse()?,
        clearing_account: "1150".parse()?,
        as_of: "2026-01-02".parse()?,
    };
    if provider.timeout.get() {
        assert!(create_link(&db, &actor, &request, provider).is_err());
    }
    let link = create_link(&db, &actor, &request, provider)?;
    let replay = create_link(&db, &actor, &request, provider)?;
    assert_eq!(link.collection, replay.collection);
    assert_eq!(link.checkout.session, replay.checkout.session);
    Ok((invoice, link.collection))
}

fn send(
    books: &Books,
    spool: &Path,
    key: &str,
    body: &Value,
    authentic: bool,
) -> Result<Value, Box<dyn Error>> {
    let bytes = serde_json::to_vec(body)?;
    let stamp = Utc::now().timestamp();
    let mut message = format!("{stamp}.").into_bytes();
    message.extend_from_slice(&bytes);
    let secret = if authentic { SECRET } else { "untrusted" };
    let signing = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let signature = hmac::sign(&signing, &message);
    fs::write(spool.join(format!("{key}.json")), bytes)?;
    fs::write(
        spool.join(format!("{key}.sig")),
        format!("t={stamp},v1={}", hex::encode(signature.as_ref())),
    )?;
    let result = books
        .command()?
        .envs([
            ("BK_STRIPE_COMPANY", "acme"),
            ("BK_STRIPE_ACCOUNT", "acct_example"),
            ("BK_STRIPE_MODE", "sandbox"),
            ("BK_STRIPE_CONNECT", "false"),
            ("BK_STRIPE_SIGNING_SECRET", SECRET),
        ])
        .env("BK_STRIPE_SPOOL", spool)
        .args(["collection", "apply", key])
        .output()?;
    if !authentic {
        assert!(!result.status.success());
        return Ok(Value::Null);
    }
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output: Value = serde_json::from_slice(&result.stdout)?;
    Ok(output["data"].clone())
}

fn event(id: &str, kind: &str, object: &Value) -> Value {
    json!({"id":id,"type":kind,"created":1_767_657_600_i64,"livemode":false,"data":{"object":object}})
}

fn pay(books: &Books, spool: &Path, index: usize, link: &CollectionId) -> TestResult {
    let delivery = format!("receipt{index}");
    let event = event(
        &format!("evt_receipt{index}"),
        "checkout.session.completed",
        &json!({
            "id":format!("cs_test_{index}"), "client_reference_id":link,
            "mode":"payment", "payment_status":"paid", "amount_total":6000,"currency":"usd",
            "payment_intent":format!("pi_receipt{index}")
        }),
    );
    send(books, spool, &delivery, &event, false)?;
    let applied = send(books, spool, &delivery, &event, true)?;
    assert_eq!(applied["status"], "applied");
    assert_eq!(send(books, spool, &delivery, &event, true)?, applied);
    Ok(())
}

#[test]
fn authenticated_collection_groups_two_receipts_and_reopens_a_partial_refund() -> TestResult {
    let books = Books::new()?;
    books.data(&["account", "create", "1150", "Processor", "--type", "asset"])?;
    let spool = tempfile::tempdir()?;
    let provider = CheckoutFixture {
        timeout: Cell::new(true),
        sequence: Cell::new(0),
    };
    let (_invoice1, first) = collection(&books, "STRIPE-1", 6000, &provider)?;
    let (_, second) = collection(&books, "STRIPE-2", 6000, &provider)?;
    let payout = event(
        "evt_payout",
        "payout.paid",
        &json!({"id":"po_test","amount":9600,"currency":"usd","arrival_date":1_768_089_600_i64,"status":"paid"}),
    );
    let outside_order = send(&books, spool.path(), "payout", &payout, true)?;
    assert_eq!(outside_order["status"], "payout_recorded");
    pay(&books, spool.path(), 1, &first)?;
    pay(&books, spool.path(), 2, &second)?;
    let refund = event(
        "evt_refund",
        "refund.created",
        &json!({"id":"re_test", "payment_intent":"pi_receipt1","amount":2000,"currency":"usd","status":"succeeded"}),
    );
    assert_eq!(
        send(&books, spool.path(), "refund", &refund, true)?["status"],
        "refund_recorded"
    );
    let terms = json!({"refund":"re_test","booked_on":"2026-01-09"}).to_string();
    books.data(&["collection", "reconcile-refund", "--terms", &terms])?;
    assert_eq!(
        books.data(&["collection", "reconcile-refund", "--terms", &terms])?["existing"],
        true
    );
    verify_payout(&books)?;
    let aging = books.aging("2026-01-15")?;
    assert_eq!(super::row(&aging, "STRIPE-1")?["open_minor"], 6000);
    assert_eq!(super::row(&aging, "STRIPE-2")?["open_minor"], 4000);
    books.data(&["verify"])?;
    Ok(())
}

fn verify_payout(books: &Books) -> TestResult {
    let manifest = json!({"payout":"po_test", "payments":["pi_receipt1","pi_receipt2"],
        "refunds":["re_test"],"fees_minor":400,"bank_account":"1000","fee_account":"5000"})
    .to_string();
    let posting = books.data(&["collection", "reconcile-payout", "--manifest", &manifest])?;
    assert_eq!(posting["existing"], false);
    let repeated = books.data(&["collection", "reconcile-payout", "--manifest", &manifest])?;
    assert_eq!(repeated["transaction"], posting["transaction"]);
    let summary = books.data(&["collection", "payouts"])?;
    assert_eq!(summary[0]["net_minor"], 9600);
    assert_eq!(summary[0]["gross_minor"], 12000);
    assert_eq!(summary[0]["fees_minor"], 400);
    assert_eq!(summary[0]["refunds_minor"], 2000);
    Ok(())
}
