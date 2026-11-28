//! Exercise the real MCP server over its newline-delimited stdio transport.

#[path = "mcp_stdio/account_rename.rs"]
mod account_rename;
#[path = "mcp_stdio/audit_receipts.rs"]
mod audit_receipts;
#[path = "mcp_stdio/cash_classification.rs"]
mod cash_classification;
#[path = "mcp_stdio/contractor_tax.rs"]
mod contractor_tax;
#[path = "mcp_stdio/corrections.rs"]
mod corrections;
#[path = "mcp_stdio/counterparties.rs"]
mod counterparties;
#[path = "mcp_stdio/documents.rs"]
mod documents;

#[path = "support/legacy_schema.rs"]
mod legacy_schema;

#[path = "mcp_stdio/expense_claims.rs"]
mod expense_claims;
mod fixed_assets;
#[path = "mcp_stdio/fx.rs"]
mod fx;

#[path = "mcp_stdio/holds.rs"]
mod holds;

#[path = "mcp_stdio/onboarding.rs"]
mod onboarding;

#[path = "mcp_stdio/schema_compat.rs"]
mod schema_compat;

#[path = "mcp_stdio/txn_links.rs"]
mod txn_links;

use beankeeper_cli::db::CreateAccountParams;
use beankeeper_cli::identity::{Admin, Granted};
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

const BLANK_COMPANY_NAMES: [&str; 2] = ["", " \t\u{2003}\n"];
const COMPANY_NAME_ERROR: &str = "name must not be empty";

#[test]
fn named_stdio_tools_identify_their_ledger() -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_META_KEY;

    let directory = tempdir()?;
    let demo = directory.path().join("demo.db");
    seed(&demo)?;
    let mut command = bk(&directory.path().join("startup.db"))?;
    command
        .arg("--ledger")
        .arg(format!("demo={}", demo.display()))
        .args(["--ledger-target", "demo", "mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    assert_eq!(listed["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
    let result = client.call("company_list", &json!({}))?;
    assert_eq!(result["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
    assert_eq!(payload(&result)?[0]["slug"], "acme");
    Ok(())
}

fn bk(path: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command.arg("--db").arg(path).stdin(Stdio::null());
    for name in [
        "BEANKEEPER_PASSPHRASE_CMD",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BK_ACTOR",
        "BK_AGENT_TOKEN",
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CODEX_THREAD_ID",
        "CODEX_CI",
        "CODEX_SANDBOX",
        "GEMINI_CLI",
        "CURSOR_AGENT",
        "AI_AGENT",
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
        vec!["--company", "acme", "account", "classify", "1000", "cash"],
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
        self.initialize_as("Codex Test/Client")
    }

    fn initialize_as(&mut self, name: &str) -> TestResult {
        let response = self.request(
            "initialize",
            &json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":name, "version":"1.0"}
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

fn assert_parameter_rejection(response: &Value, detail: &str) {
    assert_eq!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("failed to deserialize parameters:"),
        "{response}"
    );
    assert!(text.contains(detail), "{response}");
}

fn assert_cli_rejects_company_name(command: &mut Command) -> TestResult {
    let output = command.output()?;
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr)?;
    assert!(
        error.lines().any(|line| line.ends_with(COMPANY_NAME_ERROR)),
        "{error}"
    );
    Ok(())
}

fn posting(reference: &str) -> Value {
    json!({"company":"acme", "description":"MCP sale", "date":"2026-03-01",
        "currency":"USD", "reference":reference, "on_conflict":"skip",
        "debits":[{"account":"1000", "amount":"12.50", "memo":"bank receipt"}],
        "credits":[{"account":"4000", "amount":"12.50"}], "meta":{"invoice":"one"}})
}

#[test]
fn stdio_and_cli_share_configured_actor_attribution() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    for (label, explicit, client_name, account) in [
        ("operator-label", Some("  operator-label  "), "", "1010"),
        ("env-label", None, "Other Client", "1020"),
    ] {
        let configured_command = || {
            let mut command = bk(&path)?;
            command.env("BK_ACTOR", "env-label");
            if let Some(actor) = explicit {
                command.args(["--actor", actor]);
            }
            Ok::<_, Box<dyn Error>>(command)
        };
        configured_command()?
            .args([
                "--company",
                "acme",
                "account",
                "create",
                account,
                "Register",
                "--type",
                "asset",
            ])
            .assert()
            .success();
        let mut command = configured_command()?;
        command.args(["mcp", "serve"]);
        let mut client = Client::spawn(command)?;
        // The stdio handshake requires a name field, which can be empty.
        client.initialize_as(client_name)?;
        client.success("txn_post", &posting(label))?;
        let response = client.call("audit_log", &json!({"actor":label, "limit":1}))?;
        assert_eq!(response["result"]["_meta"]["pagination"]["total"], 2);
        assert_eq!(payload(&response)?[0]["actor"], label);
        let filtered = client.success("audit_log", &json!({"actor":label}))?;
        assert_eq!(filtered.as_array().map(Vec::len), Some(2));
        assert_eq!(filtered[0]["entity"], "transaction");
        assert_eq!(filtered[1]["entity"], "account");
        assert_eq!(filtered[1]["actor"], label);
        let cli_audit = bk(&path)?
            .args(["--json", "audit", "log", "--by-actor", label])
            .assert()
            .success();
        let cli_audit: Value = serde_json::from_slice(&cli_audit.get_output().stdout)?;
        assert_eq!(cli_audit["data"], filtered);
    }
    Ok(())
}

#[test]
fn cli_and_mcp_audit_show_the_same_unverified_runtime_hint() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let secret = "session-secret-that-must-never-be-stored";
    let with_markers = || -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&path)?;
        command
            .env("BK_ACTOR", "configured-operator")
            .env("AI_AGENT", secret)
            .env("CLAUDECODE", secret)
            .env("CLAUDE_CODE_ENTRYPOINT", secret);
        Ok(command)
    };
    let created = with_markers()?
        .args(["--json", "company", "create", "cli-hint", "CLI Hint"])
        .assert()
        .success();
    let notice = String::from_utf8_lossy(&created.get_output().stderr);
    assert!(notice.contains("Client hint: claude-code (unverified)"));
    assert!(!notice.contains(secret));
    with_markers()?
        .args(["--quiet", "company", "create", "quiet-hint", "Quiet Hint"])
        .assert()
        .success()
        .stderr("");

    let mut command = with_markers()?;
    command.args(["--quiet", "mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    client.success(
        "company_create",
        &json!({"slug":"mcp-hint", "name":"MCP Hint"}),
    )?;
    let mcp = client.success("audit_log", &json!({"actor":"configured-operator"}))?;
    let cli = with_markers()?
        .args([
            "--json",
            "audit",
            "log",
            "--by-actor",
            "configured-operator",
        ])
        .assert()
        .success();
    assert!(!String::from_utf8_lossy(&cli.get_output().stderr).contains("Client hint:"));
    let rendered = String::from_utf8_lossy(&cli.get_output().stdout);
    assert!(!rendered.contains(secret));
    let cli: Value = serde_json::from_str(&rendered)?;
    assert_eq!(cli["data"], mcp);
    let rows = mcp.as_array().ok_or("missing audit rows")?;
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(row["actor"], "configured-operator");
        assert!(row["agent_uri"].is_null());
        assert!(row["token_jti"].is_null());
        assert_eq!(row["hash_version"], 7);
        assert_eq!(
            row["client_hint"],
            json!({
                "verified": false, "source": "env", "runtime": "claude-code",
                "markers": ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"]
            })
        );
    }
    let raw = rusqlite::Connection::open(&path)?;
    let stored: String = raw.query_row(
        "SELECT group_concat(client_hint) FROM audit_log WHERE actor = 'configured-operator'",
        [],
        |row| row.get(0),
    )?;
    assert!(!stored.contains(secret));
    bk(&path)?.arg("verify").assert().success();
    assert!(
        raw.execute(
            "UPDATE audit_log SET client_hint = NULL WHERE company_slug = 'cli-hint'",
            [],
        )
        .is_err()
    );
    raw.execute_batch(
        "DROP TRIGGER protect_audit_update;
         UPDATE audit_log SET client_hint = NULL WHERE company_slug = 'cli-hint';",
    )?;
    let verify = bk(&path)?.arg("verify").assert().code(4);
    assert!(
        String::from_utf8_lossy(&verify.get_output().stderr).contains("audit hash chain broken")
    );
    Ok(())
}

#[test]
fn demo_provenance_matches_between_cli_and_mcp() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("demo.db");
    let operator = "demo-initializer";
    bk(&path)?
        .env("BK_ACTOR", "environment-actor")
        .env("USER", "os-user")
        .args(["--actor", operator, "init", "--demo"])
        .assert()
        .success();

    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let event = client.success("audit_log", &json!({"action":"demo_seed", "entity":"demo"}))?;
    let cli_event = bk(&path)?
        .args([
            "--json",
            "audit",
            "log",
            "--action",
            "demo_seed",
            "--entity",
            "demo",
        ])
        .output()?;
    assert!(cli_event.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cli_event.stdout)?["data"],
        event
    );
    assert_eq!(event.as_array().map(Vec::len), Some(1));
    assert_eq!(event[0]["actor"], operator);
    assert!(event[0]["company_slug"].is_null());
    assert_eq!(
        event[0]["after_display"]["message"],
        "Generated the built-in demo data"
    );

    let mut offset = 0;
    let mut companies = 0;
    let mut posts = 0;
    loop {
        let response = client.call("audit_log", &json!({"limit":100, "offset":offset}))?;
        let data = payload(&response)?;
        for row in data.as_array().ok_or("audit rows")? {
            match row["entity"].as_str() {
                Some("company") => {
                    companies += 1;
                    assert_eq!(row["actor"], operator);
                }
                Some("demo") => assert_eq!(row, &event[0]),
                _ => {
                    assert_eq!(row["actor"], "demo", "{row}");
                    if row["entity"] == "transaction" && row["action"] == "create" {
                        posts += 1;
                    }
                }
            }
        }
        let Some(next) = response["result"]["_meta"]["pagination"]["next_offset"].as_u64() else {
            break;
        };
        offset = next;
    }
    assert_eq!(companies, 3);
    let mut transaction_count = 0;
    for company in ["acme-consulting", "acme-products", "personal"] {
        let data = client.success("txn_list", &json!({"company":company}))?;
        for txn in data.as_array().ok_or("demo transactions")? {
            transaction_count += 1;
            assert_eq!(txn["posted_by"], "demo", "{txn}");
            let raw = txn["metadata"]
                .as_str()
                .ok_or("demo transaction metadata")?;
            let metadata: Value = serde_json::from_str(raw)?;
            assert_eq!(metadata["beankeeper_source"], "demo", "{txn}");
        }
    }
    assert!(transaction_count > 0);
    assert_eq!(posts, transaction_count);

    let rejected = client.call(
        "txn_meta_unset",
        &json!({"company":"acme-consulting", "reference":"AC-001", "keys":["beankeeper_source"]}),
    )?;
    assert_eq!(payload(&rejected)?["code"], "VALIDATION");
    assert_eq!(client.success("verify", &json!({}))?["status"], "healthy");
    bk(&path)?.arg("verify").assert().success();
    Ok(())
}

#[test]
fn cli_and_mcp_return_the_same_validation_error_for_blank_actor_filters() -> TestResult {
    use beankeeper_cli::{cli::Cli, db::BlankActorLabel};
    use clap::Parser as _;

    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    for blank in ["", "  \t", "\u{2003}"] {
        let Err(parse_error) = Cli::try_parse_from(["bk", "audit", "log", "--by-actor", blank])
        else {
            return Err("blank actor must fail in clap".into());
        };
        assert!(
            parse_error
                .source()
                .is_some_and(<dyn Error>::is::<BlankActorLabel>)
        );
        let cli = bk(&path)?
            .args(["--json", "audit", "log", "--by-actor", blank])
            .assert()
            .code(3);
        let cli: Value = serde_json::from_slice(&cli.get_output().stderr)?;
        let mcp = client.call("audit_log", &json!({"actor": blank}))?;
        assert_eq!(mcp["result"]["isError"], true);
        let mcp_error = payload(&mcp)?;
        assert_eq!(mcp_error["code"], "VALIDATION");
        assert_eq!(mcp_error["code"], cli["error"]["code"]);
        assert_parameter_rejection(&mcp, "actor label must not be blank");
        assert_eq!(cli["error"]["message"], "actor label must not be blank");
    }
    Ok(())
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

// Read repository docs at test time so crates.io packaging does not depend on
// files outside the CLI package. These guards run in the workspace CI suite.
fn repository_doc(path: &str) -> Result<String, Box<dyn Error>> {
    Ok(std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(path),
    )?)
}

fn doc_section<'a>(document: &'a str, heading: &str) -> &'a str {
    let (_, rest) = document.split_once(heading).unwrap_or_default();
    rest.split("\n## ")
        .next()
        .unwrap_or_default()
        .split("\n### ")
        .next()
        .unwrap_or_default()
}

fn inline_codes(text: &str) -> BTreeSet<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

fn catalog_names(section: &str) -> BTreeSet<&str> {
    section
        .lines()
        .filter(|line| line.starts_with('|'))
        .filter_map(|line| line.split('|').nth(1))
        .flat_map(inline_codes)
        .collect()
}

fn cli_report_kinds() -> Result<BTreeSet<String>, Box<dyn Error>> {
    use clap::CommandFactory;

    let cli = beankeeper_cli::cli::Cli::command();
    Ok(cli
        .find_subcommand("report")
        .ok_or("missing report command")?
        .get_subcommands()
        .map(|command| command.get_name().to_owned())
        .collect())
}

fn schema_report_kinds(schema: &Value, kinds: &mut BTreeSet<String>) {
    if let Some(kind) = schema
        .pointer("/properties/kind/const")
        .and_then(Value::as_str)
    {
        kinds.insert(kind.to_owned());
    }
    match schema {
        Value::Object(fields) => {
            for value in fields.values() {
                schema_report_kinds(value, kinds);
            }
        }
        Value::Array(values) => {
            for value in values {
                schema_report_kinds(value, kinds);
            }
        }
        _ => (),
    }
}

#[test]
fn documented_mcp_catalog_reports_and_read_only_exclusions_match_discovery() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let registered = tool_names(&listed)?;
    for path in ["README.md", "docs/beankeeper/references/commands.md"] {
        let document = repository_doc(path)?;
        assert_eq!(
            catalog_names(doc_section(&document, "### Available tools\n")),
            registered,
            "{path}: tool catalog differs from MCP discovery",
        );
    }

    let cli_kinds = cli_report_kinds()?;
    let report = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "report")
        .ok_or("missing report tool")?;
    let mut schema_kinds = BTreeSet::new();
    schema_report_kinds(&report["inputSchema"], &mut schema_kinds);
    assert_eq!(schema_kinds, cli_kinds, "MCP and CLI report kinds differ");
    let reference = repository_doc("docs/beankeeper/references/commands.md")?;
    let report_section = doc_section(&reference, "### Report kinds\n");
    let (kind_list, _) = report_section
        .split_once("including")
        .ok_or("missing report-kind list")?;
    let documented: BTreeSet<_> = inline_codes(kind_list)
        .into_iter()
        .filter(|name| *name != "report.kind")
        .map(str::to_owned)
        .collect();
    assert_eq!(documented, cli_kinds, "documented report kinds differ");
    assert!(
        repository_doc("README.md")?.contains(&format!("all {} CLI report kinds", cli_kinds.len()))
    );

    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut reader = Client::spawn(command)?;
    reader.initialize()?;
    let read_only = reader.request("tools/list", &json!({}))?;
    let retained = tool_names(&read_only)?;
    let excluded: BTreeSet<_> = registered.difference(&retained).copied().collect();
    let section = doc_section(&reference, "### Read-only tools\n");
    let (removed, _) = section
        .split_once("from the router.")
        .ok_or("missing read-only exclusion list")?;
    assert_eq!(
        inline_codes(removed),
        excluded,
        "documented read-only exclusions differ"
    );
    Ok(())
}

fn cli_leaf_paths(command: &clap::Command, prefix: &str) -> BTreeSet<String> {
    command
        .get_subcommands()
        .flat_map(|child| {
            let path = format!("{prefix} {}", child.get_name());
            if child.has_subcommands() {
                cli_leaf_paths(child, &path)
            } else {
                BTreeSet::from([path])
            }
        })
        .collect()
}

#[test]
fn cli_mutations_have_mcp_tools_or_documented_exclusions() -> TestResult {
    use beankeeper_cli::{capability::Capability, cli::Cli, operation::Operation};
    use clap::CommandFactory;

    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let tools = client.request("tools/list", &json!({}))?;
    let tool_operations = tool_names(&tools)?
        .into_iter()
        .map(str::parse)
        .collect::<Result<Vec<Operation>, _>>()?;

    let mut excluded = BTreeSet::new();
    for path in cli_leaf_paths(&Cli::command(), "bk") {
        let name = path.trim_start_matches("bk ").replace(' ', ".");
        let operation = name.parse::<Operation>()?;
        if operation == Operation::McpServe || operation.primary_capability()? == Capability::Read {
            continue;
        }
        if !tool_operations.contains(&operation) {
            excluded.insert(path);
        }
    }
    let reference = repository_doc("docs/beankeeper/references/commands.md")?;
    let documented = catalog_names(doc_section(
        &reference,
        "### Intentional CLI-only mutations\n",
    ));
    assert_eq!(
        excluded.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        documented,
        "every CLI mutation needs an MCP tool or an intentional exclusion in the command reference"
    );
    Ok(())
}

#[test]
fn skill_command_overview_covers_every_canonical_cli_operation() -> TestResult {
    use clap::CommandFactory;

    let skill = repository_doc("docs/beankeeper/SKILL.md")?;
    let section = doc_section(&skill, "## Command Overview\n");
    let documented: BTreeSet<String> = catalog_names(section)
        .into_iter()
        .flat_map(|code| {
            let (prefix, verbs) = code.rsplit_once(' ').unwrap_or((code, ""));
            verbs.split('/').map(move |verb| format!("{prefix} {verb}"))
        })
        .collect();
    let required = cli_leaf_paths(&beankeeper_cli::cli::Cli::command(), "bk");
    for path in required {
        assert!(
            documented
                .iter()
                .any(|code| code == &path || code.starts_with(&format!("{path} "))),
            "SKILL command overview omits {path}"
        );
    }
    Ok(())
}

