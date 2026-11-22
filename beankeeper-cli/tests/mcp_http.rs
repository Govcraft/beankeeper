//! Exercise the real Streamable HTTP endpoint and request-scoped credentials.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, IssueRequest};
use reqwest::blocking::{Client, Response};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const PASSPHRASE: &str = "http-test-passphrase";

struct Ledger {
    dir: TempDir,
    path: PathBuf,
    secret: PathBuf,
    reader: Value,
    writer: Value,
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
        db::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
        db::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "1000",
                name: "Cash",
                account_type: "asset",
                default_tax_category: None,
            },
        )?;
        db::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "4000",
                name: "Sales",
                account_type: "revenue",
                default_tax_category: None,
            },
        )?;
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
            dir,
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
    db::audit::verify_hash_chain(db.read())?;
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
    db::audit::verify_hash_chain(db.read())?;
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

fn embedded_request(
    client: &Client,
    endpoint: &str,
    session: Option<&str>,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> TestResult<Response> {
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
    Ok(request.send()?)
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
        .conn()
        .query_row("SELECT count(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, i64::from(!read_only));
    db::audit::verify_hash_chain(db.read())?;
    db::audit::verify_creations(db.conn())?;
    assert_eq!(
        request(Some("invalid-token"), "tools/list", json!({}))?.status(),
        401
    );
    identity::revoke(
        db.conn(),
        &Actor::new("owner"),
        ledger.writer["token_jti"].as_str().ok_or("jti")?,
    )?;
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
            .conn()
            .query_row("SELECT slug FROM companies", [], |row| row.get(0))?;
        assert_eq!(slug, "acme");
        let error = snapshot
            .conn()
            .execute("UPDATE companies SET name = 'Changed'", [])
            .err()
            .ok_or("snapshot accepted a write")?;
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
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
    db::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
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
            .conn()
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
    let navigation = identity::issue(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        &IssueRequest {
            name: "navigator",
            capabilities: &["ui/navigate".into()],
            ttl: Duration::from_secs(3600),
            public_key: None,
            generate_key: true,
        },
    )?;
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
    identity::revoke(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        navigation["token_jti"].as_str().ok_or("jti")?,
    )?;
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
    for enforce in [true, false, true] {
        db::audit::set_policy(ledger.db()?.conn(), &Actor::new("owner"), enforce)?;
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
