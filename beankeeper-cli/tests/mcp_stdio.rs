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
        "BK_AGENT_TOKEN",
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
        "txn_reverse",
        "txn_meta_set",
        "txn_meta_unset",
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
fn tools_preserve_data_arrays_and_expose_pagination_and_currency_displays() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for currency in ["USD", "JPY", "BHD"] {
        let mut input = posting(currency);
        input["currency"] = json!(currency);
        input["debits"][0]["amount"] = json!("1800");
        input["credits"][0]["amount"] = json!("1800");
        client.success("txn_post", &input)?;
        let result = client.call("txn_list", &json!({"company":"acme", "reference":currency}))?;
        assert_eq!(result["result"]["_meta"]["pagination"]["total"], 1);
        assert_eq!(result["result"]["_meta"]["pagination"]["has_more"], false);
        let expected = match currency {
            "JPY" => "1800",
            "BHD" => "1800.000",
            _ => "1800.00",
        };
        assert_eq!(
            payload(&result)?[0]["entries"][0]["amount_display"],
            expected
        );
        let report = client.success(
            "report",
            &json!({"company":"acme", "kind":"trial-balance", "currency":currency}),
        )?;
        assert_eq!(report["total_debits_display"], expected);
    }
    for tool in ["txn_list", "audit_log"] {
        for (limit, offset, returned, has_more) in [
            (4, 0, 3, false),
            (3, 0, 3, false),
            (2, 0, 2, true),
            (2, 2, 1, false),
            (2, 5, 0, false),
        ] {
            let mut args = json!({"company":"acme", "limit":limit, "offset":offset});
            if tool == "audit_log" {
                args["action"] = json!("create");
                args["entity"] = json!("transaction");
            }
            let response = client.call(tool, &args)?;
            assert_ne!(response["result"]["isError"], true, "{response}");
            assert_eq!(
                response["result"]["_meta"]["pagination"],
                json!({"limit":limit, "offset":offset, "returned":returned, "total":3, "has_more":has_more})
            );
            let data = payload(&response)?;
            assert_eq!(
                data.as_array().map(Vec::len),
                usize::try_from(returned).ok()
            );
            if tool == "audit_log" && returned > 0 {
                assert!(data[0]["after"].is_string());
                assert!(data[0]["after_display"]["entries"][0]["amount_display"].is_string());
            }
        }
    }
    let cleared = client.success(
        "txn_clear",
        &json!({"company":"acme", "reference":"BHD", "account":"1000",
        "amount":"1.234", "date":"2026-03-02", "statement":"S1"}),
    )?;
    assert_eq!(cleared["amount"], 1234);
    assert_eq!(cleared["amount_display"], "1.234");
    assert_eq!(cleared["partially_cleared"], true);
    let budget = client.success(
        "budget_set",
        &json!({"company":"acme", "account":"1000", "currency":"BHD",
        "year":2026, "month":3, "amount":"12.345"}),
    )?;
    assert_eq!(budget[0]["amount_display"], "12.345");
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
fn mcp_writes_have_chained_audit_records_and_pass_cli_verification() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    client.success(
        "account_create",
        &json!({"company":"acme", "code":"5000", "name":"Rent", "account_type":"expense"}),
    )?;
    client.success(
        "budget_set",
        &json!({"company":"acme", "account":"5000", "year":2026, "month":3, "amount":"100"}),
    )?;
    client.success(
        "budget_delete",
        &json!({"company":"acme", "account":"5000", "year":2026, "month":3}),
    )?;
    let posted = client.success("txn_post", &posting("audited-sale"))?;
    client.success(
        "txn_clear",
        &json!({"company":"acme", "reference":"audited-sale", "account":"1000",
            "amount":"5", "date":"2026-03-02", "statement":"march"}),
    )?;
    let reversal_args = json!({"company":"acme", "id":posted["id"],
        "date":"2026-03-03", "reference":"audited-reversal"});
    let reversed = client.success("txn_reverse", &reversal_args)?;
    let audit = client.success("audit_log", &json!({}))?;
    let rows = audit.as_array().ok_or("missing audit rows")?;
    assert_eq!(rows.len(), 10, "{audit}");
    let mut previous = "0".repeat(64);
    for row in rows.iter().rev() {
        if row["id"].as_i64().is_some_and(|id| id > 3) {
            assert_eq!(row["actor"], "mcp:codex-test-client");
        }
        assert_eq!(row["prev_hash"], previous);
        let hash = row["row_hash"].as_str().ok_or("missing row hash")?;
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        previous = hash.to_owned();
    }
    let creations = client.success("audit_log", &json!({"action":"create"}))?;
    assert_eq!(creations.as_array().map(Vec::len), Some(6));
    for id in [&posted["id"], &reversed["id"]] {
        let shown = client.success("txn_show", &json!({"company":"acme", "id":id}))?;
        assert_eq!(shown[0]["posted_by"], "mcp:codex-test-client");
        let entity_id = id.to_string();
        assert!(
            creations
                .as_array()
                .ok_or("missing creations")?
                .iter()
                .any(|row| { row["entity_id"] == entity_id && row["after"].is_string() })
        );
    }
    assert_eq!(
        client.success("txn_post", &posting("audited-sale"))?["skipped"],
        true
    );
    assert_eq!(
        client.success("txn_reverse", &reversal_args)?["skipped"],
        true
    );
    assert_eq!(client.success("audit_log", &json!({}))?, audit);
    assert_eq!(client.success("verify", &json!({}))?["status"], "healthy");
    drop(client);
    bk(&path)?.args(["--json", "verify"]).assert().success();
    Ok(())
}

