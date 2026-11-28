//! Exercise the real Streamable HTTP endpoint and request-scoped credentials.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, Admin, Granted, IssueRequest};
use beankeeper_cli::ledger_target::{LedgerPath, NamedLedgerTarget};
use reqwest::blocking::{Client, RequestBuilder, Response};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

#[path = "support/legacy_schema.rs"]
mod legacy_schema;

#[path = "support/mcp_host_owned.rs"]
mod host_owned;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const PASSPHRASE: &str = "http-test-passphrase";

struct Ledger {
    dir: TempDir,
    path: PathBuf,
    secret: PathBuf,
    reader: Value,
    writer: Value,
    admin: Value,
    owner: Actor,
}

impl Ledger {
    fn new() -> TestResult<Self> {
        let ledger = Self::without_secret_file()?;
        std::fs::write(&ledger.secret, PASSPHRASE)?;
        Ok(ledger)
    }

    fn without_secret_file() -> TestResult<Self> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let secret = dir.path().join("passphrase");
        let db = Db::open(&path, Some(&PASSPHRASE.into()))?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "4000",
                    name: "Sales",
                    account_type: beankeeper::types::AccountType::Revenue,
                    default_tax_category: None,
                },
            )
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), identity::init_root)?;
        let issue = |name: &str, capabilities: &[String]| {
            Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), |grant| {
                identity::issue(
                    grant,
                    &IssueRequest {
                        name,
                        capabilities,
                        ttl: Duration::from_hours(1),
                        public_key: None,
                        generate_key: true,
                    },
                )
            })
        };
        let reader = issue("http_reader", &["ledger/read".into()])?;
        let writer = issue("http_writer", &["ledger/read".into(), "ledger/post".into()])?;
        let admin = issue("http_owner", &["ledger/admin".into()])?;
        let owner = Actor::new("owner".parse()?)
            .with_identity(identity::verify(db.read(), token(&admin)?)?);
        Ok(Self {
            dir,
            path,
            secret,
            reader,
            writer,
            admin,
            owner,
        })
    }

    fn db(&self) -> TestResult<Db> {
        Ok(Db::open_existing(&self.path, Some(&PASSPHRASE.into()))?)
    }

    fn rewind_before_restore(&self) -> TestResult {
        let raw = rusqlite::Connection::open(&self.path)?;
        raw.pragma_update(None, "key", PASSPHRASE)?;
        legacy_schema::before_restore(&raw)?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
        command
            .env_remove("BK_AGENT_TOKEN")
            .env_remove("BK_ACTOR")
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_COMPANY")
            .arg("--db")
            .arg(&self.path)
            .arg("--passphrase-file")
            .arg(&self.secret);
        command
    }

    fn enforce(&self) -> TestResult {
        Granted::<Admin>::with_db(&self.db()?, &self.owner, |grant| {
            db::audit::set_policy(grant, true)
        })?;
        Ok(())
    }

    fn revoke(&self, target: &str) -> TestResult {
        Granted::<Admin>::with_db(&self.db()?, &self.owner, |grant| {
            identity::revoke(grant, target)
        })?;
        Ok(())
    }
}

#[test]
fn http_startup_publishes_an_encrypted_backup_before_upgrading() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.rewind_before_restore()?;

    let mut server = Server::start(&ledger, true, None)?;
    let reader = Some(token(&ledger.reader)?);
    server.initialize(reader, "schema upgrade")?;
    let response = server.call("company_list", json!({}), reader)?;
    assert!(response.get("error").is_none(), "{response}");
    let upgrade = &response["result"]["_meta"][beankeeper_cli::mcp::LEDGER_UPGRADE_META_KEY];
    assert_eq!(
        db::get_schema_version(ledger.db()?.read())?,
        i64::from(db::current_schema_version())
    );

    let backup_dir = ledger
        .dir
        .path()
        .read_dir()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".bk-pre-migration-")
        })
        .ok_or("missing pre-migration backup")?;
    let backup_path = backup_dir.path().join("books.bk-backup");
    assert_eq!(
        upgrade,
        &json!({"from": 37, "to": db::current_schema_version(),
                "backup": backup_path.canonicalize()?})
    );
    let response = server.call("company_list", json!({}), reader)?;
    assert_eq!(
        response["result"]["_meta"][beankeeper_cli::mcp::LEDGER_UPGRADE_META_KEY],
        *upgrade
    );
    let key = SecretString::from(PASSPHRASE);
    assert!(Db::open_snapshot(&backup_path, None).is_err());
    assert!(matches!(
        Db::open_snapshot(&backup_path, Some(&key)),
        Err(beankeeper_cli::error::CliError::SchemaMismatch { stored, .. })
        if i64::from(stored) == 37
    ));
    Ok(())
}

struct Server {
    child: Child,
    endpoint: String,
    diagnostics: mpsc::Receiver<String>,
    client: Client,
    session: Option<String>,
    next_id: u64,
}

impl Server {
    fn start(ledger: &Ledger, read_only: bool, fallback: Option<&str>) -> TestResult<Self> {
        Self::start_at(ledger, read_only, fallback, "127.0.0.1:0", false)
    }

    fn start_at(
        ledger: &Ledger,
        read_only: bool,
        fallback: Option<&str>,
        bind: &str,
        allow_remote: bool,
    ) -> TestResult<Self> {
        let mut command = ledger.command();
        if let Some(token) = fallback {
            command.env("BK_AGENT_TOKEN", token);
        }
        command.args(["mcp", "serve", "--http", "--bind", bind]);
        if allow_remote {
            command.arg("--allow-remote");
        }
        if read_only {
            command.arg("--read-only");
        }
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> TestResult<Self> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut server = Self {
            child,
            endpoint: String::new(),
            diagnostics: mpsc::channel().1,
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .no_proxy()
                .build()?,
            session: None,
            next_id: 0,
        };
        let stderr = server.child.stderr.take().ok_or("missing stderr")?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        server.diagnostics = receiver;
        let mut startup_log = Vec::new();
        while server.endpoint.is_empty() {
            let line = server
                .diagnostics
                .recv_timeout(Duration::from_secs(20))
                .map_err(|error| format!("HTTP startup failed ({error}): {startup_log:?}"))?;
            if let Some(endpoint) = line.strip_prefix("MCP Streamable HTTP listening on ") {
                server.endpoint = endpoint.to_string();
            } else {
                startup_log.push(line);
            }
        }
        assert!(!server.endpoint.ends_with(":0/mcp"));
        // Reach a wildcard listener through the local IPv4 interface.
        server.endpoint = server.endpoint.replace("0.0.0.0:", "127.0.0.1:");
        Ok(server)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        token: Option<&str>,
    ) -> TestResult<Response> {
        self.next_id += 1;
        let mut body = json!({"jsonrpc":"2.0", "id":self.next_id, "method":method});
        body["params"] = params;
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&body);
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(request.send()?)
    }

    fn initialize(&mut self, token: Option<&str>, name: &str) -> TestResult {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":name,"version":"1.0"}
            }),
            token,
        )?;
        assert!(response.status().is_success());
        self.session = Some(
            response
                .headers()
                .get("Mcp-Session-Id")
                .ok_or("missing session")?
                .to_str()?
                .to_string(),
        );
        assert_eq!(rpc(response)?["result"]["serverInfo"]["name"], "beankeeper");
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .header(
                "Mcp-Session-Id",
                self.session.as_deref().ok_or("missing session")?,
            )
            .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert!(request.send()?.status().is_success());
        Ok(())
    }

    fn call(&mut self, name: &str, args: Value, token: Option<&str>) -> TestResult<Value> {
        let mut params = json!({"name": name});
        params["arguments"] = args;
        rpc(self.request("tools/call", params, token)?)
    }

    fn tools(&mut self, token: Option<&str>) -> TestResult<Vec<String>> {
        let response = rpc(self.request("tools/list", json!({}), token)?)?;
        response["result"]["tools"]
            .as_array()
            .ok_or("missing tools")?
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "missing name".into())
            })
            .collect()
    }

    fn finish(mut self, tokens: &[&str]) -> TestResult {
        self.child.kill()?;
        self.child.wait()?;
        let mut stdout = String::new();
        std::io::Read::read_to_string(&mut self.child.stdout.take().ok_or("stdout")?, &mut stdout)?;
        assert!(stdout.is_empty(), "unexpected stdout: {stdout}");
        for line in &self.diagnostics {
            for token in tokens {
                assert!(!line.contains(token));
            }
            assert!(!line.contains(PASSPHRASE));
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rpc(response: Response) -> TestResult<Value> {
    assert!(response.status().is_success(), "HTTP {}", response.status());
    let body = response.text()?;
    if let Ok(value) = serde_json::from_str(&body) {
        return Ok(value);
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim())
            && value.get("id").is_some()
        {
            return Ok(value);
        }
    }
    Err(format!("missing JSON-RPC result: {body}").into())
}

fn payload(response: &Value) -> TestResult<Value> {
    if let Some(value) = response["result"].get("structuredContent") {
        return Ok(value.clone());
    }
    Ok(serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing content")?,
    )?)
}

fn token(issued: &Value) -> TestResult<&str> {
    issued["token"]
        .as_str()
        .ok_or_else(|| "missing token".into())
}

fn posting(reference: &str) -> Value {
    json!({"company":"acme", "description":"HTTP sale", "date":"2026-11-20",
        "reference":reference, "on_conflict":"skip",
        "debits":[{"account":"1000","amount":"12.50"}],
        "credits":[{"account":"4000","amount":"12.50"}]})
}

