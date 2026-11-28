use super::*;

use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

const DESTINATION: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const CHANGED_DESTINATION: &str =
    "2222222222222222222222222222222222222222222222222222222222222222";

#[derive(Default)]
struct HostState {
    stopped: AtomicBool,
    changed: AtomicBool,
    submissions: AtomicUsize,
    payment: Mutex<Option<Value>>,
}

struct Provider {
    address: SocketAddr,
    state: Arc<HostState>,
    worker: Option<JoinHandle<()>>,
}

impl Provider {
    fn start() -> Result<Self, Box<dyn Error>> {
        let socket = TcpListener::bind("127.0.0.1:0")?;
        let address = socket.local_addr()?;
        let state = Arc::new(HostState::default());
        let shared = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            for stream in socket.incoming().map_while(Result::ok) {
                if shared.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let _ = serve_payment(stream, &shared);
            }
        });
        Ok(Self {
            address,
            state,
            worker: Some(worker),
        })
    }

    fn configure(&self, command: &mut Command) {
        command
            .env("BK_DISBURSEMENT_HOST", format!("http://{}", self.address))
            .env("BK_DISBURSEMENT_HOST_TOKEN", "test-host-token");
    }

    fn return_payment(&self) -> TestResult {
        let mut slot = self
            .state
            .payment
            .lock()
            .map_err(|_| "fixture provider lock")?;
        let payment = slot.as_mut().ok_or("no submitted transfer")?;
        payment["status"] = json!("returned");
        payment["settled_on"] = payment["on"].clone();
        payment["on"] = json!("2026-03-01");
        Ok(())
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_payment(mut stream: TcpStream, state: &HostState) -> TestResult {
    use std::io::Read as _;

    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut input = BufReader::new(&stream);
    let mut request = String::new();
    input.read_line(&mut request)?;
    let mut size = 0;
    let mut authorized = false;
    let mut payment_key = None;
    loop {
        let mut header = String::new();
        if input.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.trim().split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "authorization" => authorized = value.trim() == "Bearer test-host-token",
                "content-length" => size = value.trim().parse()?,
                "idempotency-key" => payment_key = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    let mut body = vec![0; size];
    input.read_exact(&mut body)?;
    let mut fields = request.split_ascii_whitespace();
    let method = fields.next();
    let path = fields.next().unwrap_or_default();
    let (status, response) = if !authorized {
        ("401 Unauthorized", Value::Null)
    } else if method == Some("GET") && path.starts_with("/destinations/") {
        let fingerprint = if state.changed.load(Ordering::Relaxed) {
            CHANGED_DESTINATION
        } else {
            DESTINATION
        };
        ("200 OK", json!({"fingerprint":fingerprint}))
    } else if method == Some("POST") && path == "/payments" {
        let args: Value = serde_json::from_slice(&body)?;
        assert_eq!(payment_key.as_deref(), args["key"].as_str());
        assert_eq!(args["destination"], DESTINATION);
        let previous = state.submissions.fetch_add(1, Ordering::Relaxed);
        assert_eq!(previous, 0, "the same payment must never be sent twice");
        *state.payment.lock().map_err(|_| "fixture provider lock")? = Some(json!({
            "key": args["key"], "order_id":"fake-order-1", "status":"settled", "on":"2026-01-05",
            "company":args["company"], "cash_account":args["cash_account"],
            "amount_minor":args["amount_minor"], "currency":args["currency"], "destination":args["destination"]
        }));
        // The host has settled the payment, but its initial response never arrives.
        return Ok(());
    } else if method == Some("GET") && path.starts_with("/payments/") {
        state
            .payment
            .lock()
            .map_err(|_| "fixture provider lock")?
            .as_ref()
            .filter(|row| row["key"].as_str() == path.strip_prefix("/payments/"))
            .map_or(("404 Not Found", Value::Null), |row| {
                ("200 OK", row.clone())
            })
    } else {
        ("404 Not Found", Value::Null)
    };
    let response = response.to_string();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    )?;
    Ok(())
}

struct Books {
    directory: tempfile::TempDir,
    path: std::path::PathBuf,
    passphrase: std::path::PathBuf,
    db: beankeeper_cli::db::Db,
    provider: Provider,
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("disbursements.db");
        let passphrase = directory.path().join("passphrase");
        identity_ledger(&path, &passphrase)?;
        let db = beankeeper_cli::db::Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
        let books = Self {
            directory,
            path,
            passphrase,
            db,
            provider: Provider::start()?,
        };
        for (code, name, kind) in [
            ("2000", "Payables", "liability"),
            ("5000", "Purchases", "expense"),
        ] {
            books
                .cli(None)?
                .args(["account", "create", code, name, "--type", kind])
                .assert()
                .success();
        }
        books
            .cli(None)?
            .args(["account", "classify", "1000", "cash"])
            .assert()
            .success();
        Ok(books)
    }

    fn cli(&self, token: Option<&str>) -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&self.path)?;
        command
            .arg("--passphrase-file")
            .arg(&self.passphrase)
            .args(["--company", "acme", "--json"]);
        if let Some(token) = token {
            command.args(["--agent-token", token]);
        }
        self.provider.configure(&mut command);
        Ok(command)
    }

    fn credential(&self, name: &str, caps: &[&str]) -> Result<String, Box<dyn Error>> {
        let issued = proposal_identity(&self.db, name, caps)?;
        Ok(issued["token"].as_str().ok_or("missing token")?.to_owned())
    }

    fn client(&self, token: &str) -> Result<Client, Box<dyn Error>> {
        let mut command = bk(&self.path)?;
        command
            .arg("--passphrase-file")
            .arg(&self.passphrase)
            .args(["--agent-token", token, "mcp", "serve"]);
        self.provider.configure(&mut command);
        let mut client = Client::spawn(command)?;
        client.initialize()?;
        Ok(client)
    }
}

