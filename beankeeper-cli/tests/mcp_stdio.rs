//! Exercise the real MCP server over its newline-delimited stdio transport.

use std::collections::BTreeSet;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use assert_cmd::prelude::*;
use serde_json::{Value, json};
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn Error>>;

fn bk(path: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command.arg("--db").arg(path).stdin(Stdio::null());
    for name in [
        "BEANKEEPER_PASSPHRASE_CMD",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BK_ACTOR",
    ] {
        command.env_remove(name);
    }
    Ok(command)
}

fn seed(path: &Path) -> TestResult {
    for args in [
        vec!["init"],
        vec!["company", "create", "acme", "Acme"],
        vec![
            "--company",
            "acme",
            "account",
            "create",
            "1000",
            "Cash",
            "--type",
            "asset",
        ],
        vec![
            "--company",
            "acme",
            "account",
            "create",
            "4000",
            "Sales",
            "--type",
            "revenue",
        ],
    ] {
        bk(path)?.args(args).assert().success();
    }
    Ok(())
}

struct Client {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Result<Value, String>>,
    next_id: u64,
}

impl Client {
    fn spawn(mut command: Command) -> Result<Self, Box<dyn Error>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().ok_or("missing server stdin")?;
        let output = child.stdout.take().ok_or("missing server stdout")?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|error| format!("non-JSON stdout: {error}: {line}"))
                });
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            messages,
            next_id: 0,
        })
    }

    fn notify(&mut self, method: &str, params: &Value) -> TestResult {
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0", "method":method, "params":params})
        )?;
        self.input.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, Box<dyn Error>> {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )?;
        self.input.flush()?;
        loop {
            let message = self.messages.recv_timeout(Duration::from_secs(20))??;
            assert_eq!(message["jsonrpc"], "2.0", "unexpected stdout: {message}");
            if message["id"] == id {
                return Ok(message);
            }
        }
    }

    fn initialize(&mut self) -> TestResult {
        let response = self.request(
            "initialize",
            &json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"Codex Test/Client", "version":"1.0"}
            }),
        )?;
        assert_eq!(response["result"]["serverInfo"]["name"], "beankeeper");
        assert!(response["result"]["capabilities"]["tools"].is_object());
        self.notify("notifications/initialized", &json!({}))
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, Box<dyn Error>> {
        self.request("tools/call", &json!({"name":name, "arguments":arguments}))
    }

    fn success(&mut self, name: &str, arguments: &Value) -> Result<Value, Box<dyn Error>> {
        let response = self.call(name, arguments)?;
        assert!(response.get("error").is_none(), "{name}: {response}");
        assert_ne!(response["result"]["isError"], true, "{name}: {response}");
        payload(&response)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn payload(response: &Value) -> Result<Value, Box<dyn Error>> {
    if let Some(value) = response["result"].get("structuredContent") {
        return Ok(value.clone());
    }
    Ok(serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing tool data")?,
    )?)
}

fn posting(reference: &str) -> Value {
    json!({"company":"acme", "description":"MCP sale", "date":"2026-03-01",
        "currency":"USD", "reference":reference, "on_conflict":"skip",
        "debits":[{"account":"1000", "amount":"12.50", "memo":"bank receipt"}],
        "credits":[{"account":"4000", "amount":"12.50"}], "meta":{"invoice":"one"}})
}

fn tool_names(response: &Value) -> Result<BTreeSet<&str>, Box<dyn Error>> {
    response["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .map(|tool| {
            assert_eq!(tool["inputSchema"]["type"], "object");
            tool["name"]
                .as_str()
                .ok_or_else(|| "missing tool name".into())
        })
        .collect()
}