#[test]
fn http_authenticates_every_request_and_preserves_ledger_behavior() -> TestResult {
    let ledger = Ledger::new()?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, false, None)?;
    for credential in [None, Some("invalid-secret-token")] {
        let response = server.request("initialize", json!({}), credential)?;
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        let body = response.text()?;
        assert_eq!(
            serde_json::from_str::<Value>(&body)?["code"],
            "UNAUTHORIZED"
        );
        assert!(!body.contains("invalid-secret-token"));
    }
    server.initialize(Some(reader), "HTTP Client/One")?;
    assert!(
        server
            .tools(Some(reader))?
            .iter()
            .any(|name| name == "txn_post")
    );
    let response = server.call("company_list", json!({}), Some(reader))?;
    assert_eq!(payload(&response)?[0]["slug"], "acme");
    // Policy off permits writes with a verified read token, as on stdio.
    let first = server.call("txn_post", posting("http-sale"), Some(reader))?;
    assert_ne!(first["result"]["isError"], true);
    assert_eq!(payload(&first)?["created"], true);
    let retry = server.call("txn_post", posting("http-sale"), Some(reader))?;
    assert_eq!(payload(&retry)?["skipped"], true);
    let db = ledger.db()?;
    let (actor, agent): (String, String) = db.read().query_row(
        "SELECT posted_by, agent_uri FROM transactions WHERE reference = 'http-sale'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(actor, ledger.reader["agent_uri"]);
    assert_eq!(agent, ledger.reader["agent_uri"]);
    db::audit::verify_hash_chain(db.read())?;
    db::audit::verify_creations(db.read())?;
    // A session ID alone is never a credential, on any HTTP method.
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let response = server
            .client
            .request(method, &server.endpoint)
            .header(
                "Mcp-Session-Id",
                server.session.as_deref().ok_or("session")?,
            )
            .send()?;
        assert_eq!(response.status(), 401);
    }
    ledger.revoke(ledger.reader["token_jti"].as_str().ok_or("jti")?)?;
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(reader))?
            .status(),
        401
    );
    server.finish(&[reader, "invalid-secret-token"])
}

#[test]
fn stateless_http_preserves_the_configured_label_and_request_agent_without_client_info()
-> TestResult {
    let ledger = Ledger::new()?;
    let writer = token(&ledger.writer)?;
    let mut command = ledger.command();
    command.args([
        "mcp",
        "serve",
        "--http",
        "--bind",
        "127.0.0.1:0",
        "--actor",
        "shared-books",
    ]);
    let server = Server::spawn(command)?;
    let call = |name, arguments| {
        // Stateless MCP sends tool calls without a clientInfo handshake.
        server
            .client
            .post(&server.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", name)
            .bearer_auth(writer)
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":name, "arguments":arguments, "_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }}}))
            .send()
    };
    let result = rpc(call("txn_post", posting("stateless-sale"))?)?;
    assert_eq!(payload(&result)?["created"], true);
    let agent = &ledger.writer["agent_uri"];
    for filter in [json!({"actor":"shared-books"}), json!({"agent":agent})] {
        let result = rpc(call("audit_log", filter)?)?;
        let rows = payload(&result)?;
        assert_eq!(rows.as_array().map(Vec::len), Some(1));
        assert_eq!(rows[0]["actor"], "shared-books");
        assert_eq!(rows[0]["agent_uri"], *agent);
        assert_eq!(rows[0]["token_jti"], ledger.writer["token_jti"]);
    }
    let db = ledger.db()?;
    db::audit::verify_hash_chain(db.read())?;
    db::audit::verify_creations(db.read())?;
    server.finish(&[writer])
}

#[test]
fn http_enforces_current_request_capabilities_and_policy() -> TestResult {
    let ledger = Ledger::new()?;
    let reader = token(&ledger.reader)?;
    let writer = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(writer), "Shared Client")?;
    ledger.enforce()?;
    assert!(server.tools(Some(writer))?.contains(&"txn_post".into()));
    assert!(!server.tools(Some(reader))?.contains(&"txn_post".into()));
    let denied = server.call("txn_post", posting("denied"), Some(reader))?;
    assert_eq!(denied["result"]["isError"], true);
    assert_eq!(payload(&denied)?["code"], "FORBIDDEN");
    let allowed = server.call("txn_post", posting("allowed"), Some(writer))?;
    assert_eq!(payload(&allowed)?["created"], true);
    let db = ledger.db()?;
    let agent: String = db.read().query_row(
        "SELECT agent_uri FROM transactions WHERE reference = 'allowed'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(agent, ledger.writer["agent_uri"]);
    let count: i64 = db
        .read()
        .query_row("SELECT count(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    server.finish(&[reader, writer])
}

#[test]
fn http_resolves_the_posting_mode_of_each_request() -> TestResult {
    use beankeeper_cli::identity::WriteMode;
    use secrecy::ExposeSecret as _;

    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let proposer = Granted::<Admin>::with_db(&db, &ledger.owner, |grant| {
        identity::issue_credential_with_mode(
            grant,
            &IssueRequest {
                name: "http_proposer",
                capabilities: &["ledger/read".into(), "ledger/post".into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
            WriteMode::ProposeOnly,
        )
    })?;
    ledger.enforce()?;
    let propose_only = proposer.token().expose_secret();
    let direct = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(direct), "Bookkeeper Client")?;
    let tools = server.tools(Some(propose_only))?;
    assert!(tools.contains(&"txn_post".into()));
    assert!(!tools.contains(&"txn_approve".into()));
    let pending = payload(&server.call("txn_post", posting("queued"), Some(propose_only))?)?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["proposer_uri"], proposer.agent_uri.as_str());
    let posted = payload(&server.call("txn_post", posting("posted"), Some(direct))?)?;
    assert_eq!(posted["created"], true);
    let transactions =
        payload(&server.call("txn_list", json!({"company":"acme"}), Some(direct))?)?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(1));
    assert_eq!(transactions[0]["id"], posted["id"]);
    server.finish(&[direct, propose_only])
}

const BOOKKEEPING_TOOLS: [&str; 4] = [
    "counterparty_create",
    "counterparty_associate",
    "project_create",
    "project_split",
];

#[test]
fn http_bookkeeping_catalog_exposes_only_authorized_tools() -> TestResult {
    let ledger = Ledger::new()?;
    let direct = token(&ledger.writer)?;
    let read = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(direct), "Agent")?;

    for enforced in [false, true] {
        if enforced {
            ledger.enforce()?;
        }
        let direct_tools = server.tools(Some(direct))?;
        let read_tools = server.tools(Some(read))?;
        for name in BOOKKEEPING_TOOLS {
            assert!(direct_tools.iter().any(|tool| tool == name), "{name}");
            assert!(!read_tools.iter().any(|tool| tool == name), "{name}");
        }
        assert!(!direct_tools.iter().any(|tool| tool == "company_delete"));
        let denied = server.call(
            "counterparty_create",
            json!({"company":"acme","name":"Customer","role":"customer"}),
            Some(read),
        )?;
        assert_eq!(payload(&denied)?["code"], "FORBIDDEN");
    }

    server.finish(&[direct, read])
}

#[test]
fn http_agent_can_apply_customer_project_income_and_expenses() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    Granted::<Admin>::with_db(&db, &ledger.owner, |grant| {
        db::create_account(
            grant,
            &CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "5000",
                name: "Hosting",
                account_type: beankeeper::types::AccountType::Expense,
                default_tax_category: None,
            },
        )
    })?;
    let direct = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(direct), "Agent")?;

    ledger.enforce()?;

    let mut call = |tool: &str, args: Value| -> TestResult<Value> {
        let result = server.call(tool, args, Some(direct))?;
        assert_ne!(result["result"]["isError"], true, "{tool}: {result}");
        payload(&result)
    };
    let customer = call(
        "counterparty_create",
        json!({"company":"acme","name":"Customer","role":"customer"}),
    )?;
    let vendor = call(
        "counterparty_create",
        json!({"company":"acme","name":"Hosting vendor","role":"vendor"}),
    )?;
    let project = call(
        "project_create",
        json!({"company":"acme","terms":{
            "code":"contract","name":"Contract","customer":customer["party"]["id"],"currency":"USD"
        }}),
    )?;
    let project_id = &project["project"]["id"];
    assert!(!project_id.is_null(), "{project}");
    for index in 0..7 {
        let (expense, expense_minor) = if index == 6 {
            ("159.41", 15941)
        } else {
            ("200.00", 20000)
        };
        for (kind, party, journal) in [
            (
                "income",
                &customer["party"]["id"],
                ("1000", "4000", "1500.00", "4000", 150_000),
            ),
            (
                "expense",
                &vendor["party"]["id"],
                ("5000", "1000", expense, "5000", expense_minor),
            ),
        ] {
            let (debit, credit, amount, operating_account, amount_minor) = journal;
            let posted = call(
                "txn_post",
                json!({
                    "company":"acme","date":"2026-11-20","description":kind,"reference":format!("{kind}-{index}"),
                    "debits":[{"account":debit,"amount":amount}],"credits":[{"account":credit,"amount":amount}],
                }),
            )?;
            call(
                "counterparty_associate",
                json!({
                    "company":"acme","id":party,"transaction_id":posted["id"]
                }),
            )?;
            let transaction = call("txn_show", json!({"company":"acme","id":posted["id"]}))?;
            let entry = transaction[0]["entries"]
                .as_array()
                .ok_or_else(|| format!("missing entries in {transaction}"))?
                .iter()
                .find(|entry| entry["account_code"] == operating_account)
                .ok_or("missing income or expense entry")?;
            call(
                "project_split",
                json!({
                    "company":"acme","entry":entry["id"],"date":"2026-11-20",
                    "splits":[{"project":project_id,"amount_minor":amount_minor}],
                }),
            )?;
        }
    }
    let report = call(
        "project_report",
        json!({"company":"acme","month":"2026-11","currency":"USD"}),
    )?;
    assert_eq!(report["projects"][0]["revenue_minor"], 1_050_000);
    assert_eq!(report["projects"][0]["expense_minor"], 135_941);
    assert_eq!(report["projects"][0]["profit_minor"], 914_059);
    server.finish(&[direct])
}

