//! Exercise a Stripe collection with mock Checkout and signed webhook receipts.

use std::{
    cell::Cell,
    error::Error,
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};

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

fn stage_delivery(
    spool: &Path,
    key: &str,
    body: &Value,
    authentic: bool,
    stamp: i64,
) -> TestResult {
    let bytes = serde_json::to_vec(body)?;
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
    Ok(())
}

fn delivery_command(books: &Books, spool: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = books.command()?;
    command
        .envs([
            ("BK_STRIPE_COMPANY", "acme"),
            ("BK_STRIPE_ACCOUNT", "acct_example"),
            ("BK_STRIPE_MODE", "sandbox"),
            ("BK_STRIPE_CONNECT", "false"),
            ("BK_STRIPE_SIGNING_SECRET", SECRET),
        ])
        .env("BK_STRIPE_SPOOL", spool);
    Ok(command)
}

fn delivery_result(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    Ok(envelope["data"].clone())
}

fn send(
    books: &Books,
    spool: &Path,
    key: &str,
    body: &Value,
    authentic: bool,
) -> Result<Value, Box<dyn Error>> {
    stage_delivery(spool, key, body, authentic, Utc::now().timestamp())?;
    let result = delivery_command(books, spool)?
        .args(["collection", "apply", key])
        .output()?;
    if authentic {
        delivery_result(&result)
    } else {
        assert!(!result.status.success());
        Ok(Value::Null)
    }
}

fn event(id: &str, kind: &str, object: &Value) -> Value {
    json!({"id":id,"type":kind,"created":1_767_657_600_i64,"livemode":false,"data":{"object":object}})
}

fn checkout_receipt(index: usize, link: &CollectionId) -> Value {
    event(
        &format!("evt_receipt{index}"),
        "checkout.session.completed",
        &json!({
            "id":format!("cs_test_{index}"), "client_reference_id":link,
            "mode":"payment", "payment_status":"paid", "amount_total":6000,"currency":"usd",
            "payment_intent":format!("pi_receipt{index}")
        }),
    )
}