const LEDGER_TOOLS: &[&str] = &[
    "security_create",
    "security_list",
    "security_price",
    "investment_buy",
    "investment_sell",
    "investment_lots",
    "investment_holdings",
    "period_close",
    "period_reopen",
    "period_list",
    "reconcile_statement",
    "reconcile_list",
    "company_list",
    "company_show",
    "company_create",
    "company_delete",
    "counterparty_create",
    "counterparty_amend",
    "counterparty_archive",
    "counterparty_associate",
    "counterparty_list",
    "counterparty_show",
    "counterparty_suggest",
    "tax_configure",
    "tax_list",
    "tax_report",
    "contractor_tax_review",
    "contractor_tax_worksheet",
    "contractor_tax_preview",
    "contractor_tax_w9",
    "contractor_tax_export",
    "document_show",
    "document_print",
    "document_list",
    "document_allocations",
    "document_aging",
    "document_proposals",
    "document_draft",
    "document_amend",
    "document_issue",
    "document_allocate",
    "document_credit",
    "document_revoke",
    "document_approve",
    "document_reject",
    "account_list",
    "account_show",
    "account_create",
    "account_rename",
    "account_classify",
    "bank_feed_show",
    "bank_feed_list",
    "bank_feed_review",
    "bank_feed_post",
    "bank_feed_link",
    "txn_post",
    "txn_propose",
    "txn_proposals",
    "txn_approve",
    "txn_reject",
    "txn_reverse",
    "txn_recategorize",
    "txn_link",
    "txn_settle_hold",
    "txn_meta_set",
    "txn_meta_unset",
    "txn_list",
    "txn_show",
    "report",
    "financial_summary",
    "fixed_asset_register",
    "fixed_asset_show",
    "fixed_asset_list",
    "fixed_asset_preview",
    "fixed_asset_depreciate",
    "fixed_asset_dispose",
    "expense_claim_receipt_intake",
    "expense_claim_receipt_show",
    "expense_claim_create",
    "expense_claim_amend",
    "expense_claim_submit",
    "expense_claim_approve",
    "expense_claim_reject",
    "expense_claim_record_payment",
    "expense_claim_void",
    "expense_claim_show",
    "expense_claim_list",
    "fx_post",
    "fx_revalue",
    "ledger_setup_status",
    "ledger_setup_book_show",
    "ledger_setup_begin",
    "ledger_setup_finish",
    "ledger_setup_revoke",
    "ledger_setup_book_create",
    "ledger_setup_book_initialize",
    "ledger_setup_book_update",
    "ledger_setup_chart_apply",
    "ledger_setup_account_add",
    "ledger_setup_opening_preview",
    "ledger_setup_opening_reserve",
    "ledger_setup_opening_post",
    "ledger_setup_statement_import",
    "ledger_setup_statement_preview",
    "ledger_setup_statement_commit",
    "ledger_setup_statement_receipt",
    "rule_list",
    "rule_match",
    "rule_create",
    "rule_amend",
    "rule_undo",
    "rule_delete",
    "bookkeeping_policy_show",
    "bookkeeping_policy_set",
    "inform_preference_show",
    "inform_preference_set",
    "budget_list",
    "budget_set",
    "budget_delete",
    "txn_clear",
    "txn_reconcile",
    "verify",
    "audit_log",
    "audit_receipts",
    "audit_undo",
    "restore",
    "restore_undo",
];

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
    let expected: BTreeSet<_> = LEDGER_TOOLS.iter().copied().collect();
    assert_eq!(tool_names(&listed_tools)?, expected);
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
fn amount_filters_require_and_match_one_currency_across_cli_and_mcp() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command
        .env("BEANKEEPER_CURRENCY", "USD")
        .args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let mut yen = posting("yen-sale");
    yen["currency"] = json!("JPY");
    yen["debits"][0]["amount"] = json!("5000");
    yen["credits"][0]["amount"] = json!("5000");
    let yen_id = client.success("txn_post", &yen)?["id"].clone();

    let cli_list = || -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&path)?;
        command.env("BEANKEEPER_CURRENCY", "USD").args([
            "--json",
            "--company",
            "acme",
            "txn",
            "list",
        ]);
        Ok(command)
    };
    let cli = cli_list()?.args(["--amount-gt", "100"]).output()?;
    assert!(cli.status.success());
    let inferred: Value = serde_json::from_slice(&cli.stdout)?;
    assert_eq!(inferred["data"][0]["id"], yen_id);
    assert_eq!(
        client.success("txn_list", &json!({"company":"acme", "amount_gt":"100"}))?,
        inferred["data"]
    );

    // Both transactions have 5000 minor units, in different currencies.
    let mut dollar = posting("dollar-sale");
    dollar["debits"][0]["amount"] = json!("50");
    dollar["credits"][0]["amount"] = json!("50");
    client.success("txn_post", &dollar)?;

    for count in [false, true] {
        let mut cli = cli_list()?;
        cli.args(["--amount-gt", "100"]);
        if count {
            cli.arg("--count");
        }
        let output = cli.output()?;
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, [] as [u8; 0]);
        let envelope: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(envelope["error"]["code"], "USAGE");

        let response = client.call(
            "txn_list",
            &json!({"company":"acme", "amount_gt":"100", "count":count}),
        )?;
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(payload(&response)?["code"], "USAGE");

        let mut cli = cli_list()?;
        cli.args([
            "--currency",
            "JPY",
            "--amount-gt",
            "100",
            "--amount-lt",
            "6000",
            "--amount-eq",
            "5000",
        ]);
        if count {
            cli.arg("--count");
        }
        let output = cli.output()?;
        assert!(output.status.success());
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            client.success(
                "txn_list",
                &json!({
                    "company":"acme", "currency":"JPY", "amount_gt":"100",
                    "amount_lt":"6000", "amount_eq":"5000", "count":count
                })
            )?,
            envelope["data"]
        );
        if count {
            assert_eq!(envelope["data"]["count"], 1);
        } else {
            assert_eq!(envelope["data"][0]["id"], yen_id);
            assert_eq!(envelope["meta"]["pagination"]["total"], 1);
        }
    }
    Ok(())
}

#[test]
fn expanded_iso_currencies_post_and_report_across_cli_and_mcp() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    bk(&path)?
        .args([
            "--company",
            "acme",
            "txn",
            "post",
            "--description",
            "Won sale",
            "--debit",
            "1000:45",
            "--credit",
            "4000:45",
            "--currency",
            "krw",
        ])
        .assert()
        .success();

    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let post_tool = listed["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "txn_post"))
        .ok_or("txn_post tool")?;
    let currency_schema = &post_tool["inputSchema"]["properties"]["currency"];
    assert!(currency_schema.is_object());
    assert!(!currency_schema.to_string().contains("\"enum\""));

    let mut dinar = posting("dinar-sale");
    dinar["currency"] = json!(" tnd ");
    dinar["debits"][0]["amount"] = json!("1.234");
    dinar["credits"][0]["amount"] = json!("1.234");
    assert_eq!(client.success("txn_post", &dinar)?["created"], true);

    for (currency, minor, display) in [("KRW", 45, "45"), ("TND", 1234, "1.234")] {
        let report = client.success(
            "report",
            &json!({"company":"acme", "kind":"trial-balance", "currency":currency}),
        )?;
        assert_eq!(report["total_debits"], minor);
        assert_eq!(report["total_debits_display"], display);
        let cli = bk(&path)?
            .args([
                "--json",
                "--company",
                "acme",
                "report",
                "trial-balance",
                "--currency",
                currency,
            ])
            .assert()
            .success();
        let envelope: Value = serde_json::from_slice(&cli.get_output().stdout)?;
        assert_eq!(envelope["data"], report);
    }
    assert_eq!(client.success("verify", &json!({}))?["status"], "healthy");
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
            let mut expected = json!({"limit":limit, "offset":offset, "returned":returned, "total":3, "has_more":has_more});
            if tool == "audit_log" && has_more {
                expected["next_offset"] = json!(offset + returned);
            }
            assert_eq!(response["result"]["_meta"]["pagination"], expected);
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
fn company_create_validates_and_normalizes_names_across_cli_and_mcp() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    bk(&path)?.arg("init").assert().success();
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    for name in BLANK_COMPANY_NAMES {
        assert_cli_rejects_company_name(bk(&path)?.args(["company", "create", "cli-book", name]))?;
        assert_parameter_rejection(
            &client.call("company_create", &json!({"slug": "mcp-book", "name": name}))?,
            COMPANY_NAME_ERROR,
        );
    }

    let cli = "cli-book";
    let mcp = "mcp-book";
    let padded_name = " \tCompany  Name\u{2003} ";
    bk(&path)?
        .args(["company", "create", cli, padded_name])
        .assert()
        .success();
    client.success("company_create", &json!({"slug": mcp, "name": padded_name}))?;
    for slug in [cli, mcp] {
        let row = client.success("company_show", &json!({"slug": slug}))?;
        assert_eq!(row[0]["name"], "Company  Name");
    }
    Ok(())
}

#[test]
fn company_tools_share_cli_results_checks_and_audit() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let company = json!({"slug":"new-company", "name":"New Company", "description":"Test book"});
    let created = client.success("company_create", &company)?;
    let shown = bk(&path)?
        .args(["--json", "company", "show", "new-company"])
        .output()?;
    assert!(shown.status.success());
    let cli: Value = serde_json::from_slice(&shown.stdout)?;
    assert_eq!(created, cli["data"]);

    let duplicate = client.call("company_create", &company)?;
    assert_eq!(payload(&duplicate)?["code"], "VALIDATION");
    assert_parameter_rejection(
        &client.call("company_create", &json!({"slug":"Bad Slug", "name":"Bad"}))?,
        "invalid company slug",
    );
    let audit = client.success(
        "audit_log",
        &json!({"company":"new-company", "entity":"company"}),
    )?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["action"], "create");
    assert_eq!(audit[0]["actor"], "mcp:codex-test-client");

    let empty = json!({"slug":"new-company"});
    assert_parameter_rejection(
        &client.call(
            "company_delete",
            &json!({"slug":"new-company", "force":true}),
        )?,
        "unknown field",
    );
    assert_eq!(client.success("company_show", &empty)?, created);

    client.success("txn_post", &posting("keep-this-posting"))?;
    let retained = payload(&client.call("company_delete", &json!({"slug":"acme"}))?)?;
    let cli_refusal = bk(&path)?
        .args(["--json", "company", "delete", "acme", "--force"])
        .output()?;
    let error: Value = serde_json::from_slice(&cli_refusal.stderr)?;
    assert_eq!(retained, error["error"]);
    assert_eq!(retained["code"], "VALIDATION");
    let message = retained["message"].as_str().ok_or("refusal message")?;
    assert!(message.contains("ledger is append-only"));
    assert!(message.contains("reverse them"));

    let deleted = client.success("company_delete", &empty)?;
    assert_eq!(deleted, json!({"deleted":"new-company"}));
    assert_eq!(
        payload(&client.call("company_show", &empty)?)?["code"],
        "NOT_FOUND"
    );
    assert_eq!(
        payload(&client.call("company_delete", &empty)?)?["code"],
        "NOT_FOUND"
    );
    let audit = client.success(
        "audit_log",
        &json!({"company":"new-company", "entity":"company", "action":"company_delete"}),
    )?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["actor"], "mcp:codex-test-client");
    assert_eq!(audit[0]["before_display"], created[0]);
    assert!(audit[0]["after"].is_null());

    client.success("company_create", &company)?;
    let cli_deleted = bk(&path)?
        .args(["--json", "company", "delete", "new-company", "--force"])
        .output()?;
    assert!(cli_deleted.status.success());
    let cli: Value = serde_json::from_slice(&cli_deleted.stdout)?;
    assert_eq!(deleted, cli["data"]);
    client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn company_mutations_use_the_resolved_actor_and_support_actor_filters() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command
        .env("BK_ACTOR", "env-label")
        .args(["--actor", "  company-operator  ", "mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    client.success(
        "company_create",
        &json!({"slug":"new-company", "name":"New Company"}),
    )?;
    client.success("company_delete", &json!({"slug":"new-company"}))?;

    let audit = client.success("audit_log", &json!({"actor":"company-operator"}))?;
    let rows = audit.as_array().ok_or("audit data is not an array")?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["action"], "company_delete");
    assert_eq!(rows[1]["action"], "create");
    for row in rows {
        assert_eq!(row["actor"], "company-operator");
        assert_eq!(row["entity"], "company");
        assert_eq!(row["entity_id"], "new-company");
    }
    let cli = bk(&path)?
        .args(["--json", "audit", "log", "--by-actor", "company-operator"])
        .assert()
        .success();
    let cli: Value = serde_json::from_slice(&cli.get_output().stdout)?;
    assert_eq!(cli["data"], audit);
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
    assert_eq!(rows.len(), 11, "{audit}");
    let mut previous = "0".repeat(64);
    for row in rows.iter().rev() {
        if row["id"].as_i64().is_some_and(|id| id > 4) {
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
    let evidence = client.success("verify", &json!({}))?;
    assert_eq!(evidence["status"], "healthy");
    assert_eq!(evidence["audit_chain"]["head_hash"], audit[0]["row_hash"]);
    assert_eq!(evidence["audit_chain"]["intact"], true);
    drop(client);
    let cli = bk(&path)?.args(["--json", "verify"]).assert().success();
    let envelope: Value = serde_json::from_slice(&cli.get_output().stdout)?;
    assert_eq!(evidence, envelope["data"]);
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
    assert_eq!(names.len(), 47);
    assert!(names.contains("tax_list"));
    assert!(names.contains("tax_report"));
    assert!(!names.contains("tax_configure"));
    assert!(names.contains("audit_receipts"));
    assert!(!names.contains("audit_undo"));
    assert!(names.contains("ledger_setup_status"));
    assert!(names.contains("ledger_setup_statement_receipt"));
    assert!(names.contains("ledger_setup_book_show"));
    assert!(names.contains("financial_summary"));
    assert!(names.contains("period_list"));
    assert!(names.contains("reconcile_list"));
    assert!(!names.contains("reconcile_statement"));
    assert!(!names.contains("period_close"));
    assert!(!names.contains("period_reopen"));
    for name in [
        "bank_feed_show",
        "bank_feed_list",
        "bank_feed_review",
        "rule_list",
        "rule_match",
        "bookkeeping_policy_show",
        "inform_preference_show",
    ] {
        assert!(names.contains(name));
    }
    for name in [
        "bank_feed_post",
        "bank_feed_link",
        "company_create",
        "company_delete",
        "security_create",
        "security_price",
        "investment_buy",
        "investment_sell",
        "account_create",
        "txn_post",
        "txn_propose",
        "txn_approve",
        "txn_reject",
        "txn_reverse",
        "txn_meta_set",
        "txn_meta_unset",
        "txn_clear",
        "txn_reconcile",
        "budget_set",
        "budget_delete",
        "ledger_setup_begin",
        "ledger_setup_finish",
        "ledger_setup_revoke",
        "ledger_setup_book_create",
        "ledger_setup_book_initialize",
        "ledger_setup_book_update",
        "ledger_setup_chart_apply",
        "ledger_setup_account_add",
        "ledger_setup_opening_preview",
        "ledger_setup_opening_reserve",
        "ledger_setup_statement_preview",
        "ledger_setup_statement_commit",
        "rule_create",
        "rule_amend",
        "rule_undo",
        "rule_delete",
        "bookkeeping_policy_set",
        "inform_preference_set",
    ] {
        assert!(!names.contains(name));
    }
    assert!(!names.contains("txn_reconcile"));
    let response = client.call("txn_post", &posting("read-only"))?;
    assert!(response.get("error").is_some() || response["result"]["isError"] == true);
    for (name, input) in [
        ("company_create", json!({"slug":"empty", "name":"Empty"})),
        ("company_delete", json!({"slug":"acme"})),
    ] {
        let response = client.call(name, &input)?;
        assert!(response.get("error").is_some() || response["result"]["isError"] == true);
    }
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
    assert_eq!(transactions, json!([]));
    let history = client.success(
        "txn_list",
        &json!({"company":"acme", "include_reversed":true}),
    )?;
    assert_eq!(history.as_array().map(Vec::len), Some(2));
    assert_eq!(history[0]["id"], posted["id"]);
    assert_eq!(history[1]["id"], created["id"]);
    for (include_reversed, count) in [(false, 0), (true, 2)] {
        assert_eq!(
            client.success(
                "txn_list",
                &json!({"company":"acme", "count":true, "include_reversed":include_reversed}),
            )?,
            json!({"count":count})
        );
    }
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
fn restore_tool_points_new_file_recovery_to_the_local_command() -> TestResult {
    let dir = tempdir()?;
    let ledger = dir.path().join("live.db");
    let backup = dir.path().join("live.bk-backup");
    let destination = dir.path().join("new.db");
    bk(&ledger)?.arg("init").assert().success();
    bk(&ledger)?
        .args(["backup", "--output"])
        .arg(&backup)
        .assert()
        .success();

    let mut command = bk(&ledger)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let tool = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "restore")
        .ok_or("missing restore tool")?;
    let description = tool["description"].as_str().ok_or("missing description")?;
    assert!(description.contains("bk --db <path> recover --input <backup>"));

    let response = client.call(
        "restore",
        &json!({"input": backup, "destination": destination}),
    )?;
    assert_parameter_rejection(&response, "unknown field `destination`");
    assert!(!destination.try_exists()?);
    assert_eq!(
        client.success("audit_log", &json!({"action":"ledger_restored"}))?,
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
    Ok(Granted::<Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name: "mcp_agent",
                    capabilities: &["ledger/read".into()],
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
            )
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
        .env("CODEX_THREAD_ID", "test-session")
        .args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    // A read-only grant still allows writes: enforcement is outside Phase 1.
    let posted = client.success("txn_post", &posting("attested-mcp"))?;
    let shown = client.success("txn_show", &json!({"company":"acme", "id":posted["id"]}))?;
    assert_eq!(shown[0]["agent_uri"], agent);
    assert_eq!(shown[0]["token_jti"], issued["token_jti"]);
    assert_eq!(shown[0]["posted_by"], agent);
    let audit = client.success("audit_log", &json!({"actor":agent, "agent":agent}))?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    assert_eq!(audit[0]["agent_uri"], agent);
    assert_eq!(audit[0]["token_jti"], issued["token_jti"]);
    assert!(audit[0]["client_hint"].is_null());
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
        .read()
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
fn metadata_set_describes_its_schema_and_recovers_from_bad_mcp_inputs() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let listed = client.request("tools/list", &json!({}))?;
    let tool = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "txn_meta_set")
        .ok_or("missing txn_meta_set")?;
    let example = r#"{"status":"pending"}"#;
    let reserved = [
        "correlate",
        "proposal_id",
        "reverses",
        "reversed_by",
        "applies_to",
        "beankeeper_source",
    ];
    let schema = &tool["inputSchema"];
    let values = &schema["properties"]["values"];
    for description in [&tool["description"], &values["description"]] {
        let text = description.as_str().ok_or("missing description")?;
        assert!(text.contains(example), "{text}");
        for key in reserved {
            assert!(text.contains(key), "missing {key}: {text}");
        }
    }
    assert_eq!(schema["type"], "object");
    assert_eq!(values["type"], "object");
    assert_eq!(values["examples"], json!([{"status":"pending"}]));
    assert!(!schema.to_string().contains(r#""$ref""#), "{schema}");

    let selector = json!({"company":"acme", "reference":"hold-1"});
    let missing = payload(&client.call(
        "txn_meta_set",
        &json!({"company":"acme", "reference":"hold-1", "values":{"status":"pending"}}),
    )?)?;
    assert_eq!(missing["code"], "NOT_FOUND");
    assert_eq!(
        missing["message"],
        "transaction with reference 'hold-1' not found in company 'acme'"
    );
    client.success("txn_post", &posting("hold-1"))?;

    for (field, input) in [
        ("values", json!(["pending"])),
        ("tags", json!(["pending"])),
        ("values", json!(true)),
        ("values", json!({"status":true})),
        ("values", json!({"correlate":"1"})),
    ] {
        let mut rejected = selector.clone();
        rejected[field] = input;
        let response = client.call("txn_meta_set", &rejected)?;
        assert_eq!(response["result"]["isError"], true, "{response}");
        let error = payload(&response)?;
        assert_eq!(error["code"], "VALIDATION");
        let message = error["message"]
            .as_str()
            .ok_or("missing validation message")?;
        assert!(message.contains(example), "{message}");
        assert!(
            !message.contains("--meta") && !message.contains("untagged enum"),
            "{message}"
        );

        let mut corrected = selector.clone();
        corrected["values"] = json!({"status":"pending"});
        let success = client.success("txn_meta_set", &corrected)?;
        let stored: Value =
            serde_json::from_str(success["metadata"].as_str().ok_or("missing metadata")?)?;
        assert_eq!(stored["status"], "pending");
    }

    for values in [
        json!({"pending":"true"}),
        json!(["status=pending"]),
        json!(r#"{"status":"pending"}"#),
        json!(r#"["status=pending"]"#),
    ] {
        let mut input = selector.clone();
        input["values"] = values;
        client.success("txn_meta_set", &input)?;
    }
    Ok(())
}

#[test]
fn metadata_validation_wording_matches_cli_and_mcp() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    for (operation, argument, field) in [
        ("set", "pending", "values"),
        ("set", "proposal_id=123", "values"),
        ("set", "=pending", "values"),
        ("set", r#"quoted"key=pending"#, "values"),
        ("unset", "beankeeper_source", "keys"),
    ] {
        let cli = bk(&path)?
            .args([
                "--json",
                "--company",
                "acme",
                "txn",
                "meta",
                operation,
                "1",
                argument,
            ])
            .assert()
            .failure();
        let cli_error: Value = serde_json::from_slice(&cli.get_output().stderr)?;
        let mut input = json!({"company":"acme", "id":1});
        input[field] = json!([argument]);
        let mcp_error = payload(&client.call(&format!("txn_meta_{operation}"), &input)?)?;
        assert_eq!(cli_error["error"]["code"], "VALIDATION");
        assert_eq!(mcp_error["code"], "VALIDATION");
        let cli_message = cli_error["error"]["message"]
            .as_str()
            .ok_or("missing CLI error")?;
        let mcp_message = mcp_error["message"].as_str().ok_or("missing MCP error")?;
        assert!(
            mcp_message.starts_with(cli_message),
            "{mcp_message} vs {cli_message}"
        );
    }
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
    let retry = client.success("txn_meta_set", &set)?;
    assert_eq!(retry["changed"], false);
    assert_eq!(retry["receipt"], Value::Null);
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
fn invalid_currency_fails_at_both_parse_boundaries_with_validation_code() -> TestResult {
    use beankeeper::types::CurrencyError;
    use beankeeper_cli::cli::Cli;
    use clap::Parser as _;

    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let args = [
        "--company",
        "acme",
        "txn",
        "post",
        "--description",
        "Invalid",
        "--debit",
        "1000:1",
        "--credit",
        "4000:1",
        "--currency",
        "XXX",
        "--format=json",
    ];
    let Err(parse_error) = Cli::try_parse_from(std::iter::once("bk").chain(args)) else {
        return Err("invalid currency must fail in clap".into());
    };
    assert!(matches!(
        parse_error.source().and_then(|cause| cause.downcast_ref::<CurrencyError>()),
        Some(CurrencyError::UnsupportedCode { code }) if code == "XXX"
    ));
    let cli = bk(&path)?.args(args).assert().code(3);
    let cli: Value = serde_json::from_slice(&cli.get_output().stderr)?;
    assert_eq!(cli["error"]["code"], "VALIDATION");
    assert_eq!(cli["meta"]["command"], "txn.post");
    assert_eq!(cli["meta"]["company"], "acme");
    let detail = cli["error"]["message"]
        .as_str()
        .ok_or("CLI error message")?;
    assert_eq!(
        detail,
        "unsupported currency code: XXX (fund or no defined minor unit)"
    );
    assert!(!path.exists());

    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let mut input = posting("invalid-currency");
    input["currency"] = json!("XXX");
    let mcp = client.call("txn_post", &input)?;
    assert_parameter_rejection(&mcp, detail);
    assert_eq!(payload(&mcp)?["code"], cli["error"]["code"]);
    Ok(())
}

#[test]
fn cli_and_mcp_reject_invalid_calendar_dates_at_the_parameter_boundary() -> TestResult {
    use beankeeper_cli::{cli::Cli, request::CalendarDateError};
    use clap::Parser as _;

    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let invalid = ["2026-02-29", "2024-02-29T00:00:00Z"];
    for date in invalid {
        let args = [
            "--company",
            "acme",
            "report",
            "trial-balance",
            "--from",
            date,
            "--period",
            "month",
            "--format=json",
        ];
        let Err(parse_error) = Cli::try_parse_from(std::iter::once("bk").chain(args)) else {
            return Err("invalid date must fail in clap".into());
        };
        assert!(
            parse_error
                .source()
                .is_some_and(<dyn Error>::is::<CalendarDateError>)
        );
        let cli = bk(&path)?.args(args).assert().code(3);
        let output: Value = serde_json::from_slice(&cli.get_output().stderr)?;
        assert_eq!(output["error"]["code"], "VALIDATION");
        assert_eq!(output["meta"]["command"], "report.trial-balance.periods");
        assert_eq!(output["meta"]["company"], "acme");
        assert!(!path.exists());
    }

    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for date in invalid.into_iter().chain(["2024-2-29"]) {
        let response = client.call(
            "report",
            &json!({"company":"acme", "kind":"trial-balance", "from":date, "period":"month"}),
        )?;
        assert_parameter_rejection(&response, "expected YYYY-MM-DD");
        assert_eq!(payload(&response)?["code"], "VALIDATION");
    }

    let compare = "2026-02-29..2026-03-31";
    let cli = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "report",
            "trial-balance",
            "--compare",
            compare,
        ])
        .assert()
        .code(3);
    let output: Value = serde_json::from_slice(&cli.get_output().stderr)?;
    assert_eq!(output["error"]["code"], "VALIDATION");
    assert_eq!(output["meta"]["command"], "report.trial-balance.compare");
    let response = client.call(
        "report",
        &json!({"company":"acme", "kind":"trial-balance", "compare":compare}),
    )?;
    assert_parameter_rejection(&response, "expected YYYY-MM-DD");
    assert_eq!(payload(&response)?["code"], "VALIDATION");
    Ok(())
}

#[test]
fn malformed_ledger_inputs_fail_at_the_mcp_parameter_boundary() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let mut empty_debits = posting("empty-debits");
    empty_debits["debits"] = json!([]);
    let mut bad_currency = posting("bad-currency");
    bad_currency["currency"] = json!("XXX");
    for (input, detail) in [
        (empty_debits, "non-empty array"),
        (bad_currency, "unsupported currency code: XXX"),
    ] {
        let response = client.call("txn_post", &input)?;
        assert_parameter_rejection(&response, detail);
    }

    let response = client.call(
        "txn_clear",
        &json!({"company":"acme","target":"entry","transaction_id":1,"reference":"other"}),
    )?;
    assert_parameter_rejection(&response, "reference");

    let db = beankeeper_cli::db::Db::open_existing(&path, None)?;
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
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

#[test]
fn investment_tools_post_exact_trades_and_match_cli_valuations() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    bk(&path)?
        .args([
            "--company",
            "acme",
            "account",
            "create",
            "1200",
            "Investments",
            "--type",
            "asset",
        ])
        .assert()
        .success();
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    client.success(
        "security_create",
        &json!({"company":"acme", "symbol":"TEST", "name":"Test shares", "currency":"BHD"}),
    )?;
    let securities = client.success("security_list", &json!({"company":"acme"}))?;
    assert_eq!(securities[0]["symbol"], "TEST");
    let bought = client.success("investment_buy", &json!({"company":"acme", "symbol":"TEST", "quantity":"1.5", "cost":"15.123", "date":"2026-01-01", "asset":"1200", "cash":"1000", "reference":"investment-buy"}))?;
    assert_eq!(bought["cost_basis"], 15123);
    assert_eq!(bought["cost_basis_display"], "15.123");
    let lot = bought["lot_id"].as_str().ok_or("missing lot ID")?;
    let observed = client.success(
        "security_price",
        &json!({"company":"acme", "symbol":"TEST", "date":"2026-01-02", "price":"12.345"}),
    )?;
    assert_eq!(observed["price_display"], "12.345");
    let sold = client.success("investment_sell", &json!({"company":"acme", "symbol":"TEST", "quantity":"0.5", "proceeds":"7.000", "date":"2026-01-03", "asset":"1200", "cash":"1000", "gain":"4000", "method":"specific", "lot":lot, "reference":"investment-sell"}))?;
    assert_eq!(sold["cost_basis"], 5041);
    assert_eq!(sold["realized_gain"], 1959);
    assert_eq!(sold["realized_gain_display"], "1.959");
    let lots = client.success(
        "investment_lots",
        &json!({"company":"acme", "symbol":"TEST"}),
    )?;
    assert_eq!(lots[0]["remaining_quantity"], "1");
    assert_eq!(lots[0]["remaining_cost_basis_display"], "10.082");
    let holdings = client.success(
        "investment_holdings",
        &json!({"company":"acme", "as_of":"2026-01-03"}),
    )?;
    assert_eq!(holdings[0]["market_value_display"], "12.345");
    assert_eq!(holdings[0]["unrealized_gain_display"], "2.263");
    let output = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "investment",
            "holdings",
            "--as-of",
            "2026-01-03",
        ])
        .output()?;
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(cli["data"], holdings);
    let transaction = client.success(
        "txn_show",
        &json!({"company":"acme", "id":sold["transaction_id"]}),
    )?;
    assert_eq!(transaction[0]["posted_by"], "mcp:codex-test-client");
    client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn investment_cli_outputs_currency_decimals_and_honors_quiet() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed_investment_cli(&path)?;
    let csv = bk(&path)?
        .args([
            "--company",
            "acme",
            "--format",
            "csv",
            "investment",
            "holdings",
            "--as-of",
            "2026-01-02",
        ])
        .output()?;
    assert!(csv.status.success());
    let csv = String::from_utf8(csv.stdout)?;
    assert!(csv.contains("symbol,asset,currency,quantity,cost_basis"));
    assert!(csv.contains("TEST,1200,BHD,1.5,15.123"));
    assert!(csv.contains("12.345,18.518,3.395"));
    let table = bk(&path)?
        .args([
            "--company",
            "acme",
            "investment",
            "holdings",
            "--as-of",
            "2026-01-02",
        ])
        .output()?;
    assert!(table.status.success());
    let table = String::from_utf8(table.stdout)?;
    assert!(table.contains("15.123"));
    assert!(table.contains("18.518"));
    let quiet = bk(&path)?
        .args(["--company", "acme", "--quiet", "security", "list"])
        .output()?;
    assert!(quiet.status.success());
    assert_eq!(quiet.stdout, [] as [u8; 0]);
    assert_eq!(quiet.stderr, [] as [u8; 0]);
    let output = bk(&path)?
        .args([
            "--quiet",
            "--json",
            "investment",
            "holdings",
            "--company",
            "acme",
            "--as-of",
            "2026-01-02",
        ])
        .output()?;
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["meta"]["company"], "acme");
    assert_eq!(json["meta"]["command"], "investment.holdings");
    assert_eq!(json["data"][0]["market_value_display"], "18.518");
    let output = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "txn",
            "list",
            "--reference",
            "trimmed-buy",
        ])
        .output()?;
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["data"].as_array().map(Vec::len), Some(1));
    bk(&path)?
        .args([
            "--company",
            "acme",
            "investment",
            "buy",
            "TEST",
            "--quantity",
            "1",
            "--cost",
            "10",
            "--date",
            "2026-01-03",
            "--asset",
            "1200",
            "--cash",
            "1000",
            "--reference",
            "  ",
        ])
        .assert()
        .failure();
    Ok(())
}