#[test]
fn http_agent_bookkeeping_proposals_need_approval_before_changes_apply() -> TestResult {
    use beankeeper_cli::identity::WriteMode;
    use secrecy::ExposeSecret as _;

    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let issue = |name, caps: &[String], mode| {
        Granted::<Admin>::with_db(&db, &ledger.owner, |grant| {
            identity::issue_credential_with_mode(
                grant,
                &IssueRequest {
                    name,
                    capabilities: caps,
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
                mode,
            )
        })
    };
    let proposer = issue(
        "bookkeeping_proposer",
        &["ledger/read".into(), "ledger/post".into()],
        WriteMode::ProposeOnly,
    )?;
    let reviewer = issue(
        "bookkeeping_approver",
        &["ledger/read".into(), "ledger/approve".into()],
        WriteMode::Direct,
    )?;
    ledger.enforce()?;
    let propose_token = proposer.token().expose_secret();
    let approve_token = reviewer.token().expose_secret();
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(propose_token), "Approval-mode Agent")?;
    let tools = server.tools(Some(propose_token))?;
    for name in BOOKKEEPING_TOOLS {
        assert!(tools.iter().any(|tool| tool == name), "{name}");
    }

    let mut call = |tool: &str, args: Value, bearer: &str| -> TestResult<Value> {
        let result = server.call(tool, args, Some(bearer))?;
        assert_ne!(result["result"]["isError"], true, "{tool}: {result}");
        payload(&result)
    };
    let create = json!({"company":"acme","name":"Prospective customer","role":"customer"});
    let pending = call("counterparty_create", create.clone(), propose_token)?;
    assert_eq!(pending["status"], "pending");
    assert!(pending["proposal_id"].is_string());
    assert_eq!(call("counterparty_create", create, propose_token)?, pending);
    let party_count = || -> TestResult<i64> {
        Ok(db
            .read()
            .query_row("SELECT count(*) FROM counterparties", [], |row| row.get(0))?)
    };
    assert_eq!(party_count()?, 0);
    let decision = json!({"company":"acme","id":pending["proposal_id"]});
    let approved = call("txn_approve", decision.clone(), approve_token)?;
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["effect"], "bookkeeping");
    assert!(
        approved["receipt_id"].as_i64().is_some_and(|id| id > 0),
        "{approved}"
    );
    assert_eq!(call("txn_approve", decision, approve_token)?, approved);
    assert_eq!(party_count()?, 1);
    let project_request = json!({"company":"acme","terms":{
        "code":"prospect","name":"Prospect","customer":approved["result"]["party"]["id"],"currency":"USD"
    }});
    let project = call("project_create", project_request, propose_token)?;
    assert_eq!(project["status"], "pending");
    assert_eq!(
        call("project_list", json!({"company":"acme"}), propose_token)?,
        json!([])
    );
    let rejected = call(
        "txn_reject",
        json!({"company":"acme","id":project["proposal_id"],"reason":"Wait for the contract"}),
        approve_token,
    )?;
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(
        call("project_list", json!({"company":"acme"}), propose_token)?,
        json!([])
    );
    server.finish(&[propose_token, approve_token])
}

#[test]
fn http_proposal_discovery_and_resolution_follow_request_grants() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let issue = |name: &str, capability: &str| {
        Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name,
                    capabilities: &[capability.into()],
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
            )
        })
    };
    let proposer = issue("http_proposer", "ledger/propose")?;
    let approver = issue("http_approver", "ledger/approve")?;
    let proposer_token = token(&proposer)?;
    let approver_token = token(&approver)?;
    ledger.enforce()?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(proposer_token), "Proposal Client")?;
    for (credential, expected) in [
        (
            proposer_token,
            vec!["txn_proposals", "txn_propose", "txn_reject"],
        ),
        (
            approver_token,
            vec!["txn_proposals", "txn_approve", "txn_reject"],
        ),
        (token(&ledger.reader)?, vec!["txn_proposals"]),
    ] {
        let tools = server.tools(Some(credential))?;
        for name in ["txn_proposals", "txn_propose", "txn_approve", "txn_reject"] {
            assert_eq!(
                tools.iter().any(|tool| tool == name),
                expected.contains(&name)
            );
        }
    }
    let request = payload(&server.call(
        "txn_propose",
        posting("http-proposal"),
        Some(proposer_token),
    )?)?;
    assert_eq!(request["status"], "pending");
    let pending = payload(&server.call(
        "txn_proposals",
        json!({"company":"acme"}),
        Some(approver_token),
    )?)?;
    assert_eq!(pending[0]["id"], request["id"]);
    let resolution = payload(&server.call(
        "txn_approve",
        json!({"company":"acme", "id":request["id"]}),
        Some(approver_token),
    )?)?;
    assert_eq!(resolution["status"], "approved");
    let rejected = payload(&server.call(
        "txn_propose",
        posting("http-rejected-proposal"),
        Some(proposer_token),
    )?)?;
    let rejected = payload(&server.call(
        "txn_reject",
        json!({"company":"acme", "id":rejected["id"], "reason":"Requote"}),
        Some(approver_token),
    )?)?;
    assert_eq!(rejected["status"], "rejected");
    let posted_by: String = db.read().query_row(
        "SELECT agent_uri FROM transactions WHERE reference = 'http-proposal'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(posted_by, approver["agent_uri"]);
    db::audit::verify_hash_chain(db.read())?;
    db::audit::verify_creations(db.read())?;
    server.finish(&[proposer_token, approver_token])
}

#[test]
fn http_read_only_is_a_ceiling_and_explicit_headers_override_fallback() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enforce()?;
    let reader = token(&ledger.reader)?;
    let writer = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, true, Some(writer))?;
    server.initialize(None, "Fallback Client")?;
    let tools = server.tools(None)?;
    for name in tools {
        assert_eq!(
            name.parse::<beankeeper_cli::operation::Operation>()?
                .primary_capability()?,
            beankeeper_cli::capability::Capability::Read
        );
    }
    assert_eq!(
        payload(&server.call("company_list", json!({}), None)?)?[0]["slug"],
        "acme"
    );
    let denied = server.call("txn_post", posting("read-only"), Some(writer))?;
    assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    for header in [
        "Bearer invalid-secret-token",
        "Basic ignored",
        "Bearer ",
        "Bearer x y",
    ] {
        let response = server
            .client
            .post(&server.endpoint)
            .header("Authorization", header)
            .send()?;
        assert_eq!(response.status(), 401);
    }
    let response = server
        .client
        .post(&server.endpoint)
        .header("Authorization", format!("Bearer {reader}"))
        .header("Authorization", format!("Bearer {writer}"))
        .send()?;
    assert_eq!(response.status(), 401);
    // Neither hostile Host nor browser Origin reaches a tool handler.
    for (header, value) in [
        ("Host", "attacker.example"),
        ("Origin", "https://attacker.example"),
    ] {
        let response = server
            .client
            .post(&server.endpoint)
            .bearer_auth(writer)
            .header(header, value)
            .send()?;
        assert_eq!(response.status(), 403);
    }
    server.finish(&[reader, writer, "invalid-secret-token"])
}

#[test]
fn http_bind_flags_are_validated_and_startup_never_creates_a_ledger() -> TestResult {
    use assert_cmd::assert::OutputAssertExt;
    let ledger = Ledger::new()?;
    ledger
        .command()
        .args(["mcp", "serve", "--http", "--bind", "0.0.0.0:0"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--allow-remote"));
    ledger
        .command()
        .args(["mcp", "serve", "--http", "--bind", "[::]:0"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--allow-remote"));
    for args in [vec!["--bind", "127.0.0.1:0"], vec!["--allow-remote"]] {
        ledger
            .command()
            .args(["mcp", "serve"])
            .args(args)
            .assert()
            .code(2);
    }
    let missing = ledger.path.with_file_name("missing.db");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
    command
        .env_remove("BK_AGENT_TOKEN")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(&missing)
        .args(["mcp", "serve", "--http", "--bind", "127.0.0.1:0"])
        .assert()
        .failure()
        .stdout("");
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn http_fallback_grants_do_not_override_explicit_request_grants() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enforce()?;
    let reader = token(&ledger.reader)?;
    let writer = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, false, Some(writer))?;
    server.initialize(None, "Fallback Writer")?;
    let denied = server.call("txn_post", posting("explicit-reader"), Some(reader))?;
    assert_eq!(payload(&denied)?["code"], "FORBIDDEN");
    assert_eq!(
        payload(&server.call("txn_post", posting("fallback-writer"), None)?)?["created"],
        true
    );
    ledger.revoke(ledger.writer["token_jti"].as_str().ok_or("jti")?)?;
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    assert_eq!(
        payload(&server.call("company_list", json!({}), Some(reader))?)?[0]["slug"],
        "acme"
    );
    server.finish(&[reader, writer])
}

#[test]
fn http_rechecks_token_expiry_and_rejects_other_ledger_tokens() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enforce()?;
    // Start without process credentials, even with enforcement already enabled.
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(token(&ledger.reader)?), "Expiry Client")?;
    let issued = Granted::<Admin>::with_db(&ledger.db()?, &ledger.owner, |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "short_lived_http",
                capabilities: &["ledger/read".into()],
                ttl: Duration::from_secs(3),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    let short_lived = token(&issued)?;
    assert_eq!(
        payload(&server.call("company_list", json!({}), Some(short_lived))?)?[0]["slug"],
        "acme"
    );
    std::thread::sleep(Duration::from_secs(4));
    let response = server.request("tools/list", json!({}), Some(short_lived))?;
    assert_eq!(response.status(), 401);
    assert_eq!(response.json::<Value>()?["code"], "UNAUTHORIZED");
    let other = Ledger::new()?;
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(token(&other.writer)?))?
            .status(),
        401
    );
    server.finish(&[short_lived, token(&other.writer)?])
}