fn bill_and_draft(client: &mut Client) -> Result<Value, Box<dyn Error>> {
    let party = client.success(
        "counterparty_create",
        &json!({
           "company":"acme","name":"Test vendor","role":"vendor",
        }),
    )?;
    let draft = client.success("document_draft", &json!({
        "company":"acme","terms":{"kind":"bill","number":"BILL-100","counterparty_id":party["party"]["id"],
            "control_account":"2000","lines":[{"description":"Fixtures","quantity":"1","unit_price_minor":300,"account":"5000"}]}
    }))?;
    let bill = &draft["record"]["id"];
    client.success(
        "document_issue",
        &json!({"company":"acme","id":bill,"date":"2026-01-02"}),
    )?;
    Ok(
        json!({"company":"acme","terms":{"cash_account":"1000","bills":[{"document_id":bill,"amount_minor":200}]}}),
    )
}

fn failure(client: &mut Client, name: &str, arguments: &Value) -> Result<Value, Box<dyn Error>> {
    let reply = client.call(name, arguments)?;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    payload(&reply)
}

#[test]
fn payments_are_reviewed_and_a_lost_response_is_reconciled_once_after_restart() -> TestResult {
    let books = Books::new()?;
    let draft_token = books.credential(
        "fixture_drafter",
        &["ledger/read", "ledger/post", "ledger/disbursement-draft"],
    )?;
    let approve_token = books.credential(
        "fixture_reviewer",
        &["ledger/read", "ledger/disbursement-approve"],
    )?;
    let dispatch_token = books.credential(
        "fixture_dispatcher",
        &["ledger/read", "ledger/disbursement-dispatch"],
    )?;
    let mut drafter = books.client(&draft_token)?;
    let mut reviewer = books.client(&approve_token)?;
    let mut dispatcher = books.client(&dispatch_token)?;
    let terms = bill_and_draft(&mut drafter)?;
    let draft = drafter.success("disbursement_draft", &terms)?;
    let id = draft["id"].as_str().ok_or("missing payment ID")?;
    let choice = json!({"company":"acme","id":id});
    let approval = json!({"company":"acme","id":id,"destination":DESTINATION});
    assert_eq!(
        failure(&mut drafter, "disbursement_dispatch", &choice)?["code"],
        "FORBIDDEN"
    );
    assert_eq!(
        reviewer.success("disbursement_preview", &choice)?["destination"],
        DESTINATION
    );
    reviewer.success("disbursement_approve", &approval)?;
    let extra = drafter.success("disbursement_draft", &terms)?;
    assert_eq!(
        failure(
            &mut reviewer,
            "disbursement_approve",
            &json!({
                "company":"acme","id":extra["id"],"destination":DESTINATION
            })
        )?["code"],
        "VALIDATION"
    );

    let backup = books.directory.path().join("approved.bk-backup");
    let backup = backup.to_str().ok_or("non-UTF8 backup path")?;
    books
        .cli(None)?
        .args(["backup", "--output", backup])
        .assert()
        .success();

    assert_uncertain_attempt(&books, &dispatch_token, &mut dispatcher, id)?;
    drop(dispatcher);
    let mut dispatcher = books.client(&dispatch_token)?;
    let settled = dispatcher.success("disbursement_dispatch", &choice)?;
    assert_eq!(settled["state"]["status"], "settled");
    assert!(settled["settlement_posting"].is_null());
    assert_eq!(books.provider.state.submissions.load(Ordering::Relaxed), 1);

    assert_accounting(&books, &dispatch_token, &mut dispatcher, id)?;
    books
        .cli(None)?
        .args(["restore", "--input", backup])
        .assert()
        .success();
    let observed = dispatcher.success("disbursement_dispatch", &choice)?;
    assert_eq!(observed["state"]["status"], "returned");
    assert_eq!(books.provider.state.submissions.load(Ordering::Relaxed), 1);
    assert_no_recovery_dispatch(&books, &dispatch_token, id, backup)
}