fn seed_investment_cli(path: &Path) -> TestResult {
    seed(path)?;
    for args in [
        vec![
            "account",
            "create",
            "1200",
            "Investments",
            "--type",
            "asset",
        ],
        vec![
            "security",
            "create",
            "TEST",
            "--name",
            "Test shares",
            "--currency",
            "BHD",
        ],
        vec![
            "investment",
            "buy",
            "TEST",
            "--quantity",
            "1.5",
            "--cost",
            "15.123",
            "--date",
            "2026-01-01",
            "--asset",
            "1200",
            "--cash",
            "1000",
            "--reference",
            "  trimmed-buy  ",
        ],
        vec![
            "security",
            "price",
            "TEST",
            "--date",
            "2026-01-02",
            "--price",
            "12.345",
        ],
    ] {
        bk(path)?
            .args(["--company", "acme"])
            .args(args)
            .assert()
            .success();
    }
    Ok(())
}

#[test]
fn tax_reports_over_stdio_match_cli_and_advertise_threshold() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    bk(&path)?
        .args([
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "Tax example",
            "--date",
            "2026-01-01",
            "--debit",
            "1000:700",
            "--credit",
            "4000:700",
            "--tax",
            "4000=sched-c:1",
            "--meta",
            "payee=Jordan Key",
            "--meta",
            "tin=001-02-0003",
        ])
        .assert()
        .success();
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let schema = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .find(|tool| tool["name"] == "report")
        .ok_or("report tool")?["inputSchema"]
        .to_string();
    for name in ["schedule-c", "1099", "threshold"] {
        assert!(schema.contains(name), "{schema}");
    }
    for kind in ["schedule-c", "1099"] {
        let mut input = json!({"company":"acme", "kind":kind, "to":"2026-12-31"});
        let mut args = vec![
            "--json",
            "--company",
            "acme",
            "report",
            kind,
            "--to",
            "2026-12-31",
        ];
        if kind == "1099" {
            input["threshold"] = "700.00".into();
            args.extend(["--threshold", "700.00"]);
        }
        let actual = client.success("report", &input)?;
        let output = bk(&path)?.args(args).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(actual, cli["data"]);
        if kind == "1099" {
            assert_eq!(actual["vendors"][0]["total"], 70_000);
            assert_eq!(actual["vendors"][0]["tin"], "001-02-0003");
        } else {
            assert_eq!(actual["lines"][0]["net"], 70_000);
        }
    }
    let rejected = client.call(
        "report",
        &json!({"company":"acme", "kind":"1099", "threshold":"-1"}),
    )?;
    assert_eq!(rejected["result"]["isError"], true);
    Ok(())
}

fn identity_client(
    path: &Path,
    passphrase_file: &Path,
    token: Option<&str>,
    read_only: bool,
) -> Result<Client, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.arg("--passphrase-file").arg(passphrase_file);
    if let Some(token) = token {
        command.args(["--agent-token", token]);
    }
    command.args(["mcp", "serve"]);
    if read_only {
        command.arg("--read-only");
    }
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

fn proposal_identity(
    db: &beankeeper_cli::db::Db,
    name: &str,
    capabilities: &[&str],
) -> Result<Value, Box<dyn Error>> {
    use beankeeper_cli::identity::{self, IssueRequest};
    Ok(Granted::<Admin>::with_db(
        db,
        &beankeeper_cli::db::Actor::new("owner".parse()?),
        |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name,
                    capabilities: &capabilities
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    ttl: Duration::from_hours(1),
                    public_key: None,
                    generate_key: true,
                },
            )
        },
    )?)
}

#[test]
fn rule_creation_undo_is_owner_only_and_identical_via_cli_and_mcp() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};

    let dir = tempdir()?;
    let path = dir.path().join("rules.db");
    let passphrase_file = dir.path().join("passphrase");
    let read_only = identity_ledger(&path, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let ledger = Db::open_existing(&path, Some(&passphrase))?;
    Granted::<Admin>::with_db(&ledger, &Actor::new("fixture".parse()?), |grant| {
        db::create_company(grant, &"other".parse()?, &"Other".parse()?, None)
    })?;
    Granted::<Admin>::with_db(&ledger, &Actor::new("fixture".parse()?), |grant| {
        db::create_account(
            grant,
            &CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "9000",
                name: "Suspense",
                account_type: beankeeper::types::AccountType::Expense,
                default_tax_category: None,
            },
        )
    })?;
    let owner = proposal_identity(&ledger, "owner", &["ledger/admin", "ledger/read"])?;
    let bookkeeper = proposal_identity(&ledger, "bookkeeper", &["ledger/post", "ledger/read"])?;
    let token = owner["token"].as_str().ok_or("missing owner token")?;
    let mut agent = identity_client(&path, &passphrase_file, bookkeeper["token"].as_str(), false)?;
    let mut owner_client = identity_client(&path, &passphrase_file, Some(token), false)?;
    let mut reader = identity_client(&path, &passphrase_file, read_only["token"].as_str(), false)?;
    let cli = |token: Option<&str>| -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&path)?;
        command.arg("--passphrase-file").arg(&passphrase_file);
        command.args(["--json", "--company", "acme"]);
        if let Some(token) = token {
            command.args(["--agent-token", token]);
        }
        Ok(command)
    };

    for (pattern, mcp_first) in [("coffee", true), ("tea", false)] {
        let rules = agent.success(
            "rule_create",
            &json!({"company":"acme", "contains":pattern, "account":"4000"}),
        )?;
        let id = rules[0]["id"].as_str().ok_or("missing rule ID")?;
        let args = json!({"company":"acme", "id":id});
        let candidate = json!({"company":"acme", "description":pattern});
        assert_eq!(reader.success("rule_match", &candidate)?["rule"]["id"], id);
        assert_eq!(agent.call("rule_undo", &args)?["error"]["code"], -32602);
        cli(None)?.args(["rule", "undo", id]).assert().code(6);
        let wrong_company = json!({"company":"other", "id":id});
        assert_eq!(
            payload(&owner_client.call("rule_undo", &wrong_company)?)?["code"],
            "NOT_FOUND"
        );
        let run_cli = || -> Result<Value, Box<dyn Error>> {
            let output = cli(Some(token))?.args(["rule", "undo", id]).output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].clone())
        };
        let (first, retry) = if mcp_first {
            (owner_client.success("rule_undo", &args)?, run_cli()?)
        } else {
            (run_cli()?, owner_client.success("rule_undo", &args)?)
        };
        assert_eq!(first, retry);
        assert_eq!(first["rule"], id);
        assert_eq!(
            reader.success("rule_match", &candidate)?,
            json!({"outcome":"no_match"})
        );
        assert_eq!(
            reader.success("rule_list", &json!({"company":"acme"}))?,
            json!([])
        );

        assert_rule_undo_audit(
            &mut reader,
            id,
            &first["undo_receipt"],
            &owner["agent_uri"],
            &bookkeeper["agent_uri"],
        )?;
    }

    assert_rule_undo_import(
        &mut cli(bookkeeper["token"].as_str())?,
        &dir.path().join("bank.csv"),
        &ledger,
    )?;
    reader.success("verify", &json!({}))?;
    Ok(())
}

fn assert_rule_undo_audit(
    reader: &mut Client,
    id: &str,
    receipt: &Value,
    owner_uri: &Value,
    bookkeeper_uri: &Value,
) -> TestResult {
    let events = reader.success("audit_log", &json!({"company":"acme", "entity":"rule"}))?;
    let events: Vec<_> = events
        .as_array()
        .ok_or("missing audit events")?
        .iter()
        .filter(|event| event["entity_id"] == id)
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(&events[0]["id"], receipt);
    assert_eq!(events[0]["undo_of"], events[1]["id"]);
    assert_eq!(events[0]["action"], "rule_deactivate");
    assert_eq!(&events[0]["agent_uri"], owner_uri);
    assert_eq!(
        serde_json::from_str::<Value>(events[0]["after"].as_str().ok_or("missing after")?)?,
        "deactivated"
    );
    assert_eq!(events[1]["action"], "create");
    assert_eq!(&events[1]["agent_uri"], bookkeeper_uri);

    Ok(())
}