#[test]
fn http_remote_wildcard_bind_works_only_with_explicit_opt_in() -> TestResult {
    let ledger = Ledger::new()?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start_at(&ledger, false, None, "0.0.0.0:0", true)?;
    server.initialize(Some(reader), "Remote Client")?;
    assert_eq!(
        payload(&server.call("company_list", json!({}), Some(reader))?)?[0]["slug"],
        "acme"
    );
    server.finish(&[reader])
}

fn embedded_request(
    client: &Client,
    endpoint: &str,
    session: Option<&str>,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> TestResult<Response> {
    Ok(embedded_rpc(client, endpoint, session, token, method, params).send()?)
}

fn embedded_rpc(
    client: &Client,
    endpoint: &str,
    session: Option<&str>,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> RequestBuilder {
    let mut body = json!({"jsonrpc":"2.0", "id":1, "method":method});
    body["params"] = params;
    let mut request = client
        .post(endpoint)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&body);
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request
}

fn exercise_embedded_router(read_only: bool) -> TestResult {
    let ledger = Ledger::new()?;
    exercise_router(&ledger, read_only, |cli, config| {
        Ok(beankeeper_cli::mcp::http_router(cli, read_only, config)?)
    })
}

fn exercise_router(
    ledger: &Ledger,
    read_only: bool,
    factory: impl FnOnce(
        &mut beankeeper_cli::cli::Cli,
        beankeeper_cli::mcp::StreamableHttpServerConfig,
    ) -> TestResult<axum::Router>,
) -> TestResult {
    exercise_router_with_check(ledger, read_only, factory, |_, _| Ok(()))
}

fn exercise_router_with_check(
    ledger: &Ledger,
    read_only: bool,
    factory: impl FnOnce(
        &mut beankeeper_cli::cli::Cli,
        beankeeper_cli::mcp::StreamableHttpServerConfig,
    ) -> TestResult<axum::Router>,
    check: impl FnOnce(&Client, &str) -> TestResult,
) -> TestResult {
    use axum::{Router, routing::get};
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::StreamableHttpServerConfig;
    use clap::Parser;

    ledger.enforce()?;
    let writer = token(&ledger.writer)?;
    let mut cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    cli.db.clone_from(&ledger.path);
    cli.passphrase.passphrase_file = Some(ledger.secret.clone());
    cli.agent_token = Some(writer.into());
    let config = StreamableHttpServerConfig::default().enforce_origin_validation();
    let cancellation = config.cancellation_token.clone();
    // Construction works before entering a runtime and does not bind a socket.
    let app = Router::new()
        .route("/health", get(|| async { "host is healthy" }))
        .merge(factory(&mut cli, config)?);
    // The passphrase is no longer needed by requests after construction.
    if ledger.secret.exists() {
        std::fs::remove_file(&ledger.secret)?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let base = format!("http://{}", listener.local_addr()?);
    let endpoint = format!("{base}/mcp");
    let shutdown = cancellation.clone();
    let server = runtime.spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    // MCP authentication must not leak onto the host's sibling routes.
    assert_eq!(
        client.get(format!("{base}/health")).send()?.text()?,
        "host is healthy"
    );
    // A host browser session cannot substitute for an MCP bearer credential.
    assert_eq!(
        client
            .post(&endpoint)
            .header("Cookie", "session=host-browser-session")
            .header("Origin", "http://localhost")
            .send()?
            .status(),
        403
    );
    check(&client, &endpoint)?;
    verify_embedded_router(&client, &endpoint, ledger, read_only)?;
    cancellation.cancel();
    runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), server).await })???;
    Ok(())
}

fn initialize_embedded_router(client: &Client, endpoint: &str) -> TestResult<String> {
    let response = embedded_request(
        client,
        endpoint,
        None,
        None,
        "initialize",
        json!({
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"Embedded Client","version":"1.0"}
        }),
    )?;
    assert!(response.status().is_success());
    let session = response
        .headers()
        .get("Mcp-Session-Id")
        .ok_or("session")?
        .to_str()?
        .to_owned();
    assert_eq!(rpc(response)?["result"]["serverInfo"]["name"], "beankeeper");
    assert!(
        client
            .post(endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .header("Mcp-Session-Id", &session)
            .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .send()?
            .status()
            .is_success()
    );
    Ok(session)
}

fn verify_embedded_router(
    client: &Client,
    endpoint: &str,
    ledger: &Ledger,
    read_only: bool,
) -> TestResult {
    let reader = token(&ledger.reader)?;
    let session = initialize_embedded_router(client, endpoint)?;
    let request = |credential, method, params| {
        embedded_request(client, endpoint, Some(&session), credential, method, params)
    };
    for (credential, can_post) in [(None, !read_only), (Some(reader), false)] {
        let response = rpc(request(credential, "tools/list", json!({}))?)?;
        let tools = response["result"]["tools"].as_array().ok_or("tools")?;
        assert_eq!(
            tools.iter().any(|tool| tool["name"] == "txn_post"),
            can_post
        );
    }
    let denied = rpc(request(
        Some(reader),
        "tools/call",
        json!({
            "name":"txn_post", "arguments":posting("embedded-denied")
        }),
    )?)?;
    if read_only {
        assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    } else {
        assert_eq!(payload(&denied)?["code"], "FORBIDDEN");
    }
    let posted = rpc(request(
        None,
        "tools/call",
        json!({
            "name":"txn_post", "arguments":posting("embedded-post")
        }),
    )?)?;
    if read_only {
        assert!(posted.get("error").is_some() || posted["result"]["isError"] == true);
    } else {
        assert_eq!(payload(&posted)?["created"], true);
    }
    let db = ledger.db()?;
    let count: i64 = db
        .read()
        .query_row("SELECT count(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, i64::from(!read_only));
    db::audit::verify_hash_chain(db.read())?;
    db::audit::verify_creations(db.read())?;
    assert_eq!(
        request(Some("invalid-token"), "tools/list", json!({}))?.status(),
        401
    );
    ledger.revoke(ledger.writer["token_jti"].as_str().ok_or("jti")?)?;
    // The process fallback is checked again even in an established session.
    assert_eq!(request(None, "tools/list", json!({}))?.status(), 401);
    let read = rpc(request(
        Some(reader),
        "tools/call",
        json!({
            "name":"company_list", "arguments":{}
        }),
    )?)?;
    assert_eq!(payload(&read)?[0]["slug"], "acme");
    Ok(())
}

#[test]
fn embedded_http_router_preserves_authentication_and_capabilities() -> TestResult {
    exercise_embedded_router(false)
}

#[test]
fn embedded_http_router_preserves_read_only_ceiling() -> TestResult {
    exercise_embedded_router(true)
}

#[test]
fn embedded_http_router_reports_startup_errors_without_creating_a_ledger() -> TestResult {
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::{StreamableHttpServerConfig, http_router};
    use clap::Parser;

    let ledger = Ledger::new()?;
    let mut cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    cli.db.clone_from(&ledger.path);
    cli.passphrase.passphrase_file = Some(ledger.secret.with_file_name("missing-passphrase"));
    assert!(http_router(&cli, false, StreamableHttpServerConfig::default()).is_err());
    cli.passphrase.passphrase_file = Some(ledger.secret.clone());
    cli.db = ledger.path.with_file_name("missing.db");
    assert!(matches!(
        http_router(&cli, false, StreamableHttpServerConfig::default()),
        Err(beankeeper_cli::error::CliError::NotFound(_))
    ));
    assert!(!cli.db.exists());
    Ok(())
}

fn exercise_resolved_router(passphrase: Option<SecretString>, read_only: bool) -> TestResult {
    use beankeeper_cli::mcp::http_router_with_passphrase;

    let ledger = Ledger::without_secret_file()?;
    exercise_router(&ledger, read_only, |cli, config| {
        // Neither the invalid FD nor the nonexistent file may be consulted,
        // including on platforms where passphrase FDs are unsupported.
        cli.passphrase.passphrase_fd = Some(-1);
        let app = http_router_with_passphrase(cli, passphrase.as_ref(), read_only, config)?;
        let snapshot = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
        drop(passphrase);
        let slug: String = snapshot
            .read()
            .query_row("SELECT slug FROM companies", [], |row| row.get(0))?;
        assert_eq!(slug, "acme");
        let write = Granted::<Admin>::with_db(&snapshot, &ledger.owner, |grant| {
            db::audit::set_policy(grant, false)
        });
        assert!(matches!(
            write,
            Err(beankeeper_cli::error::CliError::Sqlite(error))
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ReadOnly)
        ));
        Ok(app)
    })?;
    // Only the ledger and SQLite sidecars may have been created. In particular,
    // the resolved secret must never be bridged through a plaintext file.
    for entry in std::fs::read_dir(ledger.dir.path())? {
        assert!(matches!(
            entry?.file_name().to_str(),
            Some("books.db" | "books.db-wal" | "books.db-shm")
        ));
    }
    Ok(())
}