#[test]
fn settlement_report_schema_and_stdio_preserve_statement_charge_applications() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let report_tool = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "report")
        .ok_or("missing report tool")?;
    let schema = report_tool["inputSchema"].to_string();
    for field in ["mode", "bank", "settlement", "statement", "link_key"] {
        assert!(schema.contains(&format!("\"{field}\"")), "{schema}");
    }
    client.success(
        "account_create",
        &json!({"company":"acme", "code":"1200", "name":"Receivables", "account_type":"asset"}),
    )?;
    let mut charge = posting("charge");
    charge["debits"][0]["account"] = json!("1200");
    client.success("txn_post", &charge)?;
    let mut payment = posting("payment");
    payment["debits"][0]["amount"] = json!("10.00");
    payment["credits"][0] = json!({"account":"1200", "amount":"10.00"});
    payment["meta"] = json!({"settles":"charge"});
    client.success("txn_post", &payment)?;
    client.success(
        "txn_clear",
        &json!({"company":"acme", "reference":"charge", "account":"1200", "amount":"10.00",
            "date":"2026-03-02", "statement":"march"}),
    )?;
    let arguments = json!({"company":"acme", "kind":"reconciliation", "account":"1200",
        "mode":"settlement", "statement":"march", "link_key":"settles",
        "statement_balance":"10.00", "to":"2026-03-31"});
    let report = client.success("report", &arguments)?;
    assert_eq!(report["mode"], "settlement");
    assert_eq!(report["link_key"], "settles");
    assert_eq!(report["statement"], "march");
    assert_eq!(report["reconciled"], true);
    assert_eq!(report["difference"], 0);
    assert_eq!(report["uncleared_total"], 250);
    assert_eq!(report["charges"][0]["reference"], "charge");
    assert_eq!(report["charges"][0]["expected"], 1250);
    assert_eq!(report["charges"][0]["applied_this_statement"], 1000);
    assert_eq!(report["charges"][0]["short_pay"], 250);
    assert_eq!(
        report["applied_settlements"].as_array().map(Vec::len),
        Some(1)
    );
    let output = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "report",
            "reconciliation",
            "--account",
            "1200",
            "--mode",
            "settlement",
            "--statement",
            "march",
            "--link-key",
            "settles",
            "--statement-balance",
            "10.00",
            "--to",
            "2026-03-31",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report, cli["data"]);
    let mut other_statement = arguments.clone();
    other_statement["statement"] = json!("april");
    let other_report = client.success("report", &other_statement)?;
    assert_eq!(other_report["charges"][0]["applied_this_statement"], 0);
    assert_eq!(other_report["reconciled"], false);
    let mut bank = arguments;
    bank["mode"] = json!("bank");
    let error = client.call("report", &bank)?;
    assert_eq!(error["result"]["isError"], true);
    assert_eq!(payload(&error)?["code"], "USAGE");
    bank.as_object_mut()
        .ok_or("missing report arguments")?
        .remove("link_key");
    let bank_report = client.success("report", &bank)?;
    assert!(bank_report.get("charges").is_none());
    assert_eq!(bank_report["cleared_balance"], 1000);
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
        "txn_reverse",
        "txn_meta_set",
        "txn_meta_unset",
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
fn reversal_mirrors_entries_and_retries_with_the_mcp_actor() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let posted = client.success("txn_post", &posting("sale-to-reverse"))?;
    let show = json!({"company":"acme", "id":posted["id"]});
    let before = client.success("txn_show", &show)?;
    let arguments = json!({"company":"acme", "id":posted["id"],
        "date":"2026-03-02", "description":"Correct the sale", "reference":"void-sale"});
    let created = client.success("txn_reverse", &arguments)?;
    assert_eq!(created["created"], true);
    assert_eq!(created["skipped"], false);
    let retry = client.success("txn_reverse", &arguments)?;
    assert_eq!(retry["created"], false);
    assert_eq!(retry["skipped"], true);
    assert_eq!(retry["existing_id"], created["id"]);
    let original = client.success("txn_show", &show)?;
    let reversal = client.success("txn_show", &json!({"company":"acme", "id":created["id"]}))?;
    assert_eq!(original[0]["entries"], before[0]["entries"]);
    let original_meta: Value = serde_json::from_str(
        original[0]["metadata"]
            .as_str()
            .ok_or("missing original metadata")?,
    )?;
    let reversal_meta: Value = serde_json::from_str(
        reversal[0]["metadata"]
            .as_str()
            .ok_or("missing reversal metadata")?,
    )?;
    assert_eq!(original_meta["reversed_by"], created["id"]);
    assert_eq!(reversal_meta["reverses"], posted["id"]);
    assert_eq!(reversal[0]["reference"], "void-sale");
    assert_eq!(reversal[0]["date"], "2026-03-02");
    assert_eq!(reversal[0]["description"], "Correct the sale");
    let entries = before[0]["entries"]
        .as_array()
        .ok_or("missing original entries")?;
    let reversed = reversal[0]["entries"]
        .as_array()
        .ok_or("missing reversal entries")?;
    assert_eq!(entries.len(), reversed.len());
    for (old, new) in entries.iter().zip(reversed) {
        for key in ["account_code", "amount", "memo", "tax_category"] {
            assert_eq!(new[key], old[key]);
        }
        assert_ne!(new["direction"], old["direction"]);
        assert_eq!(new["status"], "uncleared");
        assert_eq!(new["cleared_amount"], 0);
    }
    let transactions = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(2));
    let audit = client.success("audit_log", &json!({"company":"acme", "action":"reverse"}))?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["actor"], "mcp:codex-test-client");
    assert_eq!(audit[0]["entity_id"], posted["id"].to_string());
    assert!(client.success("verify", &json!({}))?.is_object());
    Ok(())
}

