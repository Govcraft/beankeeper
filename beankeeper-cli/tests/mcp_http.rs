//! Exercise the real Streamable HTTP endpoint and request-scoped credentials.

use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, IssueRequest};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const PASSPHRASE: &str = "http-test-passphrase";

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
    secret: PathBuf,
    reader: Value,
    writer: Value,
}

impl Ledger {
    fn new() -> TestResult<Self> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let secret = dir.path().join("passphrase");
        std::fs::write(&secret, PASSPHRASE)?;
        let db = Db::open(&path, Some(&PASSPHRASE.into()))?;
        db::create_company(db.conn(), "acme", "Acme", None)?;
        db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
        db::create_account(db.conn(), "acme", "4000", "Sales", "revenue", None)?;
        identity::init_root(db.conn(), &Actor::new("owner"))?;
        let issue = |name: &str, capabilities: &[String]| {
            identity::issue(
                db.conn(),
                &Actor::new("owner"),
                &IssueRequest {
                    name,
                    capabilities,
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        };
        let reader = issue("http_reader", &["ledger/read".into()])?;
        let writer = issue("http_writer", &["ledger/read".into(), "ledger/post".into()])?;
        Ok(Self {
            _dir: dir,
            path,
            secret,
            reader,
            writer,
        })
    }

    fn db(&self) -> TestResult<Db> {
        Ok(Db::open_existing(&self.path, Some(&PASSPHRASE.into()))?)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
        command
            .env_remove("BK_AGENT_TOKEN")
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_COMPANY")
            .arg("--db")
            .arg(&self.path)
            .arg("--passphrase-file")
            .arg(&self.secret);
        command
    }

    fn enforce(&self) -> TestResult {
        db::audit::set_policy(self.db()?.conn(), &Actor::new("owner"), true)?;
        Ok(())
    }
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
        let startup = server.diagnostics.recv_timeout(Duration::from_secs(20))?;
        server.endpoint = startup
            .strip_prefix("MCP Streamable HTTP listening on ")
            .ok_or("missing bound address")?
            .to_string();
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
    let (actor, agent): (String, String) = db.conn().query_row(
        "SELECT posted_by, agent_uri FROM transactions WHERE reference = 'http-sale'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(actor, "mcp:http-client-one");
    assert_eq!(agent, ledger.reader["agent_uri"]);
    db::audit::verify_hash_chain(db.conn())?;
    db::audit::verify_creations(db.conn())?;
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
    identity::revoke(
        db.conn(),
        &Actor::new("owner"),
        ledger.reader["token_jti"].as_str().ok_or("jti")?,
    )?;
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(reader))?
            .status(),
        401
    );
    server.finish(&[reader, "invalid-secret-token"])
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
    let agent: String = db.conn().query_row(
        "SELECT agent_uri FROM transactions WHERE reference = 'allowed'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(agent, ledger.writer["agent_uri"]);
    let count: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    server.finish(&[reader, writer])
}

#[test]
fn http_proposal_discovery_and_resolution_follow_request_grants() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let issue = |name: &str, grant: &str| {
        identity::issue(
            db.conn(),
            &Actor::new("owner"),
            &IssueRequest {
                name,
                capabilities: &[grant.into()],
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )
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
    let posted_by: String = db.conn().query_row(
        "SELECT agent_uri FROM transactions WHERE reference = 'http-proposal'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(posted_by, approver["agent_uri"]);
    db::audit::verify_hash_chain(db.conn())?;
    db::audit::verify_creations(db.conn())?;
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
            beankeeper_cli::capability::Capability::for_operation(&name)?,
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
    identity::revoke(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        ledger.writer["token_jti"].as_str().ok_or("jti")?,
    )?;
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
    let issued = identity::issue(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        &IssueRequest {
            name: "short_lived_http",
            capabilities: &["ledger/read".into()],
            ttl: Duration::from_secs(3),
            public_key: None,
            generate_key: true,
        },
    )?;
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