#[test]
fn resolved_http_router_supports_encrypted_snapshots_and_read_only_ceiling() -> TestResult {
    for read_only in [false, true] {
        exercise_resolved_router(Some(PASSPHRASE.into()), read_only)?;
    }
    Ok(())
}

#[test]
fn resolved_http_router_supports_unencrypted_snapshots_without_resolving_sources() -> TestResult {
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::{StreamableHttpServerConfig, http_router_with_passphrase};
    use clap::Parser;

    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let db = Db::open(&path, None)?;
    Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
        db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
    })?;
    let mut cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    cli.db = path;
    cli.passphrase.passphrase_fd = Some(-1);
    cli.passphrase.passphrase_file = Some(dir.path().join("missing-passphrase"));
    for read_only in [false, true] {
        let _app = http_router_with_passphrase(
            &cli,
            None,
            read_only,
            StreamableHttpServerConfig::default(),
        )?;
        let snapshot = Db::open_snapshot(&cli.db, None)?;
        let slug: String = snapshot
            .read()
            .query_row("SELECT slug FROM companies", [], |row| row.get(0))?;
        assert_eq!(slug, "acme");
    }
    for entry in std::fs::read_dir(dir.path())? {
        assert!(matches!(
            entry?.file_name().to_str(),
            Some("books.db" | "books.db-wal" | "books.db-shm")
        ));
    }
    Ok(())
}

#[test]
fn resolved_http_router_requires_bearer_authentication_without_a_fallback() -> TestResult {
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::{StreamableHttpServerConfig, http_router_with_passphrase};
    use clap::Parser;

    let passphrase = SecretString::from(PASSPHRASE);
    let ledger = Ledger::without_secret_file()?;
    let mut cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    cli.db.clone_from(&ledger.path);
    cli.agent_token = None;
    let config = StreamableHttpServerConfig::default().enforce_origin_validation();
    let cancellation = config.cancellation_token.clone();
    let app = http_router_with_passphrase(&cli, Some(&passphrase), false, config)?;
    drop(passphrase);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let shutdown = cancellation.clone();
    let server = runtime.spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    for bearer in [None, Some("invalid-token")] {
        let mut request = client
            .post(&endpoint)
            .header("Cookie", "session=host-browser-session");
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        assert_eq!(request.send()?.status(), 401);
    }
    let reader = token(&ledger.reader)?;
    assert_eq!(
        client
            .post(&endpoint)
            .bearer_auth(reader)
            .header("Cookie", "session=host-browser-session")
            .header("Origin", "http://localhost")
            .send()?
            .status(),
        403
    );
    let initialized = embedded_request(
        &client,
        &endpoint,
        None,
        Some(reader),
        "initialize",
        json!({
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"Bearer Client","version":"1.0"}
        }),
    )?;
    assert!(initialized.status().is_success());
    assert_eq!(
        rpc(initialized)?["result"]["serverInfo"]["name"],
        "beankeeper"
    );
    cancellation.cancel();
    runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), server).await })???;
    Ok(())
}

#[cfg(unix)]
#[test]
fn resolved_http_router_consumes_a_one_shot_fd_once() -> TestResult {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let (reader, mut writer) = std::io::pipe()?;
    writeln!(writer, "{PASSPHRASE}")?;
    drop(writer);
    let passphrase =
        beankeeper_cli::passphrase::resolve_passphrase(Some(reader.as_raw_fd()), None, false)?;
    exercise_resolved_router(passphrase, false)?;
    assert!(
        beankeeper_cli::passphrase::resolve_passphrase(Some(reader.as_raw_fd()), None, false)
            .is_err()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn resolved_http_router_consumes_a_one_shot_command_once() -> TestResult {
    const CHILD: &str = "BK_TEST_RESOLVED_PASSPHRASE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let passphrase = beankeeper_cli::passphrase::resolve_passphrase(None, None, false)?;
        assert!(passphrase.is_some());
        return exercise_resolved_router(passphrase, false);
    }

    // Isolate the command environment in a child rather than mutating the
    // process environment while other tests are running. The command stores
    // only a call counter, never the secret, and fails on a second invocation.
    let dir = tempdir()?;
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "resolved_http_router_consumes_a_one_shot_command_once",
            "--nocapture",
        ])
        .current_dir(dir.path())
        .env(CHILD, "1")
        .env(
            "BEANKEEPER_PASSPHRASE_CMD",
            format!(
                "test ! -e calls || exit 23; printf 'called\\n' >> calls; printf '{PASSPHRASE}\\n'"
            ),
        )
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls"))?,
        "called\n"
    );
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[test]
fn resolved_http_router_rejects_missing_and_incorrect_keys_without_creating_a_ledger() -> TestResult
{
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::{StreamableHttpServerConfig, http_router_with_passphrase};
    use clap::Parser;

    let ledger = Ledger::without_secret_file()?;
    let mut cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    cli.db.clone_from(&ledger.path);
    for passphrase in [None, Some(SecretString::from("incorrect key"))] {
        assert!(
            http_router_with_passphrase(
                &cli,
                passphrase.as_ref(),
                false,
                StreamableHttpServerConfig::default()
            )
            .is_err()
        );
    }
    cli.db = ledger.path.with_file_name("missing.db");
    assert!(matches!(
        http_router_with_passphrase(
            &cli,
            Some(&PASSPHRASE.into()),
            false,
            StreamableHttpServerConfig::default()
        ),
        Err(beankeeper_cli::error::CliError::NotFound(_))
    ));
    assert!(!cli.db.exists());
    Ok(())
}

struct TestHost {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[test]
fn preopened_startup_ledger_replays_missed_sse_response() -> TestResult {
    use beankeeper_cli::mcp::{HostTool, HostToolHandler, http_router_with_ledger_targets};
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct ReplayHost {
        host: TestHost,
        release: Arc<Notify>,
    }

    impl HostToolHandler for ReplayHost {
        fn tools(&self) -> Vec<HostTool> {
            self.host.tools()
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            identity: identity::VerifiedAgent,
        ) -> Result<CallToolResult, rmcp::ErrorData> {
            self.release.notified().await;
            self.host.call_tool(request, identity).await
        }
    }

    let ledger = Ledger::new()?;
    let release = Arc::new(Notify::new());
    let calls = Arc::default();
    exercise_router_with_check(
        &ledger,
        false,
        |cli, config| {
            let host = ReplayHost {
                host: TestHost {
                    calls: Arc::clone(&calls),
                },
                release: Arc::clone(&release),
            };
            Ok(http_router_with_ledger_targets(
                cli,
                ledger.db()?,
                false,
                config,
                host,
            )?)
        },
        |client, endpoint| {
            let session = initialize_embedded_router(client, endpoint)?;
            let original = embedded_request(
                client,
                endpoint,
                Some(&session),
                None,
                "tools/call",
                json!({"name":"host_post", "arguments":{}}),
            )?;
            assert_eq!(original.status(), 200);
            let mut lines = BufReader::new(original).lines();
            let last_event_id = loop {
                let line = lines.next().ok_or("missing SSE event ID")??;
                if let Some(id) = line.strip_prefix("id:") {
                    break id.trim().to_owned();
                }
            };
            drop(lines);

            let resume = || {
                client
                    .get(endpoint)
                    .header("Accept", "text/event-stream")
                    .header("MCP-Protocol-Version", "2025-11-25")
                    .header("Mcp-Session-Id", &session)
                    .header("Last-Event-ID", &last_event_id)
            };
            assert_eq!(resume().bearer_auth("invalid").send()?.status(), 401);
            let resumed = resume().send()?;
            assert_eq!(resumed.status(), 200);
            // Keep the call in flight until the resumed stream is attached.
            release.notify_one();
            let response = rpc(resumed)?;
            assert_eq!(response["id"], 1);
            assert_eq!(payload(&response)?["tool"], "host_post");
            assert_eq!(calls.lock().map_err(|_| "host calls lock")?.len(), 1);
            Ok(())
        },
    )
}

#[test]
fn cli_exposes_validated_ledger_bindings_to_hosts() -> TestResult {
    use beankeeper_cli::{cli::Cli, error::CliError};
    use clap::Parser;
    use std::path::Path;

    let cli = Cli::try_parse_from([
        "host",
        "--db",
        "startup.db",
        "--ledger",
        "zebra=relative=a.db",
        "--ledger",
        "demo=/tmp/demo.db",
        "--ledger-target",
        "zebra",
        "mcp",
        "serve",
    ])?;
    let bindings: Vec<(&NamedLedgerTarget, &LedgerPath)> = cli.ledger_bindings()?.collect();
    let [(demo, demo_path), (zebra, zebra_path)] = bindings.as_slice() else {
        return Err("expected two named bindings".into());
    };
    assert_eq!(demo.as_ref(), "demo");
    assert_eq!(demo_path.as_ref(), Path::new("/tmp/demo.db"));
    assert_eq!(zebra.as_ref(), "zebra");
    assert_eq!(zebra_path.as_ref(), Path::new("relative=a.db"));
    assert_eq!(cli.db_path()?, zebra_path.as_ref());

    let duplicate =
        Cli::try_parse_from(["host", "--ledger", "demo=a", "--ledger", "demo=b", "verify"])?;
    assert!(matches!(
        duplicate.ledger_bindings(),
        Err(CliError::Validation(_))
    ));
    let reserved = Cli::try_parse_from(["host", "--ledger", "startup=a", "verify"])?;
    assert!(matches!(
        reserved.ledger_bindings(),
        Err(CliError::Validation(_))
    ));
    assert!(Cli::try_parse_from(["host", "--ledger", "demo=", "verify"]).is_err());
    Ok(())
}

#[test]
fn embedded_session_dispatches_against_a_ledger_registered_after_initialization() -> TestResult {
    use axum::{
        extract::Request,
        middleware::{self, Next},
    };
    use beankeeper_cli::identity::WriteMode;
    use beankeeper_cli::mcp::{LedgerTarget, LedgerTargets, http_router_with_ledger_targets};
    use secrecy::ExposeSecret as _;

    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    let targets = LedgerTargets::new(startup.db()?);
    let target: LedgerTarget = "demo".parse()?;
    let proposer = Granted::<Admin>::with_db(&demo.db()?, &demo.owner, |grant| {
        identity::issue_credential_with_mode(
            grant,
            &IssueRequest {
                name: "demo_proposer",
                capabilities: &["ledger/read".into(), "ledger/post".into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
            WriteMode::ProposeOnly,
        )
    })?;
    exercise_router_with_check(
        &startup,
        false,
        |cli, config| {
            let host = TestHost {
                calls: std::sync::Arc::default(),
            };
            let selected = target.clone();
            let app = http_router_with_ledger_targets(cli, targets.clone(), false, config, host)?;
            Ok(app.layer(middleware::from_fn(
                move |mut request: Request, next: Next| {
                    if request.headers().contains_key("x-test-host-selection") {
                        request.extensions_mut().insert(selected.clone());
                    }
                    async move { next.run(request).await }
                },
            )))
        },
        |client, endpoint| {
            let session = initialize_embedded_router(client, endpoint)?;
            targets.register(target.clone(), demo.db()?)?;
            verify_selected_ledger(
                &TargetSession {
                    client,
                    endpoint,
                    id: &session,
                },
                &startup,
                &demo,
                proposer.token().expose_secret(),
                proposer.agent_uri.as_str(),
            )
        },
    )
}

struct TargetSession<'a> {
    client: &'a Client,
    endpoint: &'a str,
    id: &'a str,
}

impl TargetSession<'_> {
    fn request(&self, method: &str, params: Value, bearer: Option<&str>) -> RequestBuilder {
        embedded_rpc(
            self.client,
            self.endpoint,
            Some(self.id),
            bearer,
            method,
            params,
        )
    }

    fn call(&self, name: &str, arguments: &Value, bearer: Option<&str>) -> TestResult<Value> {
        use beankeeper_cli::mcp::LEDGER_TARGET_HEADER;
        rpc(self
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                bearer,
            )
            .header(LEDGER_TARGET_HEADER, "demo")
            .send()?)
    }
}