#[test]
fn read_only_rejects_reversal_without_changing_the_original() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut writer = Client::spawn(command)?;
    writer.initialize()?;
    let posted = writer.success("txn_post", &posting("read-only-reversal"))?;
    drop(writer);
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    assert!(!tool_names(&listed)?.contains("txn_reverse"));
    let show = json!({"company":"acme", "id":posted["id"]});
    let before = client.success("txn_show", &show)?;
    let response = client.call(
        "txn_reverse",
        &json!({"company":"acme",
        "id":posted["id"], "date":"2026-03-02", "reference":"forbidden-reversal"}),
    )?;
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(client.success("txn_show", &show)?, before);
    let transactions = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(1));
    assert_eq!(
        client.success("audit_log", &json!({"action":"reverse"}))?,
        json!([])
    );
    Ok(())
}

#[test]
fn startup_missing_database_and_stdin_passphrase_fail_without_protocol_output() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("missing.db");
    let output = bk(&path)?.args(["--json", "mcp", "serve"]).output()?;
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(output.stdout, b"");
    assert!(!path.exists());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "NOT_FOUND");
    let output = bk(&path)?
        .args(["--passphrase-fd", "0", "mcp", "serve"])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"");
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
    assert_eq!(output.stdout, b"");
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
    assert_ne!(
        audit.as_array().ok_or("missing audit rows")?.as_slice(),
        [] as [Value; 0]
    );
    client.success("txn_reconcile", &json!({}))?;
    Ok(())
}

/// Build an encrypted server ledger and an optional identity without shell credentials.
fn identity_ledger(path: &Path, passphrase_file: &Path) -> Result<Value, Box<dyn Error>> {
    use beankeeper_cli::db::{self, Actor, Db};
    use beankeeper_cli::identity::{self, IssueRequest};

    std::fs::write(passphrase_file, "mcp-test-passphrase")?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open(path, Some(&passphrase))?;
    db::create_company(db.conn(), "acme", "Acme", None)?;
    db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
    db::create_account(db.conn(), "acme", "4000", "Sales", "revenue", None)?;
    identity::init_root(db.conn(), &Actor::new("owner"))?;
    Ok(identity::issue(
        db.conn(),
        &Actor::new("owner"),
        &IssueRequest {
            name: "mcp_agent",
            capabilities: &["ledger/read".into()],
            ttl: Duration::from_secs(3600),
            public_key: None,
            generate_key: true,
        },
    )?)
}