fn assert_rule_undo_import(
    command: &mut Command,
    bank: &Path,
    ledger: &beankeeper_cli::db::Db,
) -> TestResult {
    std::fs::write(
        bank,
        "date,description,amount\n2026-03-15,Coffee,-1\n2026-03-16,Tea,-2\n",
    )?;
    let output = command
        .args(["txn", "import", "--file"])
        .arg(bank)
        .args(["--account", "1000", "--suspense", "9000"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fallback_count: i64 = ledger.read().query_row(
        "SELECT COUNT(*) FROM entries WHERE company_slug = 'acme' AND account_code = '9000'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(fallback_count, 2);
    Ok(())
}

#[test]
fn posting_tool_follows_signed_write_mode_and_owner_undoes_direct_posts() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    let dir = tempdir()?;
    let path = dir.path().join("write-mode.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let bookkeeper_grants = ["ledger/read", "ledger/post", "ledger/propose"];
    let direct = proposal_identity(&db, "bookkeeper", &bookkeeper_grants)?;
    let proposal = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args([
            "--json",
            "agent",
            "issue",
            "--name",
            "proposer",
            "--generate-key",
            "--caps",
            &bookkeeper_grants.join(","),
            "--write-mode",
            "propose-only",
        ])
        .output()?;
    assert!(
        proposal.status.success(),
        "{}",
        String::from_utf8_lossy(&proposal.stderr)
    );
    let issued: Value = serde_json::from_slice(&proposal.stdout)?;
    let proposer = &issued["data"];
    let owner = proposal_identity(
        &db,
        "owner",
        &["ledger/read", "ledger/approve", "ledger/reverse"],
    )?;
    for enforce in [false, true] {
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::audit::set_policy(grant, enforce)
        })?;
        let mut bookkeeper =
            identity_client(&path, &passphrase_file, direct["token"].as_str(), false)?;
        let mut reviewer =
            identity_client(&path, &passphrase_file, owner["token"].as_str(), false)?;
        let mut agent =
            identity_client(&path, &passphrase_file, proposer["token"].as_str(), false)?;
        let tools = agent.request("tools/list", &json!({}))?;
        let names = tool_names(&tools)?;
        assert!(names.contains("txn_post"));
        assert!(!names.contains("txn_approve"));
        assert!(names.contains("txn_reverse"));
        assert!(names.contains("audit_undo"));

        let posted = bookkeeper.success("txn_post", &posting(&format!("direct-{enforce}")))?;
        assert_eq!(posted["created"], true);
        assert_eq!(
            reviewer.success("txn_proposals", &json!({"company":"acme"}))?,
            json!([])
        );
        let arguments = json!({"company":"acme", "id":posted["id"], "date":"2026-03-02",
            "reference":format!("undo-{enforce}")});
        let undone = reviewer.success("txn_reverse", &arguments)?;
        assert_eq!(undone["created"], true);
        let retry = reviewer.success("txn_reverse", &arguments)?;
        assert_eq!(retry["existing_id"], undone["id"]);
        for (credential, action) in [(&direct, "create"), (&owner, "reverse")] {
            let rows = reviewer.success(
                "audit_log",
                &json!({"company":"acme", "entity":"transaction",
                "action":action, "actor":credential["agent_uri"]}),
            )?;
            let row = &rows[0];
            assert_eq!(row["entity_id"], posted["id"].to_string());
            assert_eq!(row["actor"], credential["agent_uri"]);
            assert_eq!(row["agent_uri"], credential["agent_uri"]);
            assert_eq!(row["token_jti"], credential["token_jti"]);
        }

        let waiting = agent.success("txn_post", &posting(&format!("proposal-{enforce}")))?;
        assert_eq!(waiting["status"], "pending");
        assert_eq!(waiting["proposer_uri"], proposer["agent_uri"]);
        assert_eq!(waiting["proposer_jti"], proposer["token_jti"]);
        assert!(waiting["transaction_id"].is_null());
        let approve = json!({"company":"acme", "id": waiting["id"]});
        let denied = agent.call("txn_approve", &approve)?;
        assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
        let approved = reviewer.success("txn_approve", &approve)?;
        assert_eq!(approved["status"], "approved");
        assert_eq!(approved["resolved_by_uri"], owner["agent_uri"]);
        reviewer.success("verify", &json!({}))?;
    }
    Ok(())
}

#[test]
fn proposal_tools_filter_grants_and_post_approved_requests_with_reviewer_identity() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    let dir = tempdir()?;
    let path = dir.path().join("proposals.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let requester = proposal_identity(&db, "requester", &["ledger/read", "ledger/propose"])?;
    let reviewer = proposal_identity(&db, "reviewer", &["ledger/read", "ledger/approve"])?;
    beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        |grant| db::audit::set_policy(grant, true),
    )?;
    let mut agent = identity_client(&path, &passphrase_file, requester["token"].as_str(), false)?;
    let listed = agent.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert!(names.contains("txn_propose"));
    assert!(names.contains("txn_reject"));
    assert!(names.contains("txn_proposals"));
    assert!(!names.contains("txn_approve"));
    assert!(!names.contains("txn_post"));
    let proposed = agent.success("txn_propose", &posting("proposal-approved"))?;
    assert_eq!(proposed["status"], "pending");
    assert_eq!(
        agent.success("txn_list", &json!({"company":"acme"}))?,
        json!([])
    );
    let mut review_client =
        identity_client(&path, &passphrase_file, reviewer["token"].as_str(), false)?;
    let pending = review_client.success("txn_proposals", &json!({"company":"acme"}))?;
    assert_eq!(pending[0]["id"], proposed["id"]);
    let approved = review_client.success(
        "txn_approve",
        &json!({"company":"acme", "id":proposed["id"]}),
    )?;
    assert_eq!(approved["status"], "approved");
    let transactions = review_client.success(
        "txn_list",
        &json!({"company":"acme", "reference":"proposal-approved"}),
    )?;
    assert_eq!(transactions[0]["agent_uri"], reviewer["agent_uri"]);
    let metadata: Value = serde_json::from_str(
        transactions[0]["metadata"]
            .as_str()
            .ok_or("transaction metadata")?,
    )?;
    assert_eq!(metadata["proposal_id"], proposed["id"]);
    let repeated = review_client.call(
        "txn_approve",
        &json!({"company":"acme", "id":proposed["id"]}),
    )?;
    assert_eq!(payload(&repeated)?["code"], "VALIDATION");
    let withdrawn = agent.success("txn_propose", &posting("proposal-withdrawn"))?;
    let rejected = agent.success(
        "txn_reject",
        &json!({"company":"acme", "id":withdrawn["id"], "reason":"superseded"}),
    )?;
    assert_eq!(rejected["status"], "withdrawn");
    let rejected = agent.success(
        "txn_proposals",
        &json!({"company":"acme", "include_rejected":true}),
    )?;
    assert!(
        rejected
            .as_array()
            .ok_or("proposals")?
            .iter()
            .any(|row| row["id"] == withdrawn["id"])
    );
    review_client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn mcp_posting_checks_spend_limits_while_proposals_use_the_approval_gate() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    let dir = tempdir()?;
    let path = dir.path().join("limits.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let requester = proposal_identity(
        &db,
        "limited",
        &["ledger/read", "ledger/post", "ledger/propose"],
    )?;
    bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args([
            "agent",
            "limit",
            "set",
            requester["agent_uri"].as_str().ok_or("agent URI")?,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "10.00",
        ])
        .assert()
        .success();
    beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        |grant| db::audit::set_policy(grant, true),
    )?;
    let mut writer = identity_client(&path, &passphrase_file, requester["token"].as_str(), false)?;
    let rejected = writer.call("txn_post", &posting("over-limit"))?;
    assert_eq!(rejected["result"]["isError"], true);
    assert_eq!(payload(&rejected)?["code"], "FORBIDDEN");
    let proposed = writer.success("txn_propose", &posting("over-limit"))?;
    assert_eq!(proposed["status"], "pending");
    assert_eq!(
        writer.success("txn_list", &json!({"company":"acme"}))?,
        json!([])
    );
    let mut reader = identity_client(&path, &passphrase_file, requester["token"].as_str(), true)?;
    let listed = reader.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert!(names.contains("txn_proposals"));
    for name in ["txn_propose", "txn_approve", "txn_reject", "txn_post"] {
        assert!(!names.contains(name));
        let denied = reader.call(name, &posting("read-only"))?;
        assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    }
    let visible = reader.success("txn_proposals", &json!({"company":"acme"}))?;
    assert_eq!(visible[0]["id"], proposed["id"]);
    writer.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn enforced_read_session_filters_tools_and_rechecks_revocation() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    use beankeeper_cli::identity;
    let dir = tempdir()?;
    let path = dir.path().join("identity.db");
    let passphrase_file = dir.path().join("passphrase");
    let issued = identity_ledger(&path, &passphrase_file)?;
    let token = issued["token"].as_str().ok_or("token")?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let admin = proposal_identity(&db, "owner", &["ledger/admin"])?;
    let verified = identity::verify(db.read(), admin["token"].as_str().ok_or("admin token")?)?;
    let owner = Actor::new("owner".parse()?).with_identity(verified);
    Granted::<Admin>::with_db(&db, &owner, |grant| db::audit::set_policy(grant, true))?;
    let mut client = identity_client(&path, &passphrase_file, Some(token), false)?;
    let listed = client.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert_eq!(names.len(), 47);
    assert!(names.contains("audit_receipts"));
    assert!(!names.contains("audit_undo"));
    assert!(names.contains("ledger_setup_status"));
    assert!(names.contains("ledger_setup_statement_receipt"));
    assert!(names.contains("ledger_setup_book_show"));
    assert!(names.contains("financial_summary"));
    assert!(names.contains("period_list"));
    assert!(names.contains("reconcile_list"));
    assert!(!names.contains("reconcile_statement"));
    assert!(!names.contains("period_close"));
    assert!(!names.contains("period_reopen"));
    for name in names {
        assert_eq!(
            name.parse::<beankeeper_cli::operation::Operation>()?
                .primary_capability()?,
            beankeeper_cli::capability::Capability::Read
        );
    }
    let facts = client.success("ledger_setup_book_show", &json!({"company":"acme"}))?;
    assert_eq!(facts["company"], "acme");
    assert!(facts["profile"].is_null());
    client.success("company_list", &json!({}))?;
    for kind in ["schedule-c", "1099", "1099-nec"] {
        client.success(
            "report",
            &json!({"company":"acme", "kind":kind, "to":"2026-12-31"}),
        )?;
    }
    let audit = client.success(
        "audit_log",
        &json!({"entity":"policy", "action":"policy_change"}),
    )?;
    assert_eq!(audit.as_array().map(Vec::len), Some(1));
    let response = client.call("txn_post", &posting("hidden"))?;
    assert!(response.get("error").is_some() || response["result"]["isError"] == true);
    let revoked_jti = issued["token_jti"].as_str().ok_or("jti")?;
    Granted::<Admin>::with_db(&db, &owner, |grant| identity::revoke(grant, revoked_jti))?;
    let response = client.call("company_list", &json!({}))?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "UNAUTHORIZED");
    Ok(())
}

#[test]
fn live_policy_change_rechecks_capabilities_and_missing_session_tokens() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    let dir = tempdir()?;
    let path = dir.path().join("identity.db");
    let passphrase_file = dir.path().join("passphrase");
    let issued = identity_ledger(&path, &passphrase_file)?;
    let token = issued["token"].as_str().ok_or("token")?;
    let mut attested = identity_client(&path, &passphrase_file, Some(token), false)?;
    let mut named = identity_client(&path, &passphrase_file, None, false)?;
    let listed = attested.request("tools/list", &json!({}))?;
    assert!(tool_names(&listed)?.contains("txn_reconcile"));
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        |grant| db::audit::set_policy(grant, true),
    )?;
    for (name, input, capability) in [
        ("txn_post", posting("forbidden"), "ledger/post"),
        (
            "txn_propose",
            posting("forbidden-proposal"),
            "ledger/propose",
        ),
        (
            "txn_approve",
            json!({"company":"acme", "id":"proposal_00000000000000000000000000"}),
            "ledger/approve",
        ),
        (
            "txn_reject",
            json!({"company":"acme", "id":"proposal_00000000000000000000000000"}),
            "ledger/propose",
        ),
        ("txn_reconcile", json!({}), "ledger/reconcile"),
        (
            "company_create",
            json!({"slug":"empty", "name":"Empty"}),
            "ledger/admin",
        ),
        ("company_delete", json!({"slug":"acme"}), "ledger/admin"),
        (
            "account_create",
            json!({"company":"acme", "code":"2000", "name":"Loan", "account_type":"liability"}),
            "ledger/admin",
        ),
    ] {
        let result = attested.call(name, &input)?;
        assert_eq!(result["result"]["isError"], true);
        let data = payload(&result)?;
        assert_eq!(data["code"], "FORBIDDEN");
        assert!(
            data["message"]
                .as_str()
                .is_some_and(|message| message.contains(capability))
        );
    }
    attested.success("company_list", &json!({}))?;
    let denied = named.call("company_list", &json!({}))?;
    assert_eq!(payload(&denied)?["code"], "UNAUTHORIZED");
    assert_eq!(
        db.read()
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row
                .get::<_, i64>(0))?,
        0
    );
    // Once enforcement is active, a tokenless new process fails before protocol output.
    let output = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args(["--json", "mcp", "serve"])
        .output()?;
    assert_eq!(output.status.code(), Some(6));
    assert_eq!(output.stdout, Vec::<u8>::new());
    Ok(())
}

#[test]
fn enforced_post_session_and_process_read_only_ceiling_remain_independent() -> TestResult {
    use beankeeper_cli::db::{self, Actor, Db};
    use beankeeper_cli::identity::{self, IssueRequest};
    let dir = tempdir()?;
    let path = dir.path().join("identity.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let issued = Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "poster",
                capabilities: &["ledger/read".into(), "ledger/post".into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    let token = issued["token"].as_str().ok_or("token")?;
    beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        |grant| db::audit::set_policy(grant, true),
    )?;
    let mut writer = identity_client(&path, &passphrase_file, Some(token), false)?;
    let listed = writer.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert!(names.contains("txn_post"));
    assert!(names.contains("txn_reverse"));
    assert!(names.contains("audit_undo"));
    for forbidden in ["txn_reconcile", "txn_clear", "budget_set", "account_create"] {
        assert!(!names.contains(forbidden));
    }
    let posted = writer.success("txn_post", &posting("enforced"))?;
    let shown = writer.success("txn_show", &json!({"company":"acme", "id":posted["id"]}))?;
    assert_eq!(shown[0]["agent_uri"], issued["agent_uri"]);
    let mut reader = identity_client(&path, &passphrase_file, Some(token), true)?;
    let listed = reader.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert_eq!(names.len(), 47);
    assert!(names.contains("audit_receipts"));
    assert!(!names.contains("audit_undo"));
    assert!(names.contains("ledger_setup_status"));
    assert!(names.contains("ledger_setup_statement_receipt"));
    assert!(names.contains("ledger_setup_book_show"));
    assert!(!names.contains("txn_post"));
    writer.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn period_close_and_reopen_apply_to_mcp_postings_and_live_cli_changes() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let close = client.success(
        "period_close",
        &json!({"company":"acme", "through":"2026-03-31"}),
    )?;
    assert_eq!(close[0]["closed_through"], "2026-03-31");
    let response = client.call("txn_post", &posting("closed-mcp"))?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "VALIDATION");
    assert!(
        payload(&response)?["message"]
            .as_str()
            .is_some_and(|s| s.contains("closed period"))
    );
    let mut later = posting("later-mcp");
    later["date"] = json!("2026-04-01");
    client.success("txn_post", &later)?;
    client.success("verify", &json!({}))?;
    let open = client.success(
        "period_reopen",
        &json!({"company":"acme", "reason":"Correct receipt"}),
    )?;
    assert!(open[0]["closed_through"].is_null());
    client.success("txn_post", &posting("closed-mcp"))?;
    // Existing sessions see closes performed by another process.
    bk(&path)?
        .args([
            "--company",
            "acme",
            "period",
            "close",
            "--through",
            "2026-03-31",
        ])
        .assert()
        .success();
    let response = client.call("txn_post", &posting("after-cli-close"))?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "VALIDATION");
    assert_eq!(
        client.success("period_list", &json!({"company":"acme"}))?[0]["closed_through"],
        "2026-03-31"
    );
    client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn financial_summary_matches_cli_envelopes_in_read_only_mode() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("summary.db");
    seed(&path)?;
    for (date, amount) in [("2024-03-01", "12.34"), ("2024-02-01", "1")] {
        bk(&path)?
            .args([
                "--company",
                "acme",
                "txn",
                "post",
                "-d",
                "Sale",
                "--date",
                date,
                "--debit",
                &format!("1000:{amount}"),
                "--credit",
                &format!("4000:{amount}"),
            ])
            .assert()
            .success();
    }
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    assert!(tool_names(&listed)?.contains("financial_summary"));
    let mut actual = client.success(
        "financial_summary",
        &json!({"company":"acme", "to":"2024-03-31"}),
    )?;
    let output = bk(&path)?
        .args([
            "--company",
            "acme",
            "summary",
            "--json",
            "--to",
            "2024-03-31",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected: Value = serde_json::from_slice(&output.stdout)?;
    actual["meta"]
        .as_object_mut()
        .ok_or("meta")?
        .remove("timestamp");
    expected["meta"]
        .as_object_mut()
        .ok_or("meta")?
        .remove("timestamp");
    assert_eq!(actual, expected);
    let pending = &actual["data"]["needs_attention"]["items"]["unreconciled_months"]["months"][0];
    assert_eq!(pending["period_end"], "2024-02-29");
    assert!(pending["account_name"].is_string());
    assert!(
        pending["message"]
            .as_str()
            .is_some_and(|message| message.contains("February 2024 statement checked"))
    );

    assert_eq!(actual["data"]["comparison"]["series"][0]["current"], 1234);

    let today = chrono::Local::now().date_naive().to_string();
    let with_default = client.success("financial_summary", &json!({"company":"acme"}))?;
    let with_explicit_end =
        client.success("financial_summary", &json!({"company":"acme", "to":today}))?;
    assert_eq!(with_default["data"], with_explicit_end["data"]);

    for (company, to, code) in [
        ("acme", "bad", "VALIDATION"),
        ("missing", "2024-03-31", "NOT_FOUND"),
    ] {
        let response = client.call("financial_summary", &json!({"company":company,"to":to}))?;
        assert_eq!(response["result"]["isError"], true);
        let actual = payload(&response)?;
        assert_eq!(actual["ok"], false);
        assert_eq!(actual["meta"]["command"], "summary");
        assert_eq!(actual["meta"]["company"], company);
        assert_eq!(actual["error"]["code"], code);
        assert!(actual.get("data").is_none());
    }
    let partial_comparison = client.call(
        "financial_summary",
        &json!({"company":"acme", "compare_from":"2024-02-01"}),
    )?;
    assert_eq!(partial_comparison["result"]["isError"], true);
    assert_eq!(payload(&partial_comparison)?["error"]["code"], "VALIDATION");
    let unknown = client.call(
        "financial_summary",
        &json!({"company":"acme", "db":"other.db"}),
    )?;
    assert!(unknown.get("error").is_some() || unknown["result"]["isError"] == true);
    Ok(())
}

#[test]
fn comparisons_and_explanations_match_cli_on_read_only_stdio() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("compare.db");
    seed(&path)?;
    bk(&path)?
        .args([
            "--company",
            "acme",
            "account",
            "create",
            "5000",
            "Rent",
            "--type",
            "expense",
        ])
        .assert()
        .success();
    for (date, amount) in [("2024-01-15", "100"), ("2024-02-15", "160")] {
        bk(&path)?
            .args([
                "--company",
                "acme",
                "txn",
                "post",
                "-d",
                "Rent",
                "--date",
                date,
                "--debit",
                &format!("5000:{amount}"),
                "--credit",
                &format!("1000:{amount}"),
            ])
            .assert()
            .success();
    }
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for (kind, extra_cli, extra_json) in [
        ("income-statement", vec![], json!({})),
        ("trial-balance", vec![], json!({})),
        (
            "trend",
            vec!["--metric", "spending"],
            json!({"metric":"spending"}),
        ),
        (
            "trend",
            vec!["--metric", "net-worth"],
            json!({"metric":"net-worth"}),
        ),
        ("cash-flow", vec![], json!({})),
        (
            "balance",
            vec!["--account", "1000"],
            json!({"account":"1000"}),
        ),
        ("balance-sheet", vec![], json!({})),
    ] {
        let explain =
            if matches!(kind, "balance" | "balance-sheet") || extra_json["metric"] == "net-worth" {
                "1000"
            } else {
                "5000"
            };
        let mut input = json!({"company":"acme","kind":kind,"to":"2024-02-29","compare":"previous","explain_change":explain});
        input
            .as_object_mut()
            .ok_or("missing input")?
            .extend(extra_json.as_object().ok_or("missing extras")?.clone());
        let mut args = vec![
            "--json",
            "--company",
            "acme",
            "report",
            kind,
            "--to",
            "2024-02-29",
            "--compare",
            "previous",
            "--explain-change",
            explain,
        ];
        args.extend(extra_cli);
        let output = bk(&path)?.args(args).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: Value = serde_json::from_slice(&output.stdout)?;
        let mcp = client.success("report", &input)?;
        assert_eq!(mcp, cli["data"], "{kind}: {input}");
    }
    Ok(())
}

fn seed_chart_roles(path: &Path) -> TestResult {
    bk(path)?
        .args([
            "--company",
            "acme",
            "account",
            "create",
            "2000",
            "Cash",
            "--type",
            "liability",
        ])
        .assert()
        .success();
    bk(path)?
        .args([
            "--company",
            "acme",
            "account",
            "create",
            "5000",
            "Assets",
            "--type",
            "expense",
        ])
        .assert()
        .success();
    bk(path)?
        .args([
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "fictional debt",
            "--date",
            "2024-01-15",
            "--debit",
            "5000:40",
            "--credit",
            "2000:40",
        ])
        .assert()
        .success();
    Ok(())
}