fn verify_selected_ledger(
    session: &TargetSession<'_>,
    startup: &Ledger,
    demo: &Ledger,
    proposer: &str,
    proposer_uri: &str,
) -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_HEADER;

    let direct = token(&demo.writer)?;
    let discovery = |bearer| session.request("tools/list", json!({}), bearer);
    let listed = rpc(discovery(Some(proposer))
        .header(LEDGER_TARGET_HEADER, "missing")
        .header("x-test-host-selection", "1")
        .send()?)?;
    let tools = listed["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "txn_post"));
    assert!(!tools.iter().any(|tool| tool["name"] == "txn_approve"));
    let posted = payload(&session.call("txn_post", &posting("demo-direct"), Some(direct))?)?;
    assert_eq!(posted["created"], true);
    let pending = payload(&session.call("txn_post", &posting("demo-pending"), Some(proposer))?)?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["proposer_uri"], proposer_uri);
    let ledger = demo.db()?;
    let attribution: (String, String) = ledger.read().query_row(
        "SELECT agent_uri, token_jti FROM transactions WHERE reference = 'demo-direct'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(attribution.0, demo.writer["agent_uri"]);
    assert_eq!(attribution.1, demo.writer["token_jti"]);
    let list = json!({"name":"txn_list", "arguments":{"company":"acme"}});
    let startup_rows = rpc(session.request("tools/call", list.clone(), None).send()?)?;
    assert_eq!(payload(&startup_rows)?, json!([]));
    let demo_rows = payload(&session.call("txn_list", &list["arguments"], Some(direct))?)?;
    assert_eq!(demo_rows.as_array().map(Vec::len), Some(1));
    assert_eq!(demo_rows[0]["id"], posted["id"]);
    assert!(
        session
            .call("host_post", &json!({}), Some(direct))?
            .get("error")
            .is_some()
    );

    demo.enforce()?;
    let forbidden = session.call(
        "txn_post",
        &posting("demo-forbidden"),
        Some(token(&demo.reader)?),
    )?;
    assert_eq!(payload(&forbidden)?["code"], "FORBIDDEN");
    demo.revoke(demo.writer["token_jti"].as_str().ok_or("jti")?)?;
    let revoked = session
        .request(
            "tools/call",
            json!({"name":"txn_post", "arguments":posting("demo-revoked")}),
            Some(direct),
        )
        .header(LEDGER_TARGET_HEADER, "demo")
        .send()?;
    assert_eq!(revoked.status(), 401);
    assert_eq!(revoked.json::<Value>()?["code"], "UNAUTHORIZED");
    assert_target_rejections(session, token(&startup.writer)?)
}

fn assert_target_rejections(session: &TargetSession<'_>, startup_token: &str) -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_HEADER;

    let discovery = |bearer| session.request("tools/list", json!({}), bearer);
    for (name, bearer, status) in [
        ("../demo", Some(startup_token), 400),
        ("missing", Some(startup_token), 404),
        ("demo", Some(startup_token), 401),
        ("demo", None, 401),
    ] {
        let response = discovery(bearer)
            .header(LEDGER_TARGET_HEADER, name)
            .send()?;
        assert_eq!(response.status(), status);
    }
    assert_eq!(
        discovery(Some(startup_token))
            .header(LEDGER_TARGET_HEADER, "startup")
            .header(LEDGER_TARGET_HEADER, "demo")
            .send()?
            .status(),
        400
    );
    assert_eq!(
        session
            .client
            .get(session.endpoint)
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", session.id)
            .header("Last-Event-ID", "0")
            .bearer_auth(startup_token)
            .send()?
            .status(),
        405
    );
    Ok(())
}

#[test]
fn cli_selects_a_named_ledger_and_rejects_unknown_targets() -> TestResult {
    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    let binding = format!("demo={}", demo.path.display());
    let run = |target: &str| -> TestResult<std::process::Output> {
        startup
            .command()
            .args([
                "--ledger",
                &binding,
                "--ledger-target",
                target,
                "--json",
                "company",
                "list",
                "--agent-token",
                token(&demo.writer)?,
            ])
            .output()
            .map_err(Into::into)
    };
    let selected = run("demo")?;
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let result = serde_json::from_slice::<Value>(&selected.stdout)?;
    assert_eq!(result["data"][0]["slug"], "acme");
    assert_eq!(result["meta"]["ledger_target"], "demo");
    let unknown = run("missing")?;
    assert_eq!(unknown.status.code(), Some(5));
    assert_eq!(
        serde_json::from_slice::<Value>(&unknown.stderr)?["error"]["code"],
        "NOT_FOUND"
    );
    assert_eq!(run("startup")?.status.code(), Some(6));
    Ok(())
}