#[test]
fn session_identity_is_recorded_and_live_revocation_blocks_the_next_tool_call() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("identity.db");
    let passphrase_file = dir.path().join("passphrase");
    let issued = identity_ledger(&path, &passphrase_file)?;
    let token = issued["token"].as_str().ok_or("missing token")?;
    let agent = issued["agent_uri"].as_str().ok_or("missing agent URI")?;
    let mut command = bk(&path)?;
    command
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .env("BK_AGENT_TOKEN", token)
        .args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    // A read-only grant still allows writes: enforcement is outside Phase 1.
    let posted = client.success("txn_post", &posting("attested-mcp"))?;
    let shown = client.success("txn_show", &json!({"company":"acme", "id":posted["id"]}))?;
    assert_eq!(shown[0]["agent_uri"], agent);
    assert_eq!(shown[0]["token_jti"], issued["token_jti"]);
    assert_eq!(shown[0]["posted_by"], "mcp:codex-test-client");
    let audit = client.success("audit_log", &json!({"agent":agent}))?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["agent_uri"], agent);
    assert_eq!(audit[0]["token_jti"], issued["token_jti"]);
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args(["agent", "revoke", agent])
        .assert()
        .success();
    let rejected = client.request(
        "tools/call",
        &json!({"name":"txn_post", "arguments":posting("revoked-mcp")}),
    )?;
    assert_eq!(rejected["result"]["isError"], true);
    let payload: Value = serde_json::from_str(
        rejected["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing error text")?,
    )?;
    assert_eq!(payload["code"], "UNAUTHORIZED");
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .arg("verify")
        .assert()
        .success();
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = beankeeper_cli::db::Db::open_existing(&path, Some(&passphrase))?;
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn invalid_session_token_fails_before_mcp_protocol_starts() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("identity.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let output = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args(["--json", "--agent-token", "invalid", "mcp", "serve"])
        .output()?;
    assert_eq!(output.status.code(), Some(6));
    assert_eq!(output.stdout, Vec::<u8>::new());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "UNAUTHORIZED");
    Ok(())
}

#[test]
fn metadata_tools_share_cli_rules_and_audit_the_mcp_actor() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let id = client.success("txn_post", &posting("annotate"))?["id"].clone();
    let set = json!({"company":"acme", "id":id, "values":["status=disputed", "dispute_id=D=42"]});
    let updated = client.success("txn_meta_set", &set)?;
    assert_eq!(updated["id"], id);
    assert_eq!(updated["changed"], true);
    assert_eq!(client.success("txn_meta_set", &set)?["changed"], false);
    let audit = client.success("audit_log", &json!({"action":"metadata_set"}))?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["actor"], "mcp:codex-test-client");
    let bad = client.call(
        "txn_meta_unset",
        &json!({"company":"acme", "reference":"annotate", "keys":["status", "applies_to"]}),
    )?;
    assert_eq!(payload(&bad)?["code"], "VALIDATION");
    assert_eq!(
        client.success(
            "txn_meta_unset",
            &json!({"company":"acme", "reference":"annotate", "keys":["status", "dispute_id"]})
        )?["changed"],
        true
    );
    assert_eq!(
        client
            .success("audit_log", &json!({"action":"metadata_unset"}))?
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let mut unsupported = posting("unsupported");
    unsupported["on_conflict"] = "upsert".into();
    let rejected = client.call("txn_post", &unsupported)?;
    assert!(rejected.get("error").is_some() || rejected["result"]["isError"] == true);
    client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn historical_trends_over_stdio_match_cli_payloads() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    let schema = tools
        .iter()
        .find(|tool| tool["name"] == "report")
        .ok_or("missing report tool")?["inputSchema"]
        .to_string();
    assert!(schema.contains("net-worth"), "{schema}");
    assert!(schema.contains("spending"), "{schema}");
    for metric in ["spending", "net-worth"] {
        let report = client.success(
            "report",
            &json!({
                "company":"acme", "kind":"trend", "metric":metric,
                "from":"2026-01-01", "to":"2026-03-31", "period":"month"
            }),
        )?;
        let output = bk(&path)?
            .args([
                "--json",
                "--company",
                "acme",
                "report",
                "trend",
                "--metric",
                metric,
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
                "--period",
                "month",
            ])
            .output()?;
        assert!(output.status.success());
        let cli: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report, cli["data"]);
    }
    Ok(())
}