fn chart_report_cases() -> [(Value, Vec<&'static str>); 9] {
    [
        (
            json!({"kind":"trend", "metric":"spending"}),
            vec!["trend", "--metric", "spending"],
        ),
        (
            json!({"kind":"trend", "metric":"net-worth"}),
            vec!["trend", "--metric", "net-worth"],
        ),
        (
            json!({"kind":"trend", "metric":"net-worth", "compare":"previous"}),
            vec!["trend", "--metric", "net-worth", "--compare", "previous"],
        ),
        (
            json!({"kind":"cash-flow", "compare":"previous"}),
            vec!["cash-flow", "--compare", "previous"],
        ),
        (json!({"kind":"cash-flow"}), vec!["cash-flow"]),
        (json!({"kind":"income-statement"}), vec!["income-statement"]),
        (
            json!({"kind":"income-statement", "compare":"prior-year"}),
            vec!["income-statement", "--compare", "prior-year"],
        ),
        (
            json!({"kind":"trial-balance", "compare":"2023-04-01..2023-06-30"}),
            vec!["trial-balance", "--compare", "2023-04-01..2023-06-30"],
        ),
        (
            json!({"kind":"trial-balance", "period":"quarter"}),
            vec!["trial-balance", "--period", "quarter"],
        ),
    ]
}

#[test]
fn requested_charts_over_read_only_stdio_match_cli_and_are_discoverable() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("chart.db");
    seed(&path)?;
    seed_chart_roles(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    for name in ["report", "financial_summary"] {
        let tool = listed["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .find(|tool| tool["name"] == name)
            .ok_or("tool")?;
        let schema = tool["inputSchema"].to_string();
        assert!(schema.contains("chart_all"), "{schema}");
        assert!(schema.contains("\"chart\""), "{schema}");
    }
    for (input, flags) in chart_report_cases() {
        for all in [false, true] {
            let mut input = input.clone();
            input["company"] = json!("acme");
            input["chart"] = json!(true);
            input["chart_all"] = json!(all);
            input["from"] = json!("2024-01-01");
            input["to"] = json!("2024-03-31");
            let actual = client.success("report", &input)?;
            let mut args = vec!["--company", "acme", "--json", "--chart"];
            if all {
                args.push("--chart-all");
            }
            args.push("report");
            args.extend(&flags);
            args.extend(["--from", "2024-01-01", "--to", "2024-03-31"]);
            let output = bk(&path)?.args(args).output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expected: Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(actual, expected["data"]);
            assert_eq!(actual["chart"]["schema_version"], 1);
            if input["metric"] == "net-worth" {
                let series = &actual["chart"]["series"];
                assert_eq!(series[0]["role"], "asset");
                assert_eq!(series[1]["role"], "liability");
                assert_eq!(series[2]["role"], "net_worth");
                assert_eq!(series[2]["values"][0], "-40.00");
            }
            for series in actual["chart"]["series"].as_array().ok_or("series")? {
                assert!(series["role"].is_string());
            }
        }
    }
    let actual = client.success(
        "financial_summary",
        &json!({"company":"acme", "chart":true, "chart_all":true, "to":"2024-03-31"}),
    )?;
    let output = bk(&path)?
        .args([
            "--company",
            "acme",
            "--json",
            "--chart",
            "--chart-all",
            "summary",
            "--to",
            "2024-03-31",
        ])
        .output()?;
    assert!(output.status.success());
    let expected: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(actual["data"], expected["data"]);
    Ok(())
}

#[test]
fn invalid_chart_requests_keep_each_mcp_tools_error_contract() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("chart.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for (tool, input) in [
        (
            "report",
            json!({"company":"acme", "kind":"balance-sheet", "chart":true}),
        ),
        (
            "financial_summary",
            json!({"company":"acme", "chart_all":true}),
        ),
    ] {
        let result = client.request("tools/call", &json!({"name":tool,"arguments":input}))?;
        assert_eq!(result["result"]["isError"], true, "{result}");
        let error = payload(&result)?;
        let code = if tool == "financial_summary" {
            &error["error"]["code"]
        } else {
            &error["code"]
        };
        assert_eq!(code, "USAGE", "{result}");
    }
    Ok(())
}

#[test]
fn posting_preserves_structured_annotation_keys_and_line_memos() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let mut request = posting("structured-annotations");
    request["meta"] = json!({"a=b": "c=d"});
    request["debits"][0]["memo"] = json!("payee: invoice=one");
    let posted = client.success("txn_post", &request)?;
    let data = client.success("txn_show", &json!({"company":"acme", "id":posted["id"]}))?;
    let metadata = data[0]["metadata"]
        .as_str()
        .ok_or("stored metadata missing")?;
    let annotations: Value = serde_json::from_str(metadata)?;
    assert_eq!(annotations, json!({"a=b": "c=d"}));
    assert_eq!(data[0]["entries"][0]["memo"], "payee: invoice=one");
    Ok(())
}

#[test]
fn account_list_applies_typed_balance_dates_and_currency() -> TestResult {
    let dir = tempdir()?;
    let db = dir.path().join("books.db");
    seed(&db)?;
    for (date, currency, amount) in [
        ("2026-01-09", "BHD", "10.001"),
        ("2026-01-10", "BHD", "20.002"),
        ("2026-01-10", "USD", "100"),
    ] {
        bk(&db)?
            .args([
                "--company",
                "acme",
                "txn",
                "post",
                "-d",
                "Sale",
                "--date",
                date,
                "--currency",
                currency,
                "--debit",
                &format!("1000:{amount}"),
                "--credit",
                &format!("4000:{amount}"),
            ])
            .assert()
            .success();
    }
    let mut command = bk(&db)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let tool = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "account_list")
        .ok_or("missing account_list")?;
    let description = tool["description"].as_str().ok_or("missing description")?;
    assert!(description.contains("with_balances=true"));
    assert!(description.contains("currency"));
    let basic = client.success("account_list", &json!({"company":"acme"}))?;
    assert_eq!(basic[0]["code"], "1000");
    assert!(basic[0].get("debit_total").is_none());
    let arguments = json!({
        "company":"acme", "with_balances":true,
        "from":"2026-01-10", "to":"2026-01-10", "currency":"BHD"
    });
    let data = client.success("account_list", &arguments)?;
    assert_eq!(data[0]["code"], "1000");
    assert_eq!(data[0]["currency"], "BHD");
    assert_eq!(data[0]["debit_total"], 20_002);
    assert_eq!(data[0]["debit_total_display"], "20.002");
    assert_eq!(data[1]["credit_total"], 20_002);
    Ok(())
}

#[test]
fn setup_status_matches_cli_for_an_empty_encrypted_ledger() -> TestResult {
    let temp = tempdir()?;
    let path = temp.path().join("setup.db");
    let passphrase = temp.path().join("passphrase");
    std::fs::write(&passphrase, "test-only-setup-passphrase")?;
    let command = || {
        let mut command = bk(&path)?;
        command.arg("--passphrase-file").arg(&passphrase);
        Ok::<_, Box<dyn Error>>(command)
    };

    command()?.args(["init", "--encrypt"]).assert().success();
    let report = command()?.args(["setup", "status", "--json"]).output()?;
    assert!(report.status.success());
    let report: Value = serde_json::from_slice(&report.stdout)?;
    let mut server = command()?;
    server.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(server)?;
    client.initialize()?;
    let tools = client.request("tools/list", &json!({}))?;
    assert!(tool_names(&tools)?.contains("ledger_setup_status"));
    let expected = json!({
        "companies": 0, "accounts": 0, "transactions": 0, "owner_key_initialized": false,
    });
    assert_eq!(report["data"], expected);
    assert_eq!(client.success("ledger_setup_status", &json!({}))?, expected);
    Ok(())
}

#[test]
fn setup_book_show_reads_previous_schema_without_migration() -> TestResult {
    use beankeeper::types::AccountType;
    use beankeeper_cli::commands::setup::{BookAccountCode, book_facts};
    use beankeeper_cli::db::Db;

    let temp = tempdir()?;
    let path = temp.path().join("legacy.db");
    let current_path = temp.path().join("current.db");
    seed(&path)?;
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute(
        "INSERT INTO accounts(company_slug, code, name, type)
         VALUES ('acme', 'CASH', 'Legacy Cash', 'asset')",
        [],
    )?;
    // Use a real current schema for the typed and MCP comparisons. Only the
    // read-only setup commands see the fixture with its version rolled back.
    connection.execute("VACUUM INTO ?1", [current_path.to_string_lossy().as_ref()])?;
    connection.execute_batch(
        "DROP TABLE setup_requests;
         DROP TABLE setup_book_profiles;
         DROP TABLE setup_statement_batches;
         DELETE FROM schema_version WHERE version >= 22;",
    )?;
    let read = bk(&path)?
        .args(["--json", "setup", "book-show", "acme"])
        .output()?;
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let facts = serde_json::from_slice::<Value>(&read.stdout)?;
    assert_eq!(facts["data"]["company"], "acme");
    assert!(facts["data"]["profile"].is_null());
    assert_eq!(facts["data"]["accounts"][0]["code"], "1000");
    let legacy = facts["data"]["accounts"]
        .as_array()
        .ok_or("missing setup accounts")?
        .iter()
        .find(|account| account["code"] == "CASH")
        .ok_or("missing legacy account")?;
    assert_eq!(legacy["name"], "Legacy Cash");
    assert_eq!(legacy["type"], "asset");
    let status = bk(&path)?.args(["--json", "setup", "status"]).output()?;
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let version: i64 =
        connection.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    assert_eq!(version, 21);
    drop(connection);

    let db = Db::open_existing(&current_path, None)?;
    let company = "acme".parse()?;
    let typed = book_facts(&db, &company)?;
    assert!(
        matches!(&typed.accounts[0].code, BookAccountCode::Validated(code) if code.as_str() == "1000")
    );
    let typed_legacy = typed
        .accounts
        .iter()
        .find(|account| account.code.as_str() == "CASH")
        .ok_or("missing typed legacy account")?;
    assert!(matches!(&typed_legacy.code, BookAccountCode::Legacy(code) if code == "CASH"));
    assert_eq!(typed_legacy.account_type, AccountType::Asset);
    assert_eq!(serde_json::to_value(typed)?, facts["data"]);

    let mut server = bk(&current_path)?;
    server.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(server)?;
    client.initialize()?;
    assert_eq!(
        client.success("ledger_setup_book_show", &json!({"company":"acme"}))?,
        facts["data"]
    );
    Ok(())
}

#[test]
fn owner_manages_setup_session_across_cli_and_mcp() -> TestResult {
    use agent_uri_attestation::SigningKey;
    use beankeeper_cli::db::{Actor, Db};

    let directory = tempdir()?;
    let path = directory.path().join("setup.db");
    let passphrase_path = directory.path().join("passphrase");
    let passphrase: secrecy::SecretString = "mcp-setup-test".into();
    std::fs::write(&passphrase_path, "mcp-setup-test")?;
    let db = Db::open(&path, Some(&passphrase))?;
    Granted::<Admin>::with_db(
        &db,
        &Actor::new("owner".parse()?),
        beankeeper_cli::identity::init_root,
    )?;
    let owner = proposal_identity(&db, "owner", &["ledger/admin"])?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    let public_key = SigningKey::generate().verifying_key().to_base64();
    let mut owner_client = identity_client(&path, &passphrase_path, Some(owner_token), false)?;
    let started = owner_client.success(
        "ledger_setup_begin",
        &json!({ "name": "setup_agent", "public_key": public_key }),
    )?;
    assert_eq!(
        started["credential"]["capabilities"],
        json!(["ledger/setup"])
    );
    assert!(started["credential"].get("private_key").is_none());
    let setup_token = started["credential"]["token"]
        .as_str()
        .ok_or("missing setup token")?;
    let generation = started["session"]["generation"]
        .as_i64()
        .ok_or("missing setup generation")?;

    let mut setup_client = identity_client(&path, &passphrase_path, Some(setup_token), false)?;
    assert_setup_tools(&mut setup_client)?;
    let status = setup_client.success("ledger_setup_status", &json!({}))?;
    assert_eq!(status["transactions"], 0);

    let book = exercise_setup_books(&mut setup_client, &mut owner_client, &db, generation)?;
    assert_setup_book_facts(&mut setup_client, &path, &passphrase_path, setup_token)?;
    assert_existing_book_initializes_in_place(
        &mut setup_client,
        &db,
        &path,
        &passphrase_path,
        generation,
    )?;
    assert_cli_setup_book_creation(&path, &passphrase_path, setup_token, generation, &db, &book)?;

    assert_opening_preview(
        &mut setup_client,
        &db,
        &path,
        &passphrase_path,
        setup_token,
        generation,
    )?;

    let (mut approver, request, receipt) = assert_statement_import(
        &mut setup_client,
        &db,
        &path,
        &passphrase_path,
        setup_token,
        generation,
    )?;
    let (late_proposal, reviewer_token) = assert_opening_reservation(
        &mut setup_client,
        &db,
        &path,
        &passphrase_path,
        setup_token,
        generation,
    )?;

    finish_setup_and_check_revocation(
        &mut owner_client,
        &mut setup_client,
        &path,
        &passphrase_path,
        owner_token,
        generation,
    )?;
    assert_eq!(
        approver.success("ledger_setup_statement_commit", &request)?,
        receipt
    );
    assert_typed_statement_receipt(&path, &passphrase_path, &receipt)?;
    assert_opening_requires_review_after_finish(
        &db,
        &path,
        &passphrase_path,
        &late_proposal,
        &reviewer_token,
    )
}

fn assert_setup_tools(client: &mut Client) -> TestResult {
    let listed = client.request("tools/list", &json!({}))?;
    assert_eq!(
        tool_names(&listed)?,
        BTreeSet::from([
            "ledger_setup_status",
            "ledger_setup_book_show",
            "ledger_setup_book_create",
            "ledger_setup_book_initialize",
            "ledger_setup_book_update",
            "ledger_setup_chart_apply",
            "ledger_setup_account_add",
            "ledger_setup_opening_preview",
            "ledger_setup_opening_reserve",
            "ledger_setup_statement_preview",
        ])
    );
    Ok(())
}

fn assert_typed_statement_receipt(path: &Path, passphrase: &Path, wire: &Value) -> TestResult {
    use beankeeper_cli::commands::setup::{StatementFormat, statement_receipt};
    use beankeeper_cli::db::Db;

    let passphrase = secrecy::SecretString::from(std::fs::read_to_string(passphrase)?);
    let snapshot = Db::open_snapshot(path, Some(&passphrase))?;
    let receipt = statement_receipt(
        &snapshot,
        &"home".parse()?,
        serde_json::from_value(wire["batch_id"].clone())?,
    )?;
    assert_eq!(receipt.format, StatementFormat::Csv);
    assert_eq!(receipt.source_sha256.as_str(), wire["source_sha256"]);
    assert_eq!(serde_json::to_value(receipt)?, *wire);
    Ok(())
}

fn finish_setup_and_check_revocation(
    owner_client: &mut Client,
    setup_client: &mut Client,
    path: &Path,
    passphrase: &Path,
    owner_token: &str,
    generation: i64,
) -> TestResult {
    let ended = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args(["--agent-token", owner_token, "--json", "setup", "finish"])
        .arg(generation.to_string())
        .output()?;
    assert!(
        ended.status.success(),
        "{}",
        String::from_utf8_lossy(&ended.stderr)
    );
    let ended: Value = serde_json::from_slice(&ended.stdout)?;
    assert_eq!(ended["data"]["state"], "finished");
    assert_eq!(
        owner_client.success("ledger_setup_finish", &json!({ "generation": generation }))?,
        ended["data"]
    );
    let denied = setup_client.call("ledger_setup_status", &json!({}))?;
    assert_eq!(payload(&denied)?["code"], "UNAUTHORIZED");
    let denied = setup_client.call(
        "ledger_setup_opening_preview",
        &opening_preview_input(generation),
    )?;
    assert_eq!(payload(&denied)?["code"], "UNAUTHORIZED");
    Ok(())
}

fn assert_statement_import(
    setup: &mut Client,
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    setup_token: &str,
    generation: i64,
) -> Result<(Client, Value, Value), Box<dyn Error>> {
    let owner = proposal_identity(
        db,
        "statement_approver",
        &["ledger/approve", "ledger/read", "ledger/reverse"],
    )?;
    let token = owner["token"]
        .as_str()
        .ok_or("missing statement approver token")?;
    let mut approver = identity_client(path, passphrase, Some(token), false)?;
    let mut local = identity_client(path, passphrase, None, false)?;
    let existing = local.success(
        "txn_post",
        &json!({
            "company":"home", "date":"2026-05-02", "description":"Existing",
            "currency":"CAD", "reference":"already-imported",
            "debits":[{"account":"1010", "amount":"1.25"}],
            "credits":[{"account":"3000", "amount":"1.25"}]
        }),
    )?;
    let source = json!({
        "company":"home", "generation":generation, "reference":"may-statement",
        "bank":"1010", "suspense":"3000", "currency":"CAD", "format":"csv",
        "content":"date,description,amount,reference
2026-05-01,Import,12.34,may-bank-1
2026-05-02,Existing,1.25,already-imported
"
    });
    let audit = setup_audit_count(db)?;
    let preview = setup.success("ledger_setup_statement_preview", &source)?;
    assert_eq!(setup_audit_count(db)?, audit + 1);
    assert_eq!(
        setup.success("ledger_setup_statement_preview", &source)?,
        preview
    );
    assert_eq!(setup_audit_count(db)?, audit + 1);
    assert_cli_replays_statement_preview(path, passphrase, setup_token, &source, &preview)?;
    assert_eq!(setup_audit_count(db)?, audit + 1);
    assert_eq!(preview["rows"][0]["entries"][0]["amount_minor"], "1234");
    assert_eq!(preview["rows"][1]["existing_transaction"], existing["id"]);
    let mut changed = source.clone();
    changed["content"] = json!(
        "date,description,amount
2026-05-01,Different,12.34
"
    );
    assert_eq!(
        payload(&setup.call("ledger_setup_statement_preview", &changed)?)?["code"],
        "VALIDATION"
    );

    let approval = json!({"company":"home", "batch_id": preview["batch_id"],
        "preview_sha256": preview["preview_sha256"]});
    assert_statement_requires_approval(setup, &mut approver, &mut local, &approval)?;
    let receipt = approver.success("ledger_setup_statement_commit", &approval)?;
    assert_eq!(
        receipt["preexisting"],
        json!([{"reference":"already-imported", "transaction_id":existing["id"]}])
    );
    assert_eq!(receipt["created"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt["created"][0]["reference"], "may-bank-1");
    assert_eq!(receipt["reference"], "may-statement");
    assert_eq!(receipt["format"], "csv");
    assert_eq!(receipt["preview_sha256"], preview["preview_sha256"]);
    assert_eq!(receipt["source_sha256"], preview["source_sha256"]);
    assert_eq!(receipt["approved_by_jti"], owner["token_jti"]);
    let lookup = json!({"company":"home", "batch_id":preview["batch_id"]});
    assert_eq!(
        approver.success("ledger_setup_statement_receipt", &lookup)?,
        receipt
    );
    let audit = setup_audit_count(db)?;
    assert_eq!(
        approver.success("ledger_setup_statement_commit", &approval)?,
        receipt
    );
    assert_eq!(setup_audit_count(db)?, audit);
    approver.success("txn_reverse", &json!({
        "company":"home", "id":receipt["created"][0]["transaction_id"],
        "date":"2026-05-03", "description":"Correct imported transaction", "reference":"may-bank-1-reversal"
    }))?;
    assert_eq!(
        approver.success("ledger_setup_statement_receipt", &lookup)?,
        receipt
    );
    setup_investment_parity::assert_cli_statement_receipts(
        path, passphrase, token, &approval, &receipt,
    )?;
    assert_statement_collision_rolls_back(setup, &mut approver, &mut local, db, source)?;
    approver.success("verify", &json!({}))?;
    Ok((approver, approval, receipt))
}

fn assert_cli_replays_statement_preview(
    path: &Path,
    passphrase: &Path,
    token: &str,
    source: &Value,
    preview: &Value,
) -> TestResult {
    let mut content = tempfile::NamedTempFile::new()?;
    let statement = source["content"]
        .as_str()
        .ok_or("missing statement content")?;
    content.write_all(statement.as_bytes())?;
    content.flush()?;
    let generation = source["generation"].to_string();
    let replay = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--agent-token",
            token,
            "--json",
            "setup",
            "statement-preview",
            "home",
        ])
        .arg(content.path())
        .args([
            "--generation",
            &generation,
            "--reference",
            "may-statement",
            "--bank",
            "1010",
            "--suspense",
            "3000",
            "--currency",
            "CAD",
            "--statement-format",
            "csv",
        ])
        .output()?;
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay: Value = serde_json::from_slice(&replay.stdout)?;
    assert_eq!(&replay["data"], preview);
    Ok(())
}