#[test]
fn cli_http_registrations_keep_the_read_only_ceiling() -> TestResult {
    use beankeeper_cli::mcp::{
        LEDGER_TARGET_HEADER, LEDGER_TARGET_META_KEY, LEDGER_UPGRADE_META_KEY,
    };

    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    demo.rewind_before_restore()?;
    let startup_token = token(&startup.writer)?;
    let demo_token = token(&demo.writer)?;
    let mut command = startup.command();
    command.args([
        "mcp",
        "serve",
        "--http",
        "--read-only",
        "--bind",
        "127.0.0.1:0",
        "--ledger",
        &format!("demo={}", demo.path.display()),
    ]);
    let mut server = Server::spawn(command)?;
    server.initialize(Some(startup_token), "Named ledger client")?;
    let result = server.call("company_list", json!({}), Some(startup_token))?;
    assert!(result.get("error").is_none(), "{result}");
    assert!(
        result["result"]["_meta"]
            .get(LEDGER_UPGRADE_META_KEY)
            .is_none()
    );
    let session = TargetSession {
        client: &server.client,
        endpoint: &server.endpoint,
        id: server.session.as_deref().ok_or("session")?,
    };
    let listed = rpc(session
        .request("tools/list", json!({}), Some(demo_token))
        .header(LEDGER_TARGET_HEADER, "demo")
        .send()?)?;
    let tools = listed["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "txn_list"));
    assert!(!tools.iter().any(|tool| tool["name"] == "txn_post"));
    let result = session.call("company_list", &json!({}), Some(demo_token))?;
    let meta = &result["result"]["_meta"];
    assert_eq!(meta[LEDGER_TARGET_META_KEY], "demo");
    let upgrade = &meta[LEDGER_UPGRADE_META_KEY];
    assert_eq!(upgrade["from"], 37);
    assert_eq!(upgrade["to"], json!(db::current_schema_version()));
    let backup = PathBuf::from(upgrade["backup"].as_str().ok_or("missing backup path")?);
    assert!(backup.starts_with(demo.dir.path().canonicalize()?));
    assert!(backup.is_file());
    let result = session.call("txn_post", &posting("blocked"), Some(demo_token))?;
    assert!(result.get("error").is_some() || result["result"]["isError"] == true);
    server.finish(&[startup_token, demo_token])
}

impl beankeeper_cli::mcp::HostToolHandler for TestHost {
    fn tools(&self) -> Vec<beankeeper_cli::mcp::HostTool> {
        use beankeeper_cli::capability::Capability;
        use beankeeper_cli::mcp::{HostTool, HostToolEffect};
        [
            (
                "host_observe",
                Capability::Navigate,
                HostToolEffect::ReadOnly,
            ),
            (
                "host_navigate",
                Capability::Navigate,
                HostToolEffect::Mutation,
            ),
            ("host_post", Capability::Post, HostToolEffect::Mutation),
        ]
        .into_iter()
        .map(|(name, capability, effect)| HostTool {
            tool: rmcp::model::Tool::new(
                name,
                "Test host tool",
                serde_json::Map::from_iter([("type".into(), json!("object"))]),
            ),
            capability,
            effect,
        })
        .collect()
    }

    fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        identity: identity::VerifiedAgent,
    ) -> impl std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send
    {
        let result = self
            .calls
            .lock()
            .map_err(|_| rmcp::ErrorData::internal_error("host unavailable", None))
            .map(|mut calls| {
                calls.push(identity.jti().to_string());
                rmcp::model::CallToolResult::structured(json!({
                    "tool": request.name, "agent_uri": identity.uri(), "token_jti": identity.jti()
                }))
            });
        std::future::ready(result)
    }
}

#[test]
fn composed_host_tools_use_live_request_grants_and_preserve_ledger_tools() -> TestResult {
    exercise_host_tools(false)
}

#[test]
fn composed_host_read_only_ceiling_also_removes_host_mutations() -> TestResult {
    exercise_host_tools(true)
}

fn exercise_host_tools(read_only: bool) -> TestResult {
    use beankeeper_cli::mcp::{StreamableHttpServerConfig, http_router_with_host_tools};

    let ledger = Ledger::new()?;
    let navigation =
        Granted::<Admin>::with_db(&ledger.db()?, &Actor::new("owner".parse()?), |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name: "navigator",
                    capabilities: &["ui/navigate".into()],
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let host = TestHost {
        calls: calls.clone(),
    };
    exercise_router_with_check(
        &ledger,
        read_only,
        |cli, config: StreamableHttpServerConfig| {
            Ok(http_router_with_host_tools(
                cli,
                Some(&PASSPHRASE.into()),
                read_only,
                config,
                host,
            )?)
        },
        |client, endpoint| {
            verify_host_tools(client, endpoint, &ledger, read_only, &navigation, &calls)
        },
    )
}

struct HostSession<'a> {
    client: &'a Client,
    endpoint: &'a str,
    session: String,
}

impl HostSession<'_> {
    fn request(
        &self,
        credential: Option<&str>,
        method: &str,
        params: Value,
    ) -> TestResult<Response> {
        embedded_request(
            self.client,
            self.endpoint,
            Some(&self.session),
            credential,
            method,
            params,
        )
    }

    fn call(&self, credential: Option<&str>, name: &str) -> TestResult<Value> {
        rpc(self.request(
            credential,
            "tools/call",
            json!({"name":name, "arguments":{}}),
        )?)
    }
}

fn verify_host_tools(
    client: &Client,
    endpoint: &str,
    ledger: &Ledger,
    read_only: bool,
    navigation: &Value,
    calls: &std::sync::Mutex<Vec<String>>,
) -> TestResult {
    let session = HostSession {
        client,
        endpoint,
        session: initialize_embedded_router(client, endpoint)?,
    };
    let navigator = token(navigation)?;
    let reader = token(&ledger.reader)?;
    // Initialize used the writer fallback. Every subsequent bearer may differ.
    verify_host_policy(&session, ledger, read_only, navigation)?;
    let moved = session.call(Some(navigator), "host_navigate")?;
    if read_only {
        assert!(moved.get("error").is_some() || moved["result"]["isError"] == true);
    } else {
        assert_eq!(payload(&moved)?["token_jti"], navigation["token_jti"]);
    }
    let fallback = session.call(None, "host_post")?;
    if read_only {
        assert!(fallback.get("error").is_some() || fallback["result"]["isError"] == true);
    } else {
        assert_eq!(payload(&fallback)?["token_jti"], ledger.writer["token_jti"]);
    }
    // Explicit bad credentials never fall back to the configured writer.
    assert_eq!(
        session
            .request(Some("invalid"), "tools/list", json!({}))?
            .status(),
        401
    );
    assert_eq!(
        session
            .request(
                Some("invalid"),
                "tools/call",
                json!({
                    "name":"host_observe", "arguments":{}
                })
            )?
            .status(),
        401
    );
    ledger.revoke(navigation["token_jti"].as_str().ok_or("jti")?)?;
    for (method, params) in [
        ("tools/list", json!({})),
        ("tools/call", json!({"name":"host_observe", "arguments":{}})),
    ] {
        assert_eq!(
            session.request(Some(navigator), method, params)?.status(),
            401
        );
    }
    // Revoking one bearer does not revoke a different bearer on that session.
    let result = session.call(Some(reader), "company_list")?;
    assert_eq!(payload(&result)?[0]["slug"], "acme");
    let calls = calls.lock().map_err(|_| "host calls lock")?;
    assert_eq!(calls.len(), if read_only { 3 } else { 5 });
    assert_eq!(
        calls
            .iter()
            .filter(|jti| jti.as_str() == navigation["token_jti"].as_str().unwrap_or_default())
            .count(),
        if read_only { 3 } else { 4 }
    );
    assert_eq!(
        calls
            .iter()
            .filter(|jti| jti.as_str() == ledger.writer["token_jti"].as_str().unwrap_or_default())
            .count(),
        usize::from(!read_only)
    );
    Ok(())
}

fn verify_host_policy(
    session: &HostSession<'_>,
    ledger: &Ledger,
    read_only: bool,
    navigation: &Value,
) -> TestResult {
    let navigator = token(navigation)?;
    let reader = token(&ledger.reader)?;
    let db = ledger.db()?;
    for enforce in [true, false, true] {
        Granted::<Admin>::with_db(&db, &ledger.owner, |grant| {
            db::audit::set_policy(grant, enforce)
        })?;
        for credential in [None, Some(reader), Some(navigator)] {
            let tools = rpc(session.request(credential, "tools/list", json!({}))?)?;
            let names: Vec<_> = tools["result"]["tools"]
                .as_array()
                .ok_or("tools")?
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert_eq!(
                names.contains(&"host_observe"),
                credential == Some(navigator)
            );
            assert_eq!(
                names.contains(&"host_navigate"),
                credential == Some(navigator) && !read_only
            );
            assert_eq!(
                names.contains(&"host_post"),
                credential.is_none() && !read_only
            );
            assert_eq!(
                names.contains(&"company_list"),
                !enforce || credential != Some(navigator)
            );
            assert_eq!(
                names.contains(&"txn_post"),
                !read_only && (!enforce || credential.is_none())
            );
            let result = session.call(credential, "host_observe")?;
            if credential == Some(navigator) {
                assert_eq!(payload(&result)?["token_jti"], navigation["token_jti"]);
                assert_eq!(payload(&result)?["agent_uri"], navigation["agent_uri"]);
            } else {
                assert_eq!(payload(&result)?["code"], "FORBIDDEN");
            }
        }
    }
    Ok(())
}

#[test]
fn composed_router_rejects_duplicate_and_reserved_tool_names() -> TestResult {
    use beankeeper_cli::cli::Cli;
    use beankeeper_cli::mcp::{
        HostTool, HostToolHandler, StreamableHttpServerConfig, http_router_with_host_tools,
    };
    use clap::Parser;

    struct Catalog(Vec<HostTool>);
    impl HostToolHandler for Catalog {
        fn tools(&self) -> Vec<HostTool> {
            self.0.clone()
        }
        fn call_tool(
            &self,
            _: rmcp::model::CallToolRequestParams,
            _: identity::VerifiedAgent,
        ) -> impl std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send
        {
            std::future::ready(Err(rmcp::ErrorData::internal_error(
                "must not dispatch",
                None,
            )))
        }
    }
    let cli = Cli::try_parse_from(["host", "mcp", "serve"])?;
    let host = TestHost {
        calls: std::sync::Arc::default(),
    };
    let entry = host.tools().remove(0);
    for name in ["", "company_list", "txn_post", "host_observe"] {
        let mut duplicate = entry.clone();
        duplicate.tool.name = name.to_owned().into();
        for read_only in [false, true] {
            assert!(matches!(
                http_router_with_host_tools(
                    &cli,
                    None,
                    read_only,
                    StreamableHttpServerConfig::default(),
                    Catalog(vec![entry.clone(), duplicate.clone()])
                ),
                Err(beankeeper_cli::error::CliError::Validation(_))
            ));
        }
    }
    Ok(())
}