#[test]
fn initialize_lists_tools_and_posts_lists_reports_and_retries() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed_tools = client.request("tools/list", &json!({}))?;
    let names = tool_names(&listed_tools)?;
    let expected: BTreeSet<_> = [
        "company_list",
        "company_show",
        "account_list",
        "account_show",
        "account_create",
        "txn_post",
        "txn_list",
        "txn_show",
        "report",
        "budget_list",
        "budget_set",
        "budget_delete",
        "txn_clear",
        "txn_reconcile",
        "verify",
        "audit_log",
    ]
    .into_iter()
    .collect();
    assert_eq!(names, expected);
    assert_eq!(
        client.success("company_list", &json!({}))?[0]["slug"],
        "acme"
    );
    let first = client.success("txn_post", &posting("mcp-invoice-1"))?;
    assert_eq!(first["created"], true);
    let retry = client.success("txn_post", &posting("mcp-invoice-1"))?;
    assert_eq!(retry["skipped"], true);
    assert_eq!(retry["existing_id"], first["id"]);
    let transactions = client.success(
        "txn_list",
        &json!({"company":"acme", "reference":"mcp-invoice-1"}),
    )?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(1));
    assert_eq!(transactions[0]["entries"][0]["amount"], 1250);
    assert_eq!(transactions[0]["entries"][0]["memo"], "bank receipt");
    let report = client.success(
        "report",
        &json!({"company":"acme", "kind":"trial-balance",
        "from":"2026-03-01", "to":"2026-03-31"}),
    )?;
    assert_eq!(report["total_debits"], 1250);
    assert_eq!(report["total_credits"], 1250);
    assert_eq!(report["balanced"], true);
    let output = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "report",
            "trial-balance",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
        ])
        .output()?;
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report, cli["data"]);
    assert!(client.success("verify", &json!({}))?.is_object());
    Ok(())
}

#[test]
fn tool_errors_preserve_codes_and_audited_writes_attribute_the_client() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let mut unbalanced = posting("bad-invoice");
    unbalanced["credits"][0]["amount"] = json!("1.00");
    let response = client.call("txn_post", &unbalanced)?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "VALIDATION");
    let unknown = client.call("account_list", &json!({"company":"missing"}))?;
    assert_eq!(unknown["result"]["isError"], true);
    assert_eq!(payload(&unknown)?["code"], "NOT_FOUND");
    let created = client.success(
        "account_create",
        &json!({"company":"acme", "code":"5000",
        "name":"Rent", "account_type":"expense"}),
    )?;
    assert_eq!(created[0]["code"], "5000");
    client.success(
        "budget_set",
        &json!({"company":"acme", "account":"5000", "year":2026,
        "month":3, "amount":"1500.00", "currency":"USD"}),
    )?;
    bk(&path)?
        .args(["company", "create", "other", "Other"])
        .assert()
        .success();
    client.success(
        "account_create",
        &json!({"company":"other", "code":"5000",
        "name":"Other Rent", "account_type":"expense"}),
    )?;
    client.success(
        "budget_set",
        &json!({"company":"other", "account":"5000", "year":2026,
        "month":3, "amount":"1.00", "currency":"USD"}),
    )?;
    let audit = client.success("audit_log", &json!({"company":"acme"}))?;
    assert!(
        audit
            .as_array()
            .ok_or("audit data is not an array")?
            .iter()
            .all(|row| { row["company_slug"] == "acme" }),
        "company filter returned another company's audit rows: {audit}"
    );
    assert!(
        audit
            .as_array()
            .ok_or("audit data is not an array")?
            .iter()
            .any(|row| {
                row["actor"] == "mcp:codex-test-client" && row["action"] == "budget_set"
            }),
        "audit: {audit}"
    );
    Ok(())
}

#[test]
fn read_only_router_omits_writes_and_rejects_direct_write_calls() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert_eq!(names.len(), 11);
    for name in [
        "account_create",
        "txn_post",
        "txn_clear",
        "budget_set",
        "budget_delete",
    ] {
        assert!(!names.contains(name));
    }
    assert!(names.contains("txn_reconcile"));
    let response = client.call("txn_post", &posting("read-only"))?;
    assert!(response.get("error").is_some() || response["result"]["isError"] == true);
    let transactions = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(0));
    Ok(())
}