fn assert_statement_requires_approval(
    setup: &mut Client,
    approver: &mut Client,
    local: &mut Client,
    approval: &Value,
) -> TestResult {
    let denied = setup.call("ledger_setup_statement_commit", approval)?;
    assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    assert_eq!(
        payload(&local.call("ledger_setup_statement_commit", approval)?)?["code"],
        "UNAUTHORIZED"
    );
    let mut wrong_hash = approval.clone();
    wrong_hash["preview_sha256"] = json!("0".repeat(64));
    assert_eq!(
        payload(&approver.call("ledger_setup_statement_commit", &wrong_hash)?)?["code"],
        "VALIDATION"
    );
    let mut wrong_book = approval.clone();
    wrong_book["company"] = json!("studio");
    assert_eq!(
        payload(&approver.call("ledger_setup_statement_commit", &wrong_book)?)?["code"],
        "NOT_FOUND"
    );
    assert_eq!(
        local
            .success("txn_list", &json!({"company":"home"}))?
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

fn assert_statement_collision_rolls_back(
    setup: &mut Client,
    approver: &mut Client,
    local: &mut Client,
    db: &beankeeper_cli::db::Db,
    mut source: Value,
) -> TestResult {
    source["reference"] = json!("june-statement");
    source["content"] = json!(
        "date,description,amount,reference
2026-06-01,First,2,june-first
2026-06-02,Second,3,june-race
"
    );
    let preview = setup.success("ledger_setup_statement_preview", &source)?;
    local.success(
        "txn_post",
        &json!({
            "company":"home", "date":"2026-06-02", "description":"Another writer arrived",
            "currency":"CAD", "reference":"june-race",
            "debits":[{"account":"1010", "amount":"3"}],
            "credits":[{"account":"3000", "amount":"3"}]
        }),
    )?;
    let audit = setup_audit_count(db)?;
    let failed = approver.call("ledger_setup_statement_commit", &json!({
        "company":"home", "batch_id":preview["batch_id"], "preview_sha256": preview["preview_sha256"]
    }))?;
    assert_eq!(payload(&failed)?["code"], "VALIDATION");
    assert_eq!(setup_audit_count(db)?, audit);
    let posted: bool = db.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM transactions WHERE reference = 'june-first')",
        [],
        |row| row.get(0),
    )?;
    assert!(!posted);
    let receipt = approver.call(
        "ledger_setup_statement_receipt",
        &json!({
            "company":"home", "batch_id":preview["batch_id"]
        }),
    )?;
    assert_eq!(payload(&receipt)?["code"], "NOT_FOUND");
    assert_statement_import_summary(db)?;
    Ok(())
}

fn assert_statement_import_summary(db: &beankeeper_cli::db::Db) -> TestResult {
    use beankeeper_cli::commands::setup::{StatementImportSummary, statement_import_summary};
    use beankeeper_cli::error::CliError;

    assert_eq!(
        statement_import_summary(db, &"home".parse()?)?,
        StatementImportSummary {
            committed_batches: 1,
            created_transactions: 1,
            preexisting_matches: 1,
        }
    );
    assert_eq!(
        statement_import_summary(db, &"studio".parse()?)?,
        StatementImportSummary::default()
    );
    assert!(matches!(
        statement_import_summary(db, &"missing".parse()?),
        Err(CliError::NotFound(_))
    ));
    Ok(())
}

fn assert_cli_setup_book_creation(
    path: &Path,
    passphrase: &Path,
    token: &str,
    generation: i64,
    db: &beankeeper_cli::db::Db,
    expected: &Value,
) -> TestResult {
    let create = |slug: &str, name: &str| -> Result<Command, Box<dyn Error>> {
        let mut command = bk(path)?;
        command
            .arg("--passphrase-file")
            .arg(passphrase)
            .args([
                "--agent-token",
                token,
                "--json",
                "setup",
                "book-create",
                slug,
                name,
            ])
            .args([
                "--generation",
                &generation.to_string(),
                "--reference",
                &format!("{slug}-book"),
            ])
            .args([
                "--purpose",
                "personal",
                "--base-currency",
                "USD",
                "--fiscal-year-start",
                "01-01",
            ]);
        Ok(command)
    };
    let replay = create("home", "Home")?.output()?;
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        &serde_json::from_slice::<Value>(&replay.stdout)?["data"],
        expected
    );

    for name in BLANK_COMPANY_NAMES {
        assert_cli_rejects_company_name(&mut create("workshop", name)?)?;
    }
    create("workshop", "  Workshop\u{2003} ")?
        .assert()
        .success();
    let row = beankeeper_cli::db::get_company(db.read(), &"workshop".parse()?)?;
    assert_eq!(row.name, "Workshop");
    Ok(())
}

fn assert_existing_book_initializes_in_place(
    client: &mut Client,
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    generation: i64,
) -> TestResult {
    let run = |args: &[&str]| -> TestResult {
        bk(path)?
            .arg("--passphrase-file")
            .arg(passphrase)
            .args(args)
            .assert()
            .success();
        Ok(())
    };
    run(&["company", "create", "personal", "Personal"])?;
    run(&[
        "--company",
        "personal",
        "account",
        "create",
        "1000",
        "Existing Checking",
        "--type",
        "asset",
    ])?;
    run(&[
        "--company",
        "personal",
        "account",
        "create",
        "3000",
        "Existing Equity",
        "--type",
        "equity",
    ])?;
    run(&[
        "--company",
        "personal",
        "txn",
        "post",
        "-d",
        "Prior deposit",
        "--debit",
        "1000:41.25",
        "--credit",
        "3000:41.25",
        "--reference",
        "prior-deposit",
    ])?;

    let snapshot = || {
        db.read().query_row(
            "SELECT json_array(t.id, t.description, t.reference, t.currency,
                 (SELECT json_group_array(json_array(e.id, e.account_code, e.direction, e.amount))
                  FROM entries e WHERE e.transaction_id = t.id))
         FROM transactions t WHERE t.company_slug = 'personal'",
            [],
            |row| row.get::<_, String>(0),
        )
    };
    let transaction = snapshot()?;
    let company = json!({"company":"personal"});
    let prior = client.success("ledger_setup_book_show", &company)?;
    assert!(prior["profile"].is_null());

    let initial = json!({
        "target": setup_book_target("personal", generation, "desktop-personal-profile"),
        "settings": {"purpose":"personal", "base_currency":"USD", "fiscal_year_start":"01-01"},
    });
    let profile = client.success("ledger_setup_book_initialize", &initial)?;
    assert_eq!(profile["name"], "Personal");
    assert!(profile["profile"]["supersedes"].is_null());
    let facts = client.success("ledger_setup_book_show", &company)?;
    assert_eq!(facts["name"], prior["name"]);
    assert_eq!(facts["accounts"], prior["accounts"]);
    assert_eq!(facts["profile"], profile["profile"]);
    assert_eq!(transaction, snapshot()?);

    let audit = setup_audit_count(db)?;
    assert_eq!(
        client.success("ledger_setup_book_initialize", &initial)?,
        profile
    );
    let mut conflicting = initial.clone();
    conflicting["settings"]["fiscal_year_start"] = json!("04-01");
    let conflict = client.call("ledger_setup_book_initialize", &conflicting)?;
    assert_eq!(payload(&conflict)?["code"], "VALIDATION");
    conflicting = initial;
    conflicting["target"]["reference"] = json!("desktop-personal-profile-new");
    let already_profiled = client.call("ledger_setup_book_initialize", &conflicting)?;
    let error = payload(&already_profiled)?;
    assert_eq!(error["code"], "VALIDATION");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|text| text.contains("already has a setup profile"))
    );
    assert_eq!(setup_audit_count(db)?, audit);
    assert_eq!(transaction, snapshot()?);
    beankeeper_cli::db::audit::verify_hash_chain(db.read())?;
    Ok(())
}

fn assert_setup_book_facts(
    client: &mut Client,
    path: &Path,
    passphrase: &Path,
    token: &str,
) -> TestResult {
    let facts = client.success("ledger_setup_book_show", &json!({"company":"home"}))?;
    assert_eq!(facts["name"], "Home");
    assert_eq!(facts["profile"]["base_currency"], "CAD");
    assert_eq!(facts["profile"]["fiscal_year_start"], "07-01");
    assert_eq!(facts["accounts"][1]["code"], "1010");
    assert_eq!(facts["accounts"][1]["name"], "Chase Checking");
    assert_eq!(facts["accounts"][1]["type"], "asset");
    let cli = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--agent-token",
            token,
            "--json",
            "setup",
            "book-show",
            "home",
        ])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report = serde_json::from_slice::<Value>(&cli.stdout)?;
    assert_eq!(report["meta"]["command"], "ledger.setup.book.show");
    assert_eq!(report["data"], facts);
    Ok(())
}

fn exercise_setup_books(
    client: &mut Client,
    owner: &mut Client,
    db: &beankeeper_cli::db::Db,
    generation: i64,
) -> Result<Value, Box<dyn Error>> {
    let initial = json!({
        "target": setup_book_target("home", generation, "home-book"),
        "name": "  Home\u{2003} ",
        "settings": {"purpose": "personal", "base_currency":"USD", "fiscal_year_start":"01-01"}
    });
    let mut bad = initial.clone();
    for name in BLANK_COMPANY_NAMES {
        bad["name"] = json!(name);
        assert_parameter_rejection(
            &client.call("ledger_setup_book_create", &bad)?,
            COMPANY_NAME_ERROR,
        );
    }
    let book = client.success("ledger_setup_book_create", &initial)?;
    assert_eq!(book["name"], "Home");
    assert_eq!(book["profile"]["company"], "home");
    assert_eq!(book["profile"]["purpose"], "personal");
    let refusal = payload(&owner.call("company_delete", &json!({"slug":"home"}))?)?;
    assert_eq!(refusal["code"], "VALIDATION");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|text| text.contains("retained setup records"))
    );
    let count = setup_audit_count(db)?;
    assert_eq!(client.success("ledger_setup_book_create", &initial)?, book);
    assert_eq!(setup_audit_count(db)?, count);
    let mut bad = initial.clone();
    bad["target"]["generation"] = json!(generation + 1);
    assert_eq!(
        payload(&client.call("ledger_setup_book_create", &bad)?)?["code"],
        "FORBIDDEN"
    );
    bad["target"]["generation"] = json!(generation);
    bad["settings"]["base_currency"] = json!("CAD");
    assert_eq!(
        payload(&client.call("ledger_setup_book_create", &bad)?)?["code"],
        "VALIDATION"
    );

    let chart = json!({"target": setup_book_target("home", generation, "home-chart"), "template": "personal-v1"});
    let chart_result = client.success("ledger_setup_chart_apply", &chart)?;
    assert_eq!(
        chart_result["accounts"]
            .as_array()
            .ok_or("missing accounts")?
            .len(),
        5
    );
    let receipt_count = setup_audit_count(db)?;
    assert_eq!(
        client.success("ledger_setup_chart_apply", &chart)?,
        chart_result
    );
    assert_eq!(setup_audit_count(db)?, receipt_count);

    let named = json!({
        "target": setup_book_target("home", generation, "home-checking"),
        "code":"1010", "name":"Chase Checking", "account_type":"asset"
    });
    let account = client.success("ledger_setup_account_add", &named)?;
    assert_eq!(account["name"], "Chase Checking");
    assert_eq!(client.success("ledger_setup_account_add", &named)?, account);

    let revised = json!({
        "target": setup_book_target("home", generation, "home-profile-change"),
        "supersedes": book["profile"]["revision"],
        "settings": {"purpose":"personal", "base_currency":"CAD", "fiscal_year_start":"07-01"}
    });
    let update = client.success("ledger_setup_book_update", &revised)?;
    assert_eq!(update["supersedes"], book["profile"]["revision"]);
    assert_eq!(update["base_currency"], "CAD");
    assert_eq!(
        client.success("ledger_setup_book_update", &revised)?,
        update
    );
    let mut stale = revised;
    stale["target"]["reference"] = json!("home-profile-stale");
    assert_eq!(
        payload(&client.call("ledger_setup_book_update", &stale)?)?["code"],
        "VALIDATION"
    );

    assert_setup_chart_collision_rolls_back(client, db, generation)?;
    beankeeper_cli::db::audit::verify_hash_chain(db.read())?;
    Ok(book)
}

fn setup_book_target(company: &str, generation: i64, reference: &str) -> Value {
    json!({ "company": company, "generation": generation, "reference": reference })
}

fn setup_audit_count(db: &beankeeper_cli::db::Db) -> rusqlite::Result<i64> {
    db.read()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
}

fn assert_setup_chart_collision_rolls_back(
    client: &mut Client,
    db: &beankeeper_cli::db::Db,
    generation: i64,
) -> TestResult {
    client.success(
        "ledger_setup_book_create",
        &json!({
            "target": setup_book_target("studio", generation, "studio-book"),
            "name": "Studio",
            "settings":{"purpose":"business", "base_currency":"USD", "fiscal_year_start":"01-01"}
        }),
    )?;
    client.success(
        "ledger_setup_account_add",
        &json!({
            "target": setup_book_target("studio", generation, "studio-payables"),
            "code":"2000", "name":"Trade Payables", "account_type":"liability"
        }),
    )?;
    let count = setup_audit_count(db)?;
    let failed = client.call(
        "ledger_setup_chart_apply",
        &json!({
            "target": setup_book_target("studio", generation, "studio-chart"),
            "template": "business-v1"
        }),
    )?;
    assert_eq!(payload(&failed)?["code"], "VALIDATION");
    let count_accounts: i64 = db.read().query_row(
        "SELECT COUNT(*) FROM accounts WHERE company_slug = 'studio'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count_accounts, 1);
    assert_eq!(setup_audit_count(db)?, count);
    Ok(())
}

fn opening_preview_input(generation: i64) -> Value {
    json!({
        "company":"home", "generation": generation, "date":"2026-01-01", "currency":"BHD",
        "debits":[{"account":"1010", "amount":"12.345", "memo":"Opening asset"}],
        "credits":[{"account":"3000", "amount":"12.345"}],
    })
}

fn assert_opening_preview(
    client: &mut Client,
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    token: &str,
    generation: i64,
) -> TestResult {
    let state = || {
        db.read().query_row(
            "SELECT (SELECT COUNT(*) FROM audit_log), (SELECT COUNT(*) FROM transactions),
                (SELECT COUNT(*) FROM txn_proposals), (SELECT COUNT(*) FROM setup_requests)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
    };
    let before = state()?;
    let input = opening_preview_input(generation);
    let preview = client.success("ledger_setup_opening_preview", &input)?;
    assert_eq!(preview["date"], "2026-01-01");
    assert_eq!(preview["currency"], "BHD");
    assert_eq!(preview["entries"][0]["account_name"], "Chase Checking");
    assert_eq!(preview["entries"][0]["memo"], "Opening asset");
    assert_eq!(preview["entries"][1]["direction"], "credit");
    assert_eq!(
        preview["totals"]["debits"],
        json!({"amount_minor":"12345", "amount_display":"12.345"})
    );
    assert_eq!(preview["totals"]["debits"], preview["totals"]["credits"]);
    assert_cli_opening_preview_matches(path, passphrase, token, generation, &preview)?;

    let mut invalid = input.clone();
    invalid["credits"][0]["amount"] = json!("12.346");
    assert_eq!(
        payload(&client.call("ledger_setup_opening_preview", &invalid)?)?["code"],
        "VALIDATION"
    );
    invalid = input.clone();
    invalid["company"] = json!("studio");
    assert_eq!(
        payload(&client.call("ledger_setup_opening_preview", &invalid)?)?["code"],
        "VALIDATION"
    );
    invalid = input.clone();
    invalid["generation"] = json!(generation + 1);
    assert_eq!(
        payload(&client.call("ledger_setup_opening_preview", &invalid)?)?["code"],
        "FORBIDDEN"
    );

    let mut wide = input.clone();
    wide["debits"] = json!(vec![
        json!({"account":"1010", "amount":"9000000000000000"});
        3
    ]);
    wide["credits"] = json!(vec![
        json!({"account":"3000", "amount":"9000000000000000"});
        3
    ]);
    let wide = client.success("ledger_setup_opening_preview", &wide)?;
    assert_eq!(
        wide["totals"]["debits"]["amount_minor"],
        "27000000000000000000"
    );
    assert_eq!(
        wide["totals"]["debits"]["amount_display"],
        "27000000000000000.000"
    );
    assert_eq!(state()?, before);

    bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--company",
            "home",
            "period",
            "close",
            "--through",
            "2026-01-01",
        ])
        .assert()
        .success();
    let closed = state()?;
    let blocked = client.call("ledger_setup_opening_preview", &input)?;
    assert_eq!(payload(&blocked)?["code"], "VALIDATION");
    assert!(
        payload(&blocked)?["message"]
            .as_str()
            .is_some_and(|text| text.contains("closed period"))
    );
    assert_eq!(state()?, closed);
    Ok(())
}

fn assert_cli_opening_preview_matches(
    path: &Path,
    passphrase: &Path,
    token: &str,
    generation: i64,
    expected: &Value,
) -> TestResult {
    let cli = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--agent-token",
            token,
            "--json",
            "setup",
            "opening-preview",
            "home",
        ])
        .args([
            "--generation",
            &generation.to_string(),
            "--date",
            "2026-01-01",
            "--currency",
            "BHD",
        ])
        .args([
            "--debit",
            "1010:12.345:Opening asset",
            "--credit",
            "3000:12.345",
        ])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report = serde_json::from_slice::<Value>(&cli.stdout)?;
    assert_eq!(report["meta"]["command"], "ledger.setup.opening.preview");
    assert_eq!(&report["data"], expected);
    Ok(())
}