fn pay(
    books: &Books,
    spool: &Path,
    index: usize,
    link: &CollectionId,
) -> Result<Value, Box<dyn Error>> {
    let delivery = format!("receipt{index}");
    let event = checkout_receipt(index, link);
    send(books, spool, &delivery, &event, false)?;
    let applied = send(books, spool, &delivery, &event, true)?;
    assert_eq!(applied["status"], "applied");
    assert_eq!(send(books, spool, &delivery, &event, true)?, applied);
    Ok(applied)
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

#[test]
fn restored_collection_keeps_audited_receipt_and_reuses_its_posting() -> TestResult {
    let books = Books::new()?;
    books.data(&["account", "create", "1150", "Processor", "--type", "asset"])?;
    let spool = tempfile::tempdir()?;
    let provider = CheckoutFixture {
        timeout: Cell::new(false),
        sequence: Cell::new(0),
    };
    let (_, id) = collection(&books, "STRIPE-RESTORE", 6000, &provider)?;
    let path = spool.path().join("pre-payment.bk-backup");
    let path = path.to_str().ok_or("backup path is not Unicode")?;
    books.data(&["backup", "--output", path])?;
    let payment = pay(&books, spool.path(), 1, &id)?;
    let restored = books.data(&["restore", "--input", path])?;
    let open = books.aging("2026-01-15")?;
    assert_eq!(super::row(&open, "STRIPE-RESTORE")?["open_minor"], 10_000);
    books.data(&["restore-undo", &restored["undo_id"].to_string()])?;
    let after = books.aging("2026-01-15")?;
    assert_eq!(super::row(&after, "STRIPE-RESTORE")?["open_minor"], 4000);
    assert_eq!(pay(&books, spool.path(), 1, &id)?, payment);

    let db = Db::open_existing(&books.path, None)?;
    let receipt: String = db.read().query_row(
        "SELECT after FROM bookkeeping_audit WHERE company_slug='acme'
           AND entity='collection_event' AND entity_id='evt_receipt1'",
        [],
        |row| row.get(0),
    )?;
    let receipt: Value = serde_json::from_str(&receipt)?;
    assert_eq!(receipt["collection"], id.to_string());
    assert_eq!(receipt["payment"], "pi_receipt1");
    assert_eq!(receipt["posting"], payment["detail"]);
    assert_eq!(books.data(&["verify"])?["status"], "healthy");
    Ok(())
}

#[test]
fn closed_period_receipt_requires_matching_fresh_evidence_and_posts_once() -> TestResult {
    let books = Books::new()?;
    books.data(&["account", "create", "1150", "Processor", "--type", "asset"])?;
    let spool = tempfile::tempdir()?;
    let provider = CheckoutFixture {
        timeout: Cell::new(false),
        sequence: Cell::new(0),
    };
    let (_, id) = collection(&books, "STRIPE-REVIEW", 6000, &provider)?;
    books.data(&["period", "close", "--through", "2026-01-06"])?;
    let receipt = checkout_receipt(1, &id);
    let pending = send(&books, spool.path(), "closed", &receipt, true)?;
    assert_eq!(pending["status"], "review");
    assert!(
        pending["detail"]
            .as_str()
            .ok_or("missing reason")?
            .contains("closed period")
    );
    assert_eq!(
        super::row(&books.aging("2026-01-15")?, "STRIPE-REVIEW")?["open_minor"],
        10_000
    );

    let mut mismatch = receipt.clone();
    mismatch["id"] = json!("evt_amount_mismatch");
    mismatch["data"]["object"]["amount_total"] = json!(5999);
    assert_eq!(
        send(&books, spool.path(), "unmatched", &mismatch, true)?["status"],
        "review"
    );
    assert_resolution_rejected(&books, spool.path(), "unmatched", "2026-01-08")?;
    mismatch["id"] = receipt["id"].clone();
    stage_delivery(
        spool.path(),
        "altered",
        &mismatch,
        true,
        Utc::now().timestamp(),
    )?;
    assert_resolution_rejected(&books, spool.path(), "altered", "2026-01-08")?;
    stage_delivery(
        spool.path(),
        "expired",
        &receipt,
        true,
        Utc::now().timestamp() - 301,
    )?;
    assert_resolution_rejected(&books, spool.path(), "expired", "2026-01-08")?;
    assert_resolution_rejected(&books, spool.path(), "closed", "2026-01-05")?;
    assert_resolution_rejected(&books, spool.path(), "closed", "2026-01-06")?;

    stage_delivery(
        spool.path(),
        "fresh",
        &receipt,
        true,
        Utc::now().timestamp(),
    )?;
    let result = resolve_concurrently(&books, spool.path())?;
    assert_eq!(result["status"], "applied");
    assert_eq!(send(&books, spool.path(), "fresh", &receipt, true)?, result);
    assert_resolution_rejected(&books, spool.path(), "fresh", "2026-01-09")?;
    assert_eq!(
        super::row(&books.aging("2026-01-15")?, "STRIPE-REVIEW")?["open_minor"],
        4000
    );
    let exceptions = books.data(&["collection", "exceptions"])?;
    let original = exceptions
        .as_array()
        .ok_or("no exception list")?
        .iter()
        .find(|exception| exception["event"] == "evt_receipt1")
        .ok_or("missing original review")?;
    assert_eq!(original["reason"], pending["detail"]);
    assert_eq!(original["resolved_posting"], result["detail"]);
    assert_eq!(original["booked_on"], "2026-01-08");
    assert_eq!(books.data(&["verify"])?["status"], "healthy");
    Ok(())
}

fn assert_resolution_rejected(books: &Books, spool: &Path, name: &str, date: &str) -> TestResult {
    let rejected = delivery_command(books, spool)?
        .args(["collection", "resolve-receipt", name, "--booked-on", date])
        .output()?;
    assert_eq!(rejected.status.code(), Some(3), "{rejected:?}");
    Ok(())
}

fn resolve_concurrently(books: &Books, spool: &Path) -> Result<Value, Box<dyn Error>> {
    let start = || -> Result<_, Box<dyn Error>> {
        Ok(delivery_command(books, spool)?
            .args([
                "collection",
                "resolve-receipt",
                "fresh",
                "--booked-on",
                "2026-01-08",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let first = start()?;
    let second = start()?;
    let first = delivery_result(&first.wait_with_output()?)?;
    let second = delivery_result(&second.wait_with_output()?)?;
    assert_eq!(first, second);
    Ok(first)
}

fn report_evidence(books: &Books, evidence: &Value) -> Result<Value, Box<dyn Error>> {
    books.data(&[
        "collection",
        "report-evidence",
        "--evidence",
        &evidence.to_string(),
    ])
}

fn rejected_report(books: &Books, evidence: &Value) -> TestResult {
    let result = books
        .command()?
        .args([
            "collection",
            "report-evidence",
            "--evidence",
            &evidence.to_string(),
        ])
        .output()?;
    assert!(!result.status.success());
    Ok(())
}

fn facts(date: &str, amount_minor: i64) -> Value {
    json!({"account":"acct_example","live":false,"date":date,"currency":"USD","amount_minor":amount_minor})
}

fn external_payment(books: &Books, number: &str) -> Result<(String, Value), Box<dyn Error>> {
    books.data(&["account", "create", "1150", "Processor", "--type", "asset"])?;
    let invoice = books.draft("invoice", number, 10_000)?;
    books.issue(&invoice, "2026-01-01")?;
    let evidence = json!({
        "kind":"payment","facts":facts("2026-01-06",6000),"payment":"pi_agent",
        "target":{"type":"invoice","invoice":invoice,"clearing_account":"1150"},
    });
    Ok((invoice, evidence))
}

#[test]
fn agent_evidence_reuses_posting_checks_and_requires_a_bank_match() -> TestResult {
    let books = Books::new()?;
    let (_invoice, evidence) = external_payment(&books, "AGENT-STRIPE")?;
    let applied = report_evidence(&books, &evidence)?;
    assert_eq!(applied["status"], "payment_reported");
    assert_eq!(report_evidence(&books, &evidence)?, applied);
    let mut conflict = evidence.clone();
    conflict["facts"]["amount_minor"] = json!(7000);
    rejected_report(&books, &conflict)?;

    let refund = json!({"kind":"refund","facts":facts("2026-01-07",1000),"refund":"re_agent","payment":"pi_agent"});
    assert_eq!(
        report_evidence(&books, &refund)?["status"],
        "refund_recorded"
    );
    let refund_terms = json!({"refund":"re_agent","booked_on":"2026-01-08"}).to_string();
    books.data(&["collection", "reconcile-refund", "--terms", &refund_terms])?;
    let aging = books.aging("2026-01-15")?;
    assert_eq!(super::row(&aging, "AGENT-STRIPE")?["open_minor"], 5000);
    let mut wrong_account = refund.clone();
    wrong_account["refund"] = json!("re_wrong");
    wrong_account["facts"]["account"] = json!("acct_wrong");
    rejected_report(&books, &wrong_account)?;
    let mut excess = refund.clone();
    excess["refund"] = json!("re_excess");
    excess["facts"]["amount_minor"] = json!(5500);
    report_evidence(&books, &excess)?;
    let excess_terms = json!({"refund":"re_excess","booked_on":"2026-01-08"}).to_string();
    let rejected = books
        .command()?
        .args(["collection", "reconcile-refund", "--terms", &excess_terms])
        .output()?;
    assert!(
        !rejected.status.success(),
        "cumulative refunds must not exceed the receipt"
    );

    let disputed = json!({"kind":"dispute","facts":facts("2026-01-08",1000),
        "dispute":"dp_agent","payment":"pi_agent","movement":"withdrawn"});
    assert_eq!(
        report_evidence(&books, &disputed)?["status"],
        "dispute_recorded"
    );
    assert_eq!(
        books.data(&["collection", "disputes"])?[0]["source"],
        "agent_reported"
    );

    complete_agent_payout(&books)?;
    books.data(&["verify"])?;
    Ok(())
}

fn complete_agent_payout(books: &Books) -> TestResult {
    let payout = json!({"kind":"payout","facts":facts("2026-01-09",4600),"payout":"po_agent","fees_minor":400});
    assert_eq!(
        report_evidence(books, &payout)?["status"],
        "payout_recorded"
    );
    assert_eq!(report_evidence(books, &payout)?["status"], "duplicate");
    let manifest = json!({"payout":"po_agent","payments":["pi_agent"],"refunds":["re_agent"],
        "fees_minor":400,"bank_account":"1000","fee_account":"5000"})
    .to_string();
    let mut wrong_fee: Value = serde_json::from_str(&manifest)?;
    wrong_fee["fees_minor"] = json!(500);
    let wrong = books
        .command()?
        .args([
            "collection",
            "reconcile-payout",
            "--manifest",
            &wrong_fee.to_string(),
        ])
        .output()?;
    assert!(
        !wrong.status.success(),
        "selected fees must match agent evidence"
    );
    let posted = books.data(&["collection", "reconcile-payout", "--manifest", &manifest])?;
    let repeat = books.data(&["collection", "reconcile-payout", "--manifest", &manifest])?;
    assert_eq!(repeat["transaction"], posted["transaction"]);
    assert_eq!(repeat["existing"], true);
    let payouts = books.data(&["collection", "payouts"])?;
    assert_eq!(payouts[0]["source"], "agent_reported");
    assert_eq!(payouts[0]["bank_confirmed"], false);
    assert_eq!(
        books.data(&["collection", "exceptions"])?[0]["payout"],
        "po_agent"
    );

    let transaction = posted["transaction"].to_string();
    let view = books.data(&["txn", "show", &transaction])?;
    let debit = view[0]["entries"]
        .as_array()
        .ok_or("missing payout entries")?
        .iter()
        .find(|entry| entry["account_code"] == "1000")
        .ok_or("missing bank debit")?;
    books.data(&[
        "txn",
        "clear",
        &transaction,
        "--entry",
        &debit["id"].to_string(),
        "--date",
        "2026-01-09",
        "--statement",
        "bank-jan",
    ])?;
    assert_eq!(
        books.data(&["collection", "payouts"])?[0]["bank_confirmed"],
        true
    );
    assert_eq!(books.data(&["collection", "exceptions"])?, json!([]));

    let second = json!({"kind":"payout","facts":facts("2026-01-10",4600),"payout":"po_second","fees_minor":400});
    report_evidence(books, &second)?;
    let duplicate_group = manifest.replace("po_agent", "po_second");
    let result = books
        .command()?
        .args([
            "collection",
            "reconcile-payout",
            "--manifest",
            &duplicate_group,
        ])
        .output()?;
    assert!(
        !result.status.success(),
        "a payment can only settle in one payout"
    );
    Ok(())
}

#[test]
fn signed_delivery_upgrades_agreement_and_retains_conflicts_without_reposting() -> TestResult {
    let books = Books::new()?;
    let (_, evidence) = external_payment(&books, "COMPARE-STRIPE")?;
    let applied = report_evidence(&books, &evidence)?;
    let collection = applied["detail"]["collection"]
        .as_str()
        .ok_or("collection ID")?;
    let spool = tempfile::tempdir()?;
    let expected = |delivery, amount| {
        event(
            delivery,
            "payment_intent.succeeded",
            &json!({"id":"pi_agent","status":"succeeded","amount_received":amount,"currency":"usd"}),
        )
    };
    let conflict = expected("evt_conflict", 7100);
    assert_eq!(
        send(&books, spool.path(), "conflict", &conflict, true)?["status"],
        "review"
    );
    assert_eq!(
        books.data(&["collection", "exceptions"])?[0]["evidence_key"],
        "pi_agent"
    );
    let exact = expected("evt_confirms", 6000);
    assert_eq!(
        send(&books, spool.path(), "confirms", &exact, true)?["status"],
        "duplicate"
    );
    assert_eq!(
        send(&books, spool.path(), "confirms", &exact, true)?["status"],
        "duplicate"
    );
    let view = books.data(&["collection", "show", collection])?;
    assert_eq!(view["payment"]["source"], "signed");
    assert_eq!(view["payment"]["original_source"], "agent_reported");
    assert_eq!(view["payment"]["posting"], applied["detail"]["transaction"]);
    assert_eq!(
        super::row(&books.aging("2026-01-15")?, "COMPARE-STRIPE")?["open_minor"],
        4000
    );
    books.data(&["verify"])?;
    Ok(())
}