fn assert_uncertain_attempt(
    books: &Books,
    dispatch_token: &str,
    dispatcher: &mut Client,
    id: &str,
) -> TestResult {
    let choice = json!({"company":"acme","id":id});
    books.provider.state.changed.store(true, Ordering::Relaxed);
    assert_eq!(
        failure(dispatcher, "disbursement_dispatch", &choice)?["code"],
        "VALIDATION"
    );
    assert_eq!(books.provider.state.submissions.load(Ordering::Relaxed), 0);
    books.provider.state.changed.store(false, Ordering::Relaxed);
    failure(dispatcher, "disbursement_dispatch", &choice)?;
    assert_eq!(
        dispatcher.success("disbursement_show", &choice)?["state"]["status"],
        "submission_uncertain"
    );
    let output = books
        .cli(Some(dispatch_token))?
        .args(["disbursement", "show", id])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(observed["data"]["state"]["status"], "submission_uncertain");
    Ok(())
}

fn assert_no_recovery_dispatch(
    books: &Books,
    dispatcher: &str,
    id: &str,
    backup: &str,
) -> TestResult {
    let target = books.directory.path().join("recovered.db");
    bk(&target)?
        .arg("--passphrase-file")
        .arg(&books.passphrase)
        .args(["recover", "--input", backup])
        .assert()
        .success();
    let mut command = bk(&target)?;
    books.provider.configure(&mut command);
    let output = command
        .arg("--passphrase-file")
        .arg(&books.passphrase)
        .args([
            "--agent-token",
            dispatcher,
            "--company",
            "acme",
            "--json",
            "disbursement",
            "dispatch",
            id,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(envelope["data"]["state"]["status"], "returned");
    assert_eq!(books.provider.state.submissions.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn concurrent_reviewers_cannot_double_reserve_the_same_bill() -> TestResult {
    let books = Books::new()?;
    let author = books.credential(
        "reservation_author",
        &["ledger/read", "ledger/post", "ledger/disbursement-draft"],
    )?;
    let reviewer = books.credential(
        "reservation_reviewer",
        &["ledger/read", "ledger/disbursement-approve"],
    )?;
    let mut client = books.client(&author)?;
    let terms = bill_and_draft(&mut client)?;
    let one = client.success("disbursement_draft", &terms)?;
    let two = client.success("disbursement_draft", &terms)?;
    let approve = |record: &Value| -> Result<Child, Box<dyn Error>> {
        let id = record["id"].as_str().ok_or("disbursement ID")?;
        Ok(books
            .cli(Some(&reviewer))?
            .args(["disbursement", "approve", id, "--destination", DESTINATION])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let one = approve(&one)?;
    let two = approve(&two)?;
    let outcomes = [one.wait_with_output()?, two.wait_with_output()?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| result.status.success())
            .count(),
        1
    );
    let failure = outcomes
        .iter()
        .find(|result| !result.status.success())
        .ok_or("one approval must fail")?;
    let details = String::from_utf8_lossy(&failure.stderr);
    assert!(details.contains("after existing reservations"), "{details}");
    assert_eq!(books.provider.state.submissions.load(Ordering::Relaxed), 0);
    Ok(())
}

fn assert_accounting(
    books: &Books,
    dispatch_token: &str,
    dispatcher: &mut Client,
    id: &str,
) -> TestResult {
    let choice = json!({"company":"acme","id":id});
    books
        .cli(None)?
        .args(["period", "close", "--through", "2026-01-31"])
        .assert()
        .success();
    failure(
        dispatcher,
        "disbursement_reconcile",
        &json!({"company":"acme","id":id,"date":"2026-01-05"}),
    )?;
    assert_eq!(
        dispatcher.success("disbursement_show", &choice)?["confirmed"]["on"],
        "2026-01-05"
    );
    let output = books
        .cli(Some(dispatch_token))?
        .args(["disbursement", "reconcile", id, "--date", "2026-02-01"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let booked: Value = serde_json::from_slice(&output.stdout)?;
    assert!(booked["data"]["settlement_posting"].is_number());
    assert_eq!(
        dispatcher.success(
            "disbursement_reconcile",
            &json!({
                "company":"acme","id":id,"date":"2026-02-01"
            })
        )?,
        booked["data"]
    );
    books.provider.return_payment()?;
    dispatcher.success("disbursement_dispatch", &choice)?;
    let return_request = json!({"company":"acme","id":id,"date":"2026-03-01"});
    let returned = dispatcher.success("disbursement_reconcile", &return_request)?;
    assert!(returned["return_posting"].is_number());
    assert_eq!(
        dispatcher.success("disbursement_reconcile", &return_request)?,
        returned
    );
    books.cli(None)?.arg("verify").assert().success();
    Ok(())
}