fn assert_opening_reservation(
    client: &mut Client,
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    token: &str,
    generation: i64,
) -> Result<(String, String), Box<dyn Error>> {
    assert!(!beankeeper_cli::db::policy::get(db.read())?.enforce);

    let owner_review = proposal_identity(db, "owner_review", &["ledger/approve"])?;
    let owner_token = owner_review["token"]
        .as_str()
        .ok_or("missing reviewer token")?;

    let mut input = opening_preview_input(generation);
    input["date"] = json!("2026-01-02");
    let preview = client.success("ledger_setup_opening_preview", &input)?;
    let fingerprint = preview["fingerprint"]
        .as_str()
        .ok_or("missing preview fingerprint")?;
    let reservation = json!({
        "opening": input, "reference": "opening-home-2026", "fingerprint": fingerprint,
    });
    let before = setup_audit_count(db)?;
    let mut wrong_generation = reservation.clone();
    wrong_generation["opening"]["generation"] = json!(generation + 1);
    let error = client.call("ledger_setup_opening_reserve", &wrong_generation)?;
    assert_eq!(payload(&error)?["code"], "FORBIDDEN");
    let mut stale = reservation.clone();
    stale["opening"]["description"] = json!("Changed opening");
    let error = client.call("ledger_setup_opening_reserve", &stale)?;
    assert_eq!(payload(&error)?["code"], "VALIDATION");
    assert_eq!(setup_audit_count(db)?, before);

    let reserved = client.success("ledger_setup_opening_reserve", &reservation)?;
    assert_eq!(reserved["generation"], generation);
    assert_eq!(reserved["reference"], "opening-home-2026");
    assert_eq!(reserved["preview"], preview);
    let id = reserved["proposal_id"]
        .as_str()
        .ok_or("missing proposal ID")?;
    let pending = beankeeper_cli::db::proposals::get(db.read(), &"home".parse()?, &id.parse()?)?;
    assert_eq!(pending.state.code(), "pending");
    assert_eq!(
        pending.payload.reference.as_deref(),
        Some("opening-home-2026")
    );
    assert_eq!(pending.debit_total, 12_345);
    assert_eq!(setup_audit_count(db)?, before + 1);
    let kind: String = db.read().query_row(
        "SELECT entity_id FROM audit_log WHERE company_slug = 'home' ORDER BY id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(kind, "setup.request.opening-home-2026");

    assert_cli_replays_opening_reservation(
        path,
        passphrase,
        token,
        generation,
        fingerprint,
        &reserved,
    )?;
    let error = client.call("ledger_setup_opening_reserve", &stale)?;
    assert_eq!(payload(&error)?["code"], "VALIDATION");
    assert_eq!(setup_audit_count(db)?, before + 1);
    assert_opening_approval_denials(db, path, passphrase, token, id)?;

    assert_owner_approves_opening(db, path, passphrase, owner_token, id)?;
    let approved_count = setup_audit_count(db)?;
    assert_eq!(
        client.success("ledger_setup_opening_reserve", &reservation)?,
        reserved
    );
    assert_eq!(setup_audit_count(db)?, approved_count);
    let mut late = reservation;
    late["reference"] = json!("opening-home-after-session");
    let late = client.success("ledger_setup_opening_reserve", &late)?;
    let late_id = late["proposal_id"]
        .as_str()
        .ok_or("missing later proposal ID")?;
    Ok((late_id.to_owned(), owner_token.to_owned()))
}

fn assert_opening_approval_denials(
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    setup_token: &str,
    id: &str,
) -> TestResult {
    let reader = proposal_identity(db, "setup_reader", &["ledger/read"])?;
    let reader = reader["token"].as_str().ok_or("missing reader token")?;
    let audit = setup_audit_count(db)?;
    for (token, expected) in [
        (None, "UNAUTHORIZED"),
        (Some(reader), "FORBIDDEN"),
        (Some(setup_token), "FORBIDDEN"),
    ] {
        let result = opening_approval_command(path, passphrase, token, id)?.output()?;
        assert!(!result.status.success());
        let failure: Value = serde_json::from_slice(&result.stderr)?;
        assert_eq!(failure["error"]["code"], expected);
    }
    let rejected = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--agent-token",
            reader,
            "--company",
            "home",
            "--json",
            "txn",
            "reject",
            id,
        ])
        .output()?;
    assert!(!rejected.status.success());
    let failure: Value = serde_json::from_slice(&rejected.stderr)?;
    assert_eq!(failure["error"]["code"], "FORBIDDEN");
    assert_eq!(
        beankeeper_cli::db::proposals::get(db.read(), &"home".parse()?, &id.parse()?)?
            .state
            .code(),
        "pending"
    );
    assert_eq!(setup_audit_count(db)?, audit);
    Ok(())
}

fn assert_opening_requires_review_after_finish(
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    id: &str,
    reviewer: &str,
) -> TestResult {
    let result = opening_approval_command(path, passphrase, None, id)?.output()?;
    assert!(!result.status.success());
    let failure: Value = serde_json::from_slice(&result.stderr)?;
    assert_eq!(failure["error"]["code"], "UNAUTHORIZED");
    opening_approval_command(path, passphrase, Some(reviewer), id)?
        .assert()
        .success();
    assert_eq!(
        beankeeper_cli::db::proposals::get(db.read(), &"home".parse()?, &id.parse()?)?
            .state
            .code(),
        "approved"
    );
    beankeeper_cli::db::audit::verify_hash_chain(db.read())?;
    Ok(())
}

fn opening_approval_command(
    path: &Path,
    passphrase: &Path,
    token: Option<&str>,
    id: &str,
) -> Result<Command, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.arg("--passphrase-file").arg(passphrase);
    if let Some(token) = token {
        command.args(["--agent-token", token]);
    }
    command.args(["--company", "home", "--json", "txn", "approve", id]);
    Ok(command)
}

fn assert_cli_replays_opening_reservation(
    path: &Path,
    passphrase: &Path,
    token: &str,
    generation: i64,
    fingerprint: &str,
    expected: &Value,
) -> TestResult {
    let cli = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--agent-token",
            token,
            "--json",
            "setup",
            "opening-reserve",
            "home",
        ])
        .args([
            "--generation",
            &generation.to_string(),
            "--date",
            "2026-01-02",
            "--currency",
            "BHD",
        ])
        .args([
            "--reference",
            "opening-home-2026",
            "--fingerprint",
            fingerprint,
        ])
        .args([
            "--debit",
            "1010:12.345:Opening asset",
            "--credit",
            "3000:12.345",
        ])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report = serde_json::from_slice::<Value>(&cli.stdout)?;
    assert_eq!(report["meta"]["command"], "ledger.setup.opening.reserve");
    assert_eq!(&report["data"], expected);
    Ok(())
}

fn assert_owner_approves_opening(
    db: &beankeeper_cli::db::Db,
    path: &Path,
    passphrase: &Path,
    owner_token: &str,
    proposal_id: &str,
) -> TestResult {
    let approved =
        opening_approval_command(path, passphrase, Some(owner_token), proposal_id)?.output()?;
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let approved = serde_json::from_slice::<Value>(&approved.stdout)?;
    let id = approved["data"]["transaction_id"]
        .as_i64()
        .ok_or("missing posted transaction ID")?;
    let (txn, entries) = beankeeper_cli::db::get_transaction(
        &beankeeper_cli::db::companies::Company::bind(db.read(), "home".parse()?)?,
        id.try_into()?,
    )?;
    assert_eq!(txn.currency.code(), "BHD");
    assert_eq!(txn.date.to_string(), "2026-01-02");
    assert_eq!(txn.reference.as_deref(), Some("opening-home-2026"));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].account_code, "1010");
    assert_eq!(entries[0].amount.minor_units(), 12_345);
    assert_eq!(entries[0].memo.as_deref(), Some("Opening asset"));
    assert_eq!(entries[1].account_code, "3000");
    assert_eq!(entries[1].amount.minor_units(), 12_345);
    assert_eq!(
        beankeeper_cli::db::proposals::get(db.read(), &"home".parse()?, &proposal_id.parse()?)?
            .state
            .code(),
        "approved"
    );
    Ok(())
}

#[test]
fn clearing_signed_statement_amounts_matches_cli_and_preserves_unsigned_magnitudes() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let mut withdrawal = posting("RTT-BANK-2026-11-07-SHELL");
    withdrawal["description"] = json!("Fuel");
    withdrawal["debits"] = json!([{"account":"4000","amount":"95.40"}]);
    withdrawal["credits"] = json!([{"account":"1000","amount":"95.40"}]);
    client.success("txn_post", &withdrawal)?;
    client.success("txn_post", &posting("deposit"))?;

    let result = client.success(
        "txn_clear",
        &json!({
            "company":"acme", "account":"1000", "date":"2026-11-07",
            "batch":[
                {"reference": "RTT-BANK-2026-11-07-SHELL", "amount":"-95.40"},
                {"reference": "deposit", "amount":"+2.50"}
            ]
        }),
    )?;
    assert_eq!(result["cleared_total"], 9790);
    assert_eq!(result["cleared"][0]["amount"], 9540);
    assert_eq!(result["cleared"][1]["amount"], 250);

    let mismatch = client.call(
        "txn_clear",
        &json!({
            "company":"acme", "account":"1000", "date":"2026-11-07",
            "reference":"deposit", "amount":"-1.00"
        }),
    )?;
    let error = payload(&mismatch)?;
    assert_eq!(error["code"], "VALIDATION");
    let message = error["message"].as_str().ok_or("missing validation text")?;
    assert!(
        message.contains("withdrawal") && message.contains("selected debit entry"),
        "{error}"
    );

    withdrawal["reference"] = json!("cli-withdrawal");
    client.success("txn_post", &withdrawal)?;
    let cli = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "txn",
            "clear",
            "--reference",
            "cli-withdrawal",
            "--account",
            "1000",
            "--date",
            "2026-11-07",
            "--amount",
            "-95.40",
        ])
        .assert()
        .success();
    let data: Value = serde_json::from_slice(&cli.get_output().stdout)?;
    assert_eq!(data["data"]["amount"], result["cleared"][0]["amount"]);

    withdrawal["reference"] = json!("unsigned-withdrawal");
    client.success("txn_post", &withdrawal)?;
    let unsigned = client.success(
        "txn_clear",
        &json!({
            "company":"acme", "account":"1000", "date":"2026-11-07",
            "reference":"unsigned-withdrawal", "amount":"95.40"
        }),
    )?;
    assert_eq!(unsigned["amount"], 9540);
    Ok(())
}

#[test]
fn report_discovery_and_as_of_match_the_cli() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    client.success("txn_post", &posting("as-of"))?;

    let listed = client.request("tools/list", &json!({}))?;
    let report = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "report")
        .ok_or("missing report")?;
    let description = report["description"]
        .as_str()
        .ok_or("missing description")?;
    for kind in cli_report_kinds()? {
        assert!(description.contains(&kind), "description omits {kind}");
    }
    let schema = &report["inputSchema"];
    let variants = schema["oneOf"]
        .as_array()
        .ok_or("missing report variants")?;

    for kind in [
        "trial-balance",
        "balance",
        "balance-sheet",
        "reconciliation",
    ] {
        let variant = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == kind)
            .ok_or_else(|| format!("missing schema for {kind}: {schema}"))?;
        let properties = &variant["properties"];
        assert!(properties.get("as_of").is_some(), "{kind}");
        assert_eq!(properties["as_of"], properties["to"], "{kind}");
        assert_eq!(variant["not"]["required"], json!(["to", "as_of"]), "{kind}");

        let mut args = json!({"company":"acme","kind":kind,"as_of":"2026-03-01"});
        let mut command = bk(&path)?;
        command.args([
            "--json",
            "--company",
            "acme",
            "report",
            kind,
            "--as-of",
            "2026-03-01",
        ]);
        if matches!(kind, "balance" | "reconciliation") {
            args["account"] = json!("1000");
            command.args(["--account", "1000"]);
        }
        let output = command.assert().success();
        let cli: Value = serde_json::from_slice(&output.get_output().stdout)?;
        assert_eq!(client.success("report", &args)?, cli["data"], "{kind}");

        args["to"] = json!("2026-03-01");
        let both = client.call("report", &args)?;
        assert_parameter_rejection(&both, "duplicate field `to`");
        let both_flags = command.args(["--to", "2026-03-01"]).output()?;
        assert_eq!(both_flags.status.code(), Some(2), "{kind}");
        let error = String::from_utf8(both_flags.stderr)?;
        assert!(error.contains("cannot be used multiple times"), "{error}");

        args.as_object_mut()
            .ok_or("missing report arguments")?
            .remove("as_of");
        assert_eq!(client.success("report", &args)?, cli["data"], "{kind}");
    }

    let unsupported = client.call(
        "report",
        &json!({
            "company":"acme","kind":"income-statement","as_of":"2026-03-01"
        }),
    )?;
    assert_parameter_rejection(&unsupported, "unknown field `as_of`");
    assert_parameter_rejection(&unsupported, "`to`");
    Ok(())
}

#[test]
fn same_company_correlation_suggests_txn_link_without_posting() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let mut original = posting("original");
    original
        .as_object_mut()
        .ok_or("missing posting")?
        .remove("meta");
    let original = client.success("txn_post", &original)?;
    let mut correction = posting("correction");
    correction["correlate"] = original["id"].clone();
    let response = client.call("txn_post", &correction)?;
    let error = payload(&response)?;
    assert_eq!(error["code"], "VALIDATION");
    let message = error["message"].as_str().ok_or("missing validation text")?;
    for guidance in ["same company", "bk txn link", "txn_link"] {
        assert!(message.contains(guidance), "{error}");
    }
    let transactions = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(transactions.as_array().map(Vec::len), Some(1));
    assert_eq!(transactions[0]["id"], original["id"]);
    assert!(transactions[0]["metadata"].is_null());
    Ok(())
}

#[test]
fn audit_log_default_page_can_be_resumed() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let listed = client.request("tools/list", &json!({}))?;
    let audit = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "audit_log")
        .ok_or("missing audit_log")?;
    let properties = &audit["inputSchema"]["properties"];
    assert_eq!(properties["limit"]["default"], 50, "{audit}");
    assert_eq!(properties["limit"]["maximum"], 100, "{audit}");
    assert_eq!(properties["offset"]["minimum"], 0, "{audit}");

    for index in 0..51 {
        client.success("txn_post", &posting(&format!("audit-page-{index}")))?;
    }

    let mut filters = json!({"company":"acme","entity":"transaction","action":"create"});
    let first = client.call("audit_log", &filters)?;
    let first_data = payload(&first)?;
    let page = &first["result"]["_meta"]["pagination"];
    assert_eq!(
        page,
        &json!({"limit":50,"offset":0,"returned":50,"total":51,"has_more":true,"next_offset":50})
    );
    assert_eq!(first_data.as_array().map(Vec::len), Some(50));
    let cli_audit = bk(&path)?
        .args([
            "--json",
            "audit",
            "log",
            "--company",
            "acme",
            "--entity",
            "transaction",
            "--action",
            "create",
        ])
        .assert()
        .success();
    let cli_audit: Value = serde_json::from_slice(&cli_audit.get_output().stdout)?;
    assert_eq!(first_data, cli_audit["data"]);
    assert_eq!(*page, cli_audit["meta"]["pagination"]);
    filters["offset"] = page["next_offset"].clone();
    let second = client.call("audit_log", &filters)?;
    let second_data = payload(&second)?;
    assert_eq!(second_data.as_array().map(Vec::len), Some(1));
    assert!(second_data[0]["id"].as_i64() < first_data[49]["id"].as_i64());
    assert_eq!(second["result"]["_meta"]["pagination"]["has_more"], false);
    assert!(
        second["result"]["_meta"]["pagination"]
            .get("next_offset")
            .is_none()
    );

    for limit in [-1, 101] {
        filters["limit"] = json!(limit);
        let result = client.call("audit_log", &filters)?;
        assert_parameter_rejection(&result, "audit limit must be between 0 and 100");
    }
    Ok(())
}

#[test]
fn audit_log_rejects_negative_offset() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;

    let result = client.call("audit_log", &json!({"offset":-1}))?;
    assert_parameter_rejection(&result, "audit offset must be a nonnegative integer");
    Ok(())
}

fn transaction_cli_data(mut command: Command, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = command
        .args(["--json", "--company", "acme", "txn"])
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(envelope["ok"], true);
    Ok(envelope["data"].clone())
}

#[test]
fn transaction_commands_and_tools_return_the_same_data() -> TestResult {
    let directory = tempdir()?;
    let cli_db = directory.path().join("cli.db");
    let mcp_db = directory.path().join("mcp.db");
    seed(&cli_db)?;
    std::fs::copy(&cli_db, &mcp_db)?;

    let configured = |database: &Path| {
        let mut command = bk(database)?;
        command
            .env("BEANKEEPER_CURRENCY", "BHD")
            .args(["--actor", "parity-operator"]);
        Ok::<_, Box<dyn Error>>(command)
    };
    let mut server = configured(&mcp_db)?;
    server.args(["mcp", "serve"]);
    let mut client = Client::spawn(server)?;
    client.initialize()?;

    let cli_post = [
        "post",
        "--description",
        "MCP sale",
        "--date",
        "2026-03-01",
        "--debit",
        "1000:12.50:bank receipt",
        "--credit",
        "4000:12.50",
        "--reference",
        "parity-sale",
        "--on-conflict",
        "skip",
        "--meta",
        "invoice=one",
    ];
    let mut mcp_post = posting("parity-sale");
    mcp_post
        .as_object_mut()
        .ok_or("posting object")?
        .remove("currency");
    let posted = transaction_cli_data(configured(&cli_db)?, &cli_post)?;
    assert_eq!(posted, client.success("txn_post", &mcp_post)?);
    assert_eq!(
        transaction_cli_data(configured(&cli_db)?, &cli_post)?,
        client.success("txn_post", &mcp_post)?
    );
    let id = posted["id"].to_string();

    let batch_file = directory.path().join("batch.csv");
    // The cash line is a debit, so use the explicit deposit sign in both transports.
    std::fs::write(&batch_file, "parity-sale,+1.234\n")?;
    let batch_path = batch_file.to_str().ok_or("UTF-8 fixture path")?;
    let clear = transaction_cli_data(
        configured(&cli_db)?,
        &[
            "clear",
            "--from-file",
            batch_path,
            "--account",
            "1000",
            "--date",
            "2026-03-02",
            "--statement",
            "fixture",
        ],
    )?;
    assert_eq!(clear["cleared_total"], 1234);
    assert_eq!(
        clear,
        client.success(
            "txn_clear",
            &json!({
                "company":"acme", "target":"batch", "account":"1000",
                "date":"2026-03-02", "statement":"fixture",
                "batch":[{"reference":"parity-sale", "amount":"+1.234"}],
            })
        )?
    );

    assert_eq!(
        transaction_cli_data(configured(&cli_db)?, &["meta", "set", &id, "note=paid"])?,
        client.success(
            "txn_meta_set",
            &json!({
                "company":"acme", "id":posted["id"], "values":{"note":"paid"},
            })
        )?,
    );
    assert_eq!(
        transaction_cli_data(
            configured(&cli_db)?,
            &["meta", "unset", "--reference", "parity-sale", "note"]
        )?,
        client.success(
            "txn_meta_unset",
            &json!({
                "company":"acme", "reference":"parity-sale", "keys":["note"],
            })
        )?,
    );

    assert_transaction_reads_and_reverse(
        |arguments| transaction_cli_data(configured(&cli_db)?, arguments),
        &mut client,
        &posted["id"],
    )
}

fn assert_transaction_reads_and_reverse(
    cli: impl Fn(&[&str]) -> Result<Value, Box<dyn Error>>,
    client: &mut Client,
    posted_id: &Value,
) -> TestResult {
    let id = posted_id.to_string();
    let shown = cli(&["show", &id])?;
    assert_eq!(shown[0]["currency"], "BHD");
    assert_eq!(shown[0]["entries"][0]["amount_display"], "12.500");
    assert_eq!(shown[0]["entries"][0]["clearings"][0]["amount"], 1234);
    assert_eq!(
        shown,
        client.success("txn_show", &json!({"company":"acme", "id":posted_id}))?
    );

    assert_eq!(
        cli(&[
            "list",
            "--limit",
            "1",
            "--offset",
            "0",
            "--account",
            "1000",
            "--amount-gt",
            "12.000",
            "--currency",
            "BHD",
            "--meta",
            "invoice=one",
        ])?,
        client.success(
            "txn_list",
            &json!({
                "company":"acme", "limit":1, "offset":0, "account":"1000", "amount_gt":"12.000",
                "currency":"BHD", "meta":["invoice=one"],
            })
        )?,
    );
    assert_eq!(
        cli(&["list", "--count", "--reference", "parity-sale"])?,
        client.success(
            "txn_list",
            &json!({
                "company":"acme", "count":true, "reference":"parity-sale",
            })
        )?,
    );
    assert_eq!(
        cli(&[
            "reverse",
            &id,
            "--date",
            "2026-03-03",
            "--reference",
            "parity-undo",
        ])?,
        client.success(
            "txn_reverse",
            &json!({
                "company":"acme", "id":posted_id, "date":"2026-03-03", "reference":"parity-undo",
            })
        )?,
    );
    assert_eq!(
        cli(&["reconcile"])?,
        client.success("txn_reconcile", &json!({}))?,
    );
    Ok(())
}