#[test]
fn startup_missing_database_and_stdin_passphrase_fail_without_protocol_output() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("missing.db");
    let output = bk(&path)?.args(["--json", "mcp", "serve"]).output()?;
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    assert!(!path.exists());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "NOT_FOUND");
    let output = bk(&path)?
        .args(["--passphrase-fd", "0", "mcp", "serve"])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn encrypted_startup_accepts_file_and_command_and_rejects_wrong_key() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("encrypted.db");
    let secret = dir.path().join("passphrase");
    std::fs::write(&secret, "mcp transport test secret\n")?;
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&secret)
        .args(["init", "--encrypt"])
        .assert()
        .success();
    let mut command = bk(&path)?;
    command
        .arg("--passphrase-file")
        .arg(&secret)
        .args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    assert_eq!(client.success("company_list", &json!({}))?, json!([]));
    drop(client);
    let mut command = bk(&path)?;
    command
        .env(
            "BEANKEEPER_PASSPHRASE_CMD",
            "printf 'mcp transport test secret\\n'",
        )
        .args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    assert_eq!(client.success("company_list", &json!({}))?, json!([]));
    drop(client);
    std::fs::write(&secret, "incorrect secret\n")?;
    let output = bk(&path)?
        .arg("--passphrase-file")
        .arg(&secret)
        .args(["--json", "mcp", "serve"])
        .output()?;
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    // SQLCipher may log authentication diagnostics before the CLI envelope.
    // Only stdout is the MCP channel; these stderr diagnostics are allowed.
    let envelope = stderr
        .lines()
        .skip_while(|line| !line.starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n");
    let error: Value = serde_json::from_str(&envelope)?;
    assert_eq!(error["error"]["code"], "DATABASE");
    assert!(!stderr.contains("incorrect secret"));
    assert!(!stderr.contains("mcp transport test secret"));
    Ok(())
}

#[test]
fn clearing_batches_and_remaining_tools_use_the_shared_ledger() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    assert_eq!(
        client.success("company_show", &json!({"slug":"acme"}))?[0]["slug"],
        "acme"
    );
    assert_eq!(
        client.success("account_show", &json!({"company":"acme", "code":"1000"}))?[0]["code"],
        "1000"
    );
    let accounts = client.success(
        "account_list",
        &json!({"company":"acme", "account_type":"asset", "with_balances":true}),
    )?;
    assert_eq!(accounts.as_array().map(Vec::len), Some(1));
    client.success(
        "budget_set",
        &json!({"company":"acme", "account":"1000", "year":2026, "annual":"1200"}),
    )?;
    assert_eq!(
        client
            .success("budget_list", &json!({"company":"acme", "year":2026}))?
            .as_array()
            .map(Vec::len),
        Some(12)
    );
    client.success(
        "budget_delete",
        &json!({"company":"acme", "account":"1000", "year":2026}),
    )?;
    assert_eq!(
        client.success("budget_list", &json!({"company":"acme", "year":2026}))?,
        json!([])
    );
    let first = client.success("txn_post", &posting("clear-one"))?;
    client.success("txn_post", &posting("clear-two"))?;
    let failed_batch = client.call("txn_clear", &json!({"company":"acme", "account":"1000", "date":"2026-03-02", "batch":[{"reference":"clear-one", "amount":"5"}, {"reference":"missing"}]}))?;
    assert_eq!(payload(&failed_batch)?["code"], "NOT_FOUND");
    let shown = client.success("txn_show", &json!({"company":"acme", "id":first["id"]}))?;
    let entry = shown[0]["entries"]
        .as_array()
        .ok_or("missing entries")?
        .iter()
        .find(|entry| entry["account_code"] == "1000")
        .ok_or("missing cash entry")?;
    assert_eq!(entry["cleared_amount"], 0);
    let entry_id = entry["id"].clone();
    let batch = client.success("txn_clear", &json!({"company":"acme", "account":"1000", "date":"2026-03-02", "statement":"march", "batch":[{"reference":"clear-one", "amount":"5"}, {"reference":"clear-two"}]}))?;
    assert_eq!(batch["count"], 2);
    assert_eq!(batch["cleared_total"], 1750);
    let cleared = client.success("txn_clear", &json!({"company":"acme", "transaction_id":first["id"], "entry":entry_id, "date":"2026-03-03", "amount":"7.50", "statement":"march-final"}))?;
    assert_eq!(cleared["cleared_amount"], 1250);
    let audit = client.success(
        "audit_log",
        &json!({"company":"acme", "action":"status-change"}),
    )?;
    assert!(
        audit
            .as_array()
            .ok_or("missing audit rows")?
            .iter()
            .all(|row| row["actor"] == "mcp:codex-test-client")
    );
    assert!(!audit.as_array().ok_or("missing audit rows")?.is_empty());
    client.success("txn_reconcile", &json!({}))?;
    Ok(())
}