#[test]
fn demo_host_tool_provisions_only_the_host_managed_path() -> TestResult {
    use beankeeper_cli::mcp::{DemoLedgerTool, http_router_with_host_tools};

    let ledger = Ledger::new()?;
    let destination = ledger.dir.path().join("tailored.db");
    let startup_admin = Granted::<Admin>::with_db(&ledger.db()?, &ledger.owner, |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "synthetic_demo_initializer",
                capabilities: &["ledger/admin".into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    let admin = token(&startup_admin)?;
    exercise_router_with_check(
        &ledger,
        false,
        |cli, config| {
            Ok(http_router_with_host_tools(
                cli,
                Some(&PASSPHRASE.into()),
                false,
                config,
                DemoLedgerTool::new(destination.clone(), Some(PASSPHRASE.into())),
            )?)
        },
        |client, endpoint| {
            let session = HostSession {
                client,
                endpoint,
                session: initialize_embedded_router(client, endpoint)?,
            };
            let list = rpc(session.request(Some(admin), "tools/list", json!({}))?)?;
            let tools = list["result"]["tools"]
                .as_array()
                .ok_or("missing tool list")?;
            let demo = tools
                .iter()
                .find(|tool| tool["name"] == DemoLedgerTool::NAME)
                .ok_or("missing demo tool")?;
            assert_eq!(demo["inputSchema"]["additionalProperties"], false);
            assert_eq!(demo["inputSchema"]["properties"]["books"]["minItems"], 1);
            let schema = demo["inputSchema"].to_string();
            assert!(schema.contains("credit_card"));
            assert!(!schema.contains("\"$ref\""));

            let not_allowed = session.call(Some(token(&ledger.writer)?), DemoLedgerTool::NAME)?;
            assert_eq!(payload(&not_allowed)?["code"], "FORBIDDEN");
            let call = |arguments| {
                let result = rpc(session.request(
                    Some(admin),
                    "tools/call",
                    json!({
                        "name": DemoLedgerTool::NAME, "arguments": arguments
                    }),
                )?)?;
                payload(&result)
            };
            assert_eq!(call(json!({"path":"ignored.db"}))?["code"], "VALIDATION");
            assert_eq!(call(json!({"books":[]}))?["code"], "VALIDATION");
            assert!(!destination.exists());
            let result = call(json!({"country":"CA", "books":[{"kind":"sole_proprietorship"}]}))?;
            assert_eq!(result["is_demo"], true);
            assert_eq!(result["books"], 1);
            assert_eq!(call(json!({}))?["code"], "VALIDATION");

            let demo = Db::open_existing(&destination, Some(&PASSPHRASE.into()))?;
            db::verification::VerifiedLedger::read(&demo)?;
            let companies = db::list_companies(demo.read())?;
            assert_eq!(companies[0].name, "Fictional Juniper Workshop 1");
            let seeds = db::list_audit(
                demo.read(),
                &db::ListAuditParams {
                    action: Some(db::audit::AuditAction::DemoSeed),
                    actor: None,
                    agent_uri: None,
                    company_slug: None,
                    entity: None,
                    limit: db::audit::AuditPageLimit::default(),
                    offset: db::audit::AuditPageOffset::default(),
                },
            )?;
            assert_eq!(seeds[0].actor, startup_admin["agent_uri"]);
            assert_eq!(db::list_companies(ledger.db()?.read())?.len(), 1);
            Ok(())
        },
    )
}

#[test]
fn encrypted_startup_restore_and_cli_undo_keep_live_identity_and_audit() -> TestResult {
    use assert_cmd::prelude::OutputAssertExt;

    let ledger = Ledger::new()?;
    let admin = token(&ledger.admin)?;
    let writer = token(&ledger.writer)?;
    let revoked = token(&ledger.reader)?;
    let backup = ledger.dir.path().join("earlier.bk-backup");
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(admin), "Restore integration")?;
    assert_eq!(
        payload(&server.call("txn_post", posting("before-backup"), Some(writer))?)?["created"],
        true
    );
    db::VerifiedSnapshot::create(
        &ledger.path,
        Some(&SecretString::from(PASSPHRASE)),
        &backup,
        db::SnapshotOverwrite::Refuse,
    )?;
    let input = json!({"input": backup});
    let original = std::fs::read(&backup)?;
    ledger.enforce()?;
    assert_eq!(
        payload(&server.call("txn_post", posting("after-backup"), Some(writer))?)?["created"],
        true
    );
    let cutoff = json!({"company":"acme", "through":"2026-11-20"});
    let close = server.call("period_close", cutoff, Some(admin))?;
    assert_ne!(close["result"]["isError"], true, "{close}");
    ledger.revoke(ledger.reader["token_jti"].as_str().ok_or("jti")?)?;

    let observer = ledger.db()?;
    let audit = |db: &Db| -> TestResult<Vec<(i64, String)>> {
        let mut rows = db
            .read()
            .prepare("SELECT id, row_hash FROM audit_log ORDER BY id")?;
        Ok(rows
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let before = audit(&observer)?;
    let denied = server.call("restore", input.clone(), Some(writer))?;
    assert_eq!(payload(&denied)?["code"], "FORBIDDEN");

    let response = server.call("restore", input.clone(), Some(admin))?;
    let restored = payload(&response)?;
    assert_ne!(response["result"]["isError"], true, "{restored}");
    let undo = restored["undo_id"].as_i64().ok_or("missing undo id")?;
    assert_eq!(restored["restore_id"], undo);
    assert_eq!(restored["verification"]["row_counts"]["transactions"], 1);
    assert_eq!(
        restored["verification"]["desktop_view"]["link"]["view_id"],
        "review"
    );
    assert_eq!(std::fs::read(&backup)?, original);
    assert_eq!(&audit(&observer)?[..before.len()], &before);
    assert!(db::policy::get(observer.read())?.enforce);
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(revoked))?
            .status(),
        401
    );

    reject_unsupported_restore_inputs(&ledger, &mut server, admin, &backup)?;
    assert_eq!(audit(&observer)?.len(), before.len() + 1);

    let undo_output = ledger
        .command()
        .args([
            "--agent-token",
            admin,
            "--json",
            "restore-undo",
            &undo.to_string(),
        ])
        .output()?
        .assert()
        .success();
    let undone: Value = serde_json::from_slice(&undo_output.get_output().stdout)?;
    assert_eq!(undone["data"]["undo_of"], undo);
    assert_eq!(
        undone["data"]["verification"]["row_counts"]["transactions"],
        2
    );
    let all = audit(&observer)?;
    assert_eq!(&all[..before.len()], &before);
    assert_eq!(all.len(), before.len() + 2);
    assert!(db::policy::get(observer.read())?.enforce);
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(revoked))?
            .status(),
        401
    );

    let redo_id = &undone["data"]["undo_id"];
    assert_restore_redo(&mut server, &observer, &ledger, redo_id)?;
    server.finish(&[admin, writer, revoked])
}

fn reject_unsupported_restore_inputs(
    ledger: &Ledger,
    server: &mut Server,
    admin: &str,
    backup: &std::path::Path,
) -> TestResult {
    let newer = ledger.dir.path().join("newer.bk-backup");
    std::fs::copy(backup, &newer)?;
    let future = rusqlite::Connection::open(&newer)?;
    future.pragma_update(None, "key", PASSPHRASE)?;
    future.execute(
        "INSERT INTO schema_version VALUES (?1)",
        [i64::from(db::current_schema_version()) + 1],
    )?;
    drop(future);
    let denied = server.call("restore", json!({"input": newer}), Some(admin))?;
    assert_eq!(payload(&denied)?["code"], "DATABASE");

    let mut read_only = Server::start(ledger, true, None)?;
    read_only.initialize(Some(admin), "Read-only restore test")?;
    let tools = read_only.tools(Some(admin))?;
    for tool in ["restore", "restore_undo"] {
        assert!(!tools.iter().any(|name| name == tool));
    }
    read_only.finish(&[admin])
}

fn assert_restore_redo(
    server: &mut Server,
    observer: &Db,
    ledger: &Ledger,
    redo_id: &Value,
) -> TestResult {
    let admin = token(&ledger.admin)?;
    let writer = token(&ledger.writer)?;
    Granted::<Admin>::with_db(observer, &ledger.owner, |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "post_restore_reader",
                capabilities: &["ledger/read".into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
        )
    })?;

    let redo = server.call("restore_undo", json!({"undo_id": redo_id}), Some(admin))?;
    assert_eq!(
        payload(&redo)?["verification"]["row_counts"]["transactions"],
        1
    );
    let posted = server.call("txn_post", posting("after-redo"), Some(writer))?;
    assert_eq!(payload(&posted)?["created"], true);
    let blocked = server.call(
        "restore_undo",
        json!({"undo_id": payload(&redo)?["undo_id"]}),
        Some(admin),
    )?;
    assert_eq!(payload(&blocked)?["code"], "VALIDATION");
    let abandoned: i64 = observer.read().query_row(
        "SELECT CAST(entity_id AS INTEGER) FROM audit_log WHERE action = 'create' AND entity = 'transaction' ORDER BY id LIMIT 1 OFFSET 1",
        [], |row| row.get(0),
    )?;
    let fresh: i64 = observer.read().query_row(
        "SELECT id FROM transactions WHERE reference = 'after-redo'",
        [],
        |row| row.get(0),
    )?;
    assert!(fresh > abandoned);
    db::verification::VerifiedLedger::read(observer)?;
    Ok(())
}