// These values are generated independently when the two transports create or
// resolve proposals. Assert their presence and compare every other field.
fn stable_proposal(mut row: Value) -> Value {
    assert!(
        row["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("proposal_"))
    );
    assert!(row["created_at"].is_string());
    row["id"] = json!("proposal_fixture");
    row["created_at"] = json!("fixture-timestamp");
    if row["resolved_at"].is_string() {
        row["resolved_at"] = json!("fixture-timestamp");
    }
    row
}

#[test]
fn proposal_commands_and_tools_return_the_same_data() -> TestResult {
    let directory = tempdir()?;
    let cli_db = directory.path().join("cli.db");
    let mcp_db = directory.path().join("mcp.db");
    let passphrase_file = directory.path().join("passphrase");
    identity_ledger(&cli_db, &passphrase_file)?;
    let passphrase: secrecy::SecretString = "mcp-test-passphrase".into();
    let db = beankeeper_cli::db::Db::open_existing(&cli_db, Some(&passphrase))?;
    let proposer = proposal_identity(&db, "parity_proposer", &["ledger/read", "ledger/propose"])?;
    let reviewer = proposal_identity(&db, "parity_reviewer", &["ledger/read", "ledger/approve"])?;
    let proposer_token = proposer["token"].as_str().ok_or("proposer token")?;
    let reviewer_token = reviewer["token"].as_str().ok_or("reviewer token")?;
    drop(db);
    std::fs::copy(&cli_db, &mcp_db)?;
    let cli = |token: &str| {
        let mut command = bk(&cli_db)?;
        command
            .arg("--passphrase-file")
            .arg(&passphrase_file)
            .args(["--agent-token", token]);
        Ok::<_, Box<dyn Error>>(command)
    };
    let mut proposer = identity_client(&mcp_db, &passphrase_file, Some(proposer_token), false)?;
    let mut reviewer = identity_client(&mcp_db, &passphrase_file, Some(reviewer_token), false)?;

    for (reference, resolution, tool) in [
        ("parity-approved", "approve", "txn_approve"),
        ("parity-withdrawn", "reject", "txn_reject"),
    ] {
        let cli_proposal = transaction_cli_data(
            cli(proposer_token)?,
            &[
                "propose",
                "--description",
                "MCP sale",
                "--date",
                "2026-03-01",
                "--debit",
                "1000:12.50:bank receipt",
                "--credit",
                "4000:12.50",
                "--reference",
                reference,
                "--on-conflict",
                "skip",
                "--meta",
                "invoice=one",
            ],
        )?;
        let mcp_proposal = proposer.success("txn_propose", &posting(reference))?;
        let cli_id = cli_proposal["id"]
            .as_str()
            .ok_or("CLI proposal ID")?
            .to_owned();
        let mcp_id = mcp_proposal["id"]
            .as_str()
            .ok_or("MCP proposal ID")?
            .to_owned();
        assert_eq!(stable_proposal(cli_proposal), stable_proposal(mcp_proposal));

        let cli_list = transaction_cli_data(cli(proposer_token)?, &["proposals"])?;
        let mcp_list = proposer.success("txn_proposals", &json!({"company":"acme"}))?;
        assert_eq!(cli_list.as_array().map(Vec::len), Some(1));
        assert_eq!(mcp_list.as_array().map(Vec::len), Some(1));
        assert_eq!(
            stable_proposal(cli_list[0].clone()),
            stable_proposal(mcp_list[0].clone())
        );

        let (token, client) = if resolution == "approve" {
            (reviewer_token, &mut reviewer)
        } else {
            (proposer_token, &mut proposer)
        };
        let cli_resolved = transaction_cli_data(cli(token)?, &[resolution, &cli_id])?;
        let mcp_resolved = client.success(tool, &json!({"company":"acme", "id":mcp_id}))?;
        assert_eq!(stable_proposal(cli_resolved), stable_proposal(mcp_resolved));
    }
    Ok(())
}

fn report_parity_cases() -> [(&'static str, &'static [&'static str], Value); 17] {
    [
        ("trial-balance", &[], json!({})),
        (
            "trial-balance",
            &["--period", "month"],
            json!({"period": "month"}),
        ),
        (
            "trial-balance",
            &["--compare", "previous"],
            json!({"compare": "previous"}),
        ),
        (
            "balance",
            &["--account", "1000"],
            json!({"account": "1000"}),
        ),
        ("balance-sheet", &[], json!({})),
        ("income-statement", &[], json!({})),
        (
            "income-statement",
            &["--period", "quarter"],
            json!({"period": "quarter"}),
        ),
        (
            "income-statement",
            &["--compare", "previous"],
            json!({"compare": "previous"}),
        ),
        ("tax-summary", &[], json!({})),
        ("schedule-c", &[], json!({})),
        ("1099", &[], json!({})),
        (
            "ledger",
            &["--account", "1000", "--filter-metadata", "invoice=one"],
            json!({"account": ["1000"], "filter_metadata": ["invoice=one"]}),
        ),
        (
            "budget-variance",
            &["--year", "2026", "--month", "3"],
            json!({"year": 2026, "month": 3}),
        ),
        (
            "trend",
            &["--metric", "net-worth"],
            json!({"metric": "net-worth"}),
        ),
        ("cash-flow", &[], json!({})),
        (
            "aging",
            &["--account", "1000"],
            json!({"account": ["1000"]}),
        ),
        (
            "reconciliation",
            &["--account", "1000", "--statement-balance", "12.50"],
            json!({"account": "1000", "statement_balance": "12.50"}),
        ),
    ]
}

#[test]
fn report_queries_produce_the_same_data_over_cli_and_stdio() -> TestResult {
    let directory = tempdir()?;
    let books = directory.path().join("report-parity.db");
    seed(&books)?;
    let mut command = bk(&books)?;
    command.args(["mcp", "serve"]);
    let mut mcp = Client::spawn(command)?;
    mcp.initialize()?;
    mcp.success("txn_post", &posting("report-parity"))?;

    for (kind, flags, mut parameters) in report_parity_cases() {
        let mut expected = bk(&books)?;
        expected
            .args(["--json", "--company", "acme", "report", kind])
            .args(flags);
        parameters["company"] = json!("acme");
        parameters["kind"] = json!(kind);
        match kind {
            "budget-variance" => {}
            "aging" => {
                expected.args(["--as-of", "2026-03-31"]);
                parameters["as_of"] = json!("2026-03-31");
            }
            _ => {
                expected.args(["--to", "2026-03-31"]);
                parameters["to"] = json!("2026-03-31");
            }
        }
        let output = expected.assert().success();
        let cli: Value = serde_json::from_slice(&output.get_output().stdout)?;
        assert_eq!(
            mcp.success("report", &parameters)?,
            cli["data"],
            "{kind}: {flags:?}"
        );
    }

    let today = chrono::Local::now().date_naive().to_string();
    for (kind, args, end_key) in [
        ("cash-flow", json!({}), "to"),
        ("aging", json!({"account": ["1000"]}), "as_of"),
        ("reconciliation", json!({"account": "1000"}), "to"),
    ] {
        let mut args = args;
        args["company"] = json!("acme");
        args["kind"] = json!(kind);
        let with_default = mcp.success("report", &args)?;
        args[end_key] = json!(today);
        assert_eq!(with_default, mcp.success("report", &args)?, "{kind}");
    }

    let invalid = mcp.call(
        "report",
        &json!({"company":"acme","kind":"trial-balance","period":"month","compare":"previous"}),
    )?;
    assert_eq!(invalid["result"]["isError"], true);
    let error = payload(&invalid)?;
    assert_eq!(error["code"], "USAGE");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot be used with"))
    );
    Ok(())
}

#[test]
fn generated_operations_cover_cli_paths_mcp_schemas_and_permissions() -> TestResult {
    use beankeeper_cli::{
        capability::Capability,
        cli::Cli,
        operation::{CliOnlyReason, Exposure, Operation},
    };
    use clap::CommandFactory;

    let cli = Cli::command();
    let dir = tempdir()?;
    let path = dir.path().join("catalog.db");
    bk(&path)?.arg("init").assert().success();
    let mut server = bk(&path)?;
    server.args(["mcp", "serve"]);
    let mut client = Client::spawn(server)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    let mut expected = BTreeSet::new();

    for &operation in Operation::ALL {
        let path = operation.cli_path();
        assert!(
            path.split('.')
                .try_fold(&cli, |command, segment| command.find_subcommand(segment))
                .is_some(),
            "{operation:?} has no clap path {path}"
        );
        let permission = operation.primary_capability();
        match operation.exposure() {
            Exposure::Both(name) | Exposure::McpOnly(name, _) => {
                assert!(expected.insert(name), "duplicate registered tool {name}");
                let tool = tools.iter().find(|tool| tool["name"] == name).ok_or(name)?;
                assert_eq!(tool["inputSchema"]["type"], "object");
                assert_eq!(
                    tool["annotations"]["readOnlyHint"],
                    permission? == Capability::Read,
                    "{name} advertises the wrong capability effect"
                );
            }
            Exposure::Grouped(group) => {
                assert!(matches!(group.exposure(), Exposure::McpOnly(_, _)));
                assert_eq!(permission?, group.primary_capability()?);
            }
            Exposure::CliOnly(CliOnlyReason::Transport) => assert!(permission.is_err()),
            Exposure::CliOnly(_) => {
                permission?;
            }
        }
    }
    assert_eq!(tool_names(&listed)?, expected);
    Ok(())
}

mod catalog_payload_parity {
    use super::*;

    fn cli(path: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let output = bk(path)?
            .env("BEANKEEPER_CURRENCY", "BHD")
            .arg("--json")
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        Ok(serde_json::from_slice::<Value>(&output)?["data"].clone())
    }

    #[test]
    fn categorization_rules_share_cli_and_mcp_state_and_results() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("rules.db");
        let mut client = standing_client(&path)?;

        let run = |args: &[&str]| cli(&path, &[&["--company", "acme", "rule"][..], args].concat());
        let created = run(&["create", "--contains", " CAFÉ ", "--account", "4000"])?;
        let id = created[0]["id"].as_str().ok_or("missing rule ID")?;
        assert_eq!(created[0]["priority"], 0);
        assert_eq!(created[0]["contains"], "café");
        assert_eq!(
            client.success("rule_list", &json!({"company":"acme"}))?,
            created
        );

        let candidate = json!({"company":"acme", "description":"CAFÉ vendor"});
        let matched = client.success("rule_match", &candidate)?;
        assert_eq!(matched, run(&["match", "--description", "CAFÉ vendor"])?);
        assert_eq!(matched, json!({"outcome":"match", "rule": created[0]}));
        assert_eq!(
            client.success(
                "rule_match",
                &json!({"company":"other", "description":"CAFÉ vendor"})
            )?,
            json!({"outcome":"no_match"})
        );

        let replacement = json!({"company":"acme", "id":id, "contains":"coffee", "account":"1000", "priority":-4});
        let wrong_company =
            json!({"company":"other", "id":id, "contains":"coffee", "account":"1000"});
        let rejected = client.call("rule_amend", &wrong_company)?;
        assert_eq!(payload(&rejected)?["code"], "NOT_FOUND");
        let amended = client.success("rule_amend", &replacement)?;
        let new_id = amended[0]["id"]
            .as_str()
            .ok_or("missing replacement rule ID")?;
        assert_ne!(id, new_id);
        assert_eq!(amended[0]["supersedes"], id);
        assert_eq!(amended, run(&["list"])?);
        assert_eq!(
            client.success("rule_match", &candidate)?,
            json!({"outcome":"no_match"})
        );
        assert_eq!(
            payload(&client.call("rule_amend", &replacement)?)?["code"],
            "NOT_FOUND"
        );

        let restored = run(&["amend", new_id, "--contains", "café", "--account", "4000"])?;
        let restored_id = restored[0]["id"]
            .as_str()
            .ok_or("missing restored rule ID")?;
        assert_eq!(restored[0]["supersedes"], new_id);
        assert_eq!(
            client.success("rule_match", &candidate)?["rule"],
            restored[0]
        );

        assert_eq!(
            client.success("rule_delete", &json!({"company":"acme", "id":restored_id}))?,
            json!({"deleted":restored_id})
        );

        let history = client.success(
            "audit_log",
            &json!({"company":"acme","action":"rule_supersede"}),
        )?;
        let change = &history[0];
        assert_eq!(decode_snapshot(&change["before"])?, amended[0]);
        assert_eq!(decode_snapshot(&change["after"])?, restored[0]);
        assert_eq!(decode_snapshot(&history[1]["before"])?, created[0]);
        assert_eq!(decode_snapshot(&history[1]["after"])?, amended[0]);
        client.success("verify", &json!({}))?;
        Ok(())
    }

    #[test]
    fn standing_settings_share_typed_values_and_audited_amendments() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("settings.db");
        let mut client = standing_client(&path)?;

        let company = json!({"company":"acme"});
        let run = |args: &[&str]| cli(&path, &[&["--company", "acme"][..], args].concat());
        let default_policies = json!({"sales_tax":"unset", "processor_fees":"unset"});
        assert_eq!(
            client.success("bookkeeping_policy_show", &company)?,
            default_policies
        );
        assert_eq!(run(&["bookkeeping-policy", "show"])?, default_policies);
        assert_eq!(
            client.success("inform_preference_show", &company)?,
            json!({"preference":"per-change"})
        );

        let tax = run(&["bookkeeping-policy", "set", "sales-tax=track-liability"])?;
        assert_eq!(
            tax,
            json!({"sales_tax":"track-liability", "processor_fees":"unset"})
        );
        let fees = json!({"company":"acme", "policy":{"kind":"processor-fees","value":"separate-expense"}});
        let policies = client.success("bookkeeping_policy_set", &fees)?;
        assert_eq!(
            policies,
            json!({"sales_tax":"track-liability","processor_fees":"separate-expense"})
        );
        assert_eq!(run(&["bookkeeping-policy", "show"])?, policies);
        assert_eq!(client.success("bookkeeping_policy_set", &fees)?, policies);
        let cleared = client.success(
            "bookkeeping_policy_set",
            &json!({"company":"acme", "policy":{"kind":"sales-tax","value":"unset"}}),
        )?;
        assert_eq!(
            cleared,
            json!({"sales_tax":"unset","processor_fees":"separate-expense"})
        );

        let inform = client.success(
            "inform_preference_set",
            &json!({"company":"acme","preference":"daily-digest"}),
        )?;
        assert_eq!(inform, run(&["inform-preference", "show"])?);
        assert_eq!(
            run(&["inform-preference", "set", "weekly-digest"])?,
            json!({"preference":"weekly-digest"})
        );
        assert_eq!(
            client.success("inform_preference_show", &company)?,
            json!({"preference":"weekly-digest"})
        );
        let other = json!({"company":"other"});
        assert_eq!(
            client.success("bookkeeping_policy_show", &other)?,
            default_policies
        );
        assert_eq!(
            client.success("inform_preference_show", &other)?,
            json!({"preference":"per-change"})
        );

        let audit = client.success(
            "audit_log",
            &json!({"company":"acme","entity":"bookkeeping_policy"}),
        )?;
        assert_eq!(audit.as_array().map(Vec::len), Some(3));
        assert_eq!(
            decode_snapshot(&audit[0]["before"])?,
            json!({"kind":"sales-tax","value":"track-liability"})
        );
        assert_eq!(
            decode_snapshot(&audit[0]["after"])?,
            json!({"kind":"sales-tax","value":"unset"})
        );
        assert_eq!(audit[0]["hash_version"], 7);
        let audit = client.success(
            "audit_log",
            &json!({"company":"acme","entity":"inform_preference"}),
        )?;
        assert_eq!(audit.as_array().map(Vec::len), Some(2));
        assert_eq!(decode_snapshot(&audit[0]["before"])?, "daily-digest");
        assert_eq!(decode_snapshot(&audit[0]["after"])?, "weekly-digest");
        assert_eq!(decode_snapshot(&audit[1]["before"])?, "per-change");
        client.success("verify", &json!({}))?;
        Ok(())
    }

    #[test]
    fn rule_and_standing_inputs_are_validated_at_the_mcp_boundary() -> TestResult {
        let dir = tempdir()?;
        let path = dir.path().join("settings.db");
        let mut client = standing_client(&path)?;

        for inputs in [
            json!({"company":"acme", "contains":" ", "account":"4000"}),
            json!({"company":"acme", "contains":"coffee", "account":"bad"}),
            json!({"company":"acme", "contains":"coffee", "account":"4000", "priority": 2_147_483_648_i64}),
        ] {
            let response = client.call("rule_create", &inputs)?;
            assert_eq!(payload(&response)?["code"], "VALIDATION");
        }
        let other = client.success(
            "rule_create",
            &json!({"company":"acme", "contains":"tea", "account":"4000"}),
        )?;
        assert_eq!(other[0]["priority"], 0);

        let invalid = client.call(
            "bookkeeping_policy_set",
            &json!({"company":"acme", "policy":{"kind":"sales-tax","value":"separate-expense"}}),
        )?;
        assert_eq!(payload(&invalid)?["code"], "VALIDATION");
        let invalid = client.call(
            "inform_preference_set",
            &json!({"company":"acme","preference":"never"}),
        )?;
        assert_eq!(payload(&invalid)?["code"], "VALIDATION");

        Ok(())
    }

    fn standing_client(path: &Path) -> Result<Client, Box<dyn Error>> {
        seed(path)?;
        bk(path)?
            .args(["company", "create", "other", "Other"])
            .assert()
            .success();
        let mut command = bk(path)?;
        command.args(["mcp", "serve"]);
        let mut client = Client::spawn(command)?;
        client.initialize()?;
        Ok(client)
    }

    fn decode_snapshot(value: &Value) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_str(
            value.as_str().ok_or("missing audit snapshot")?,
        )?)
    }

    fn without_row_timestamps(mut rows: Value) -> Value {
        if let Some(rows) = rows.as_array_mut() {
            for row in rows {
                if let Some(fields) = row.as_object_mut() {
                    fields.remove("created_at");
                }
            }
        }
        rows
    }

    type Fixture = (&'static str, Value, &'static [&'static str]);

    fn fixtures() -> [Fixture; 10] {
        [
            ("company_list", json!({}), &["company", "list"]),
            (
                "company_show",
                json!({"slug":"acme"}),
                &["company", "show", "acme"],
            ),
            (
                "company_create",
                json!({"slug":"empty", "name":" Empty "}),
                &["company", "create", "empty", " Empty "],
            ),
            (
                "company_delete",
                json!({"slug":"empty"}),
                &["company", "delete", "empty", "--force"],
            ),
            (
                "account_create",
                json!({"company":"acme", "code":"5000", "name":"Rent", "account_type":"expense"}),
                &[
                    "--company",
                    "acme",
                    "account",
                    "create",
                    "5000",
                    "Rent",
                    "--type",
                    "expense",
                ],
            ),
            (
                "account_list",
                json!({"company":"acme"}),
                &["--company", "acme", "account", "list"],
            ),
            (
                "account_show",
                json!({"company":"acme", "code":"5000"}),
                &["--company", "acme", "account", "show", "5000"],
            ),
            (
                "budget_set",
                json!({"company":"acme", "account":"5000", "year":2026, "month":3, "amount":"12.345"}),
                &[
                    "--company",
                    "acme",
                    "budget",
                    "set",
                    "5000",
                    "--year",
                    "2026",
                    "--month",
                    "3",
                    "--amount",
                    "12.345",
                ],
            ),
            (
                "budget_list",
                json!({"company":"acme", "year":2026}),
                &["--company", "acme", "budget", "list", "--year", "2026"],
            ),
            (
                "budget_delete",
                json!({"company":"acme", "account":"5000", "year":2026, "month":3}),
                &[
                    "--company",
                    "acme",
                    "budget",
                    "delete",
                    "5000",
                    "--year",
                    "2026",
                    "--month",
                    "3",
                    "--force",
                ],
            ),
        ]
    }

    #[test]
    fn companies_accounts_budgets_and_verification_share_payloads() -> TestResult {
        let dir = tempdir()?;
        let cli_db = dir.path().join("cli.db");
        let mcp_db = dir.path().join("mcp.db");
        seed(&cli_db)?;
        std::fs::copy(&cli_db, &mcp_db)?;

        let mut server = bk(&mcp_db)?;
        server
            .env("BEANKEEPER_CURRENCY", "BHD")
            .args(["mcp", "serve"]);
        let mut client = Client::spawn(server)?;
        client.initialize()?;

        for (tool, input, args) in fixtures() {
            let local = cli(&cli_db, args)?;
            if tool == "budget_set" {
                assert_eq!(local[0]["currency"], "BHD");
                assert_eq!(local[0]["amount_display"], "12.345");
            }
            // Mirrored writes can run on opposite sides of a clock tick.
            assert_eq!(
                without_row_timestamps(client.success(tool, &input)?),
                without_row_timestamps(local),
                "{tool}"
            );
        }
        // Verification evidence describes one ledger's exact audit hashes.
        assert_eq!(
            client.success("verify", &json!({}))?,
            cli(&mcp_db, &["verify"])?
        );
        Ok(())
    }
}

#[path = "support/mcp_setup_investment_parity.rs"]
mod setup_investment_parity;
