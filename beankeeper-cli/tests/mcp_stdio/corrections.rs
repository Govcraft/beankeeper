//! Signed bookkeepers correct their work through the CLI and MCP.

use std::{
    error::Error,
    path::PathBuf,
    process::{Command, Output},
};

use beankeeper_cli::{
    db::{self, Actor, Db},
    identity::{Admin, Granted},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

use super::{
    Client, LEDGER_TOOLS, TestResult, bk, identity_client, identity_ledger, payload, posting,
    proposal_identity, tool_names,
};

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    passphrase: PathBuf,
    db: Db,
    owner: Value,
}

impl Fixture {
    fn new(enforce: bool) -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("corrections.db");
        let passphrase = dir.path().join("passphrase");
        identity_ledger(&path, &passphrase)?;
        let secret: secrecy::SecretString = "mcp-test-passphrase".into();
        let db = Db::open_existing(&path, Some(&secret))?;
        let owner = proposal_identity(
            &db,
            "correction_owner",
            &[
                "ledger/read",
                "ledger/admin",
                "ledger/approve",
                "ledger/reconcile",
            ],
        )?;
        if enforce {
            Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                db::audit::set_policy(grant, true)
            })?;
        }
        Ok(Self {
            _dir: dir,
            path,
            passphrase,
            db,
            owner,
        })
    }

    fn command(&self, credential: &Value) -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&self.path)?;
        command
            .arg("--passphrase-file")
            .arg(&self.passphrase)
            .env(
                "BK_AGENT_TOKEN",
                credential["token"].as_str().ok_or("missing credential")?,
            )
            .args(["--company", "acme", "--json"]);
        Ok(command)
    }

    fn assert_admin_restricted(
        &self,
        credential: &Value,
        tools: &std::collections::BTreeSet<&str>,
    ) -> TestResult {
        for admin in [
            "account_create",
            "company_create",
            "period_reopen",
            "ledger_setup_begin",
        ] {
            assert!(!tools.contains(admin), "{admin}");
        }
        require_exit(
            self.command(credential)?
                .args(["account", "create", "5555", "Other", "--type", "asset"]),
            7,
        )?;
        require_exit(
            self.command(credential)?
                .args(["backup", "--output"])
                .arg(self.path.with_extension("backup")),
            7,
        )?;
        Ok(())
    }

    fn client(&self, credential: &Value) -> Result<Client, Box<dyn Error>> {
        identity_client(
            &self.path,
            &self.passphrase,
            credential["token"].as_str(),
            false,
        )
    }

    fn issue(&self, name: &str, grants: &str, arguments: &[&str]) -> Result<Value, Box<dyn Error>> {
        let issue = require_exit(
            self.command(&self.owner)?
                .args([
                    "agent",
                    "issue",
                    "--name",
                    name,
                    "--generate-key",
                    "--caps",
                    grants,
                ])
                .args(arguments),
            0,
        )?;
        let wire: Value = serde_json::from_slice(&issue.stdout)?;
        Ok(wire["data"].clone())
    }

    fn proposer(&self) -> Result<Value, Box<dyn Error>> {
        self.issue(
            "correction_proposer",
            "ledger/read,ledger/post,ledger/propose",
            &["--write-mode", "propose-only"],
        )
    }
}

fn require_exit(command: &mut Command, code: i32) -> Result<Output, Box<dyn Error>> {
    let output = command.output()?;
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn cli_data(command: &mut Command) -> Result<Value, Box<dyn Error>> {
    let output = require_exit(command, 0)?;
    let wire: Value = serde_json::from_slice(&output.stdout)?;
    Ok(wire["data"].clone())
}

fn receipt(client: &mut Client, reference: &str) -> Result<Value, Box<dyn Error>> {
    let receipts = client.success("audit_receipts", &json!({"company":"acme"}))?;
    let entry = receipts["receipts"]
        .as_array()
        .ok_or("missing receipts")?
        .iter()
        .find(|row| row["change"]["source_reference"] == reference)
        .ok_or("missing posting receipt")?;
    Ok(entry["id"].clone())
}

#[test]
fn legacy_direct_bookkeepers_correct_without_admin() -> TestResult {
    for enforce in [false, true] {
        let fixture = Fixture::new(enforce)?;
        for grants in [
            &["ledger/read", "ledger/post"][..],
            &["ledger/read", "ledger/post", "ledger/propose"][..],
        ] {
            let label = if grants.len() == 2 {
                "bookkeeper_short"
            } else {
                "bookkeeper_managed"
            };
            let credential = fixture.issue(label, &grants.join(","), &[])?;
            let mut agent = fixture.client(&credential)?;
            let catalog = agent.request("tools/list", &json!({}))?;
            let names = tool_names(&catalog)?;
            assert!(names.contains("txn_reverse"));
            assert!(names.contains("audit_undo"));
            fixture.assert_admin_restricted(&credential, &names)?;

            let posted = agent.success("txn_post", &posting(label))?;
            let original = receipt(&mut agent, label)?;
            let args = json!({"company":"acme","receipt":original});
            let outcome = agent.success("audit_undo", &args)?;
            assert_eq!(outcome["undo_of"], original);
            assert_eq!(outcome["transaction"], posted["id"]);
            assert_eq!(outcome["kind"], "reversed");
            assert!(outcome["desktop_path"].is_string());
            assert_eq!(
                cli_data(fixture.command(&credential)?.args([
                    "audit",
                    "undo",
                    &original.to_string()
                ]))?,
                outcome,
            );
            let blocked = agent.call(
                "audit_undo",
                &json!({"company":"acme","receipt":outcome["undo_receipt"]}),
            )?;
            assert_eq!(payload(&blocked)?["code"], "UNDO_INELIGIBLE");
            assert!(
                payload(&blocked)?["message"]
                    .as_str()
                    .ok_or("missing message")?
                    .contains("already records an undo")
            );
            let account_receipts = agent.success("audit_receipts", &json!({"company":"acme"}))?;
            let cash = account_receipts["receipts"]
                .as_array()
                .ok_or("missing receipts")?
                .iter()
                .find(|row| row["change"]["target"]["id"] == "1000")
                .ok_or("missing creation")?;
            let denied = agent.call(
                "audit_undo",
                &json!({"company":"acme","receipt":cash["id"]}),
            )?;
            assert_eq!(payload(&denied)?["code"], "UNDO_OWNER_ONLY");

            let posted = agent.success("txn_post", &posting(&format!("{label}-reverse")))?;
            let reversal_reference = format!("{label}-correction");
            let first = cli_data(fixture.command(&credential)?.args([
                "txn",
                "reverse",
                &posted["id"].to_string(),
                "--date",
                "2026-03-02",
                "--reference",
                &reversal_reference,
            ]))?;
            let retry = agent.success(
                "txn_reverse",
                &json!({"company":"acme","id":posted["id"],"reference":reversal_reference}),
            )?;
            assert_eq!(retry["existing_id"], first["id"]);
            assert_eq!(retry["receipt"], first["receipt"]);
            assert!(first["desktop_path"].is_string());
            let blocked = agent.call("txn_reverse", &json!({"company":"acme","id":first["id"]}))?;
            assert!(
                payload(&blocked)?["message"]
                    .as_str()
                    .ok_or("missing message")?
                    .contains("itself a reversal")
            );
            agent.success("verify", &json!({}))?;
        }
    }
    Ok(())
}

#[test]
fn propose_only_corrections_are_pending_until_a_signed_approver_applies_them() -> TestResult {
    let fixture = Fixture::new(false)?;
    let proposer = fixture.proposer()?;
    let mut agent = fixture.client(&proposer)?;
    let mut owner = fixture.client(&fixture.owner)?;
    let listed = agent.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    for tool in ["txn_reverse", "audit_undo"] {
        assert!(names.contains(tool));
    }
    assert!(!names.contains("txn_approve"));

    let first = owner.success("txn_post", &posting("reverse-proposal"))?;
    let second = owner.success("txn_post", &posting("undo-proposal"))?;
    let original = receipt(&mut owner, "undo-proposal")?;
    let reverse_args = json!({"company":"acme","id":first["id"],"reference":"reviewed-reversal"});
    let undo_args = json!({"company":"acme","receipt":original});
    let reverse = agent.success("txn_reverse", &reverse_args)?;
    let undo = agent.success("audit_undo", &undo_args)?;
    assert_eq!(reverse["status"], "pending");
    assert_eq!(undo["status"], "pending");
    assert_eq!(undo["undo_of"], original);
    assert_eq!(
        cli_data(
            fixture
                .command(&proposer)?
                .args(["audit", "undo", &original.to_string()])
        )?,
        undo
    );
    assert_eq!(agent.success("txn_reverse", &reverse_args)?, reverse);
    let company = json!({"company":"acme"});
    assert_eq!(
        owner
            .success("txn_list", &company)?
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(
        owner
            .success("txn_proposals", &company)?
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    for pending in [&reverse, &undo] {
        let approval = json!({"company":"acme","id":pending["proposal_id"]});
        require_exit(
            bk(&fixture.path)?
                .arg("--passphrase-file")
                .arg(&fixture.passphrase)
                .args([
                    "--company",
                    "acme",
                    "txn",
                    "approve",
                    pending["proposal_id"].as_str().ok_or("proposal ID")?,
                ]),
            6,
        )?;
        let approved = owner.success("txn_approve", &approval)?;
        assert_eq!(approved["status"], "approved");
        assert_eq!(approved, owner.success("txn_approve", &approval)?);
    }
    assert_eq!(owner.success("txn_list", &company)?, json!([]));
    assert_eq!(
        owner
            .success(
                "txn_list",
                &json!({"company":"acme", "include_reversed":true})
            )?
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    let retried = agent.success("audit_undo", &undo_args)?;
    assert_eq!(retried["transaction"], second["id"]);
    assert_eq!(retried["undo_of"], original);
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn correction_hints_reference_only_discovered_tools() -> TestResult {
    let fixture = Fixture::new(false)?;
    let reader = proposal_identity(&fixture.db, "reader", &["ledger/read"])?;
    let direct = proposal_identity(
        &fixture.db,
        "direct",
        &["ledger/read", "ledger/post", "ledger/propose"],
    )?;
    let proposer = fixture.proposer()?;
    Granted::<Admin>::with_db(&fixture.db, &Actor::new("fixture".parse()?), |grant| {
        db::audit::set_policy(grant, true)
    })?;
    let all = LEDGER_TOOLS.iter().copied().collect();
    for credential in [&reader, &direct, &proposer] {
        let mut client = Client::spawn({
            let mut command = fixture.command(credential)?;
            command.args(["mcp", "serve"]);
            command
        })?;
        let initialized = client.request(
            "initialize",
            &json!({
                "protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"Correction catalog test","version":"1.0"}
            }),
        )?;
        client.notify("notifications/initialized", &json!({}))?;
        let listed = client.request("tools/list", &json!({}))?;
        let names = tool_names(&listed)?;
        let instructions = initialized["result"]["instructions"]
            .as_str()
            .ok_or("missing instructions")?;
        check_mentions(instructions, &all, &names);
        for tool in listed["result"]["tools"]
            .as_array()
            .ok_or("missing tools")?
        {
            check_mentions(
                tool["description"].as_str().ok_or("missing description")?,
                &all,
                &names,
            );
        }
        if names.contains("txn_post") && names.contains("txn_reverse") {
            let posted = client.success("txn_post", &posting("hint-source"))?;
            if let Some(id) = posted["id"].as_i64() {
                let mut request = posting("hint-correlation");
                request["correlate"] = json!(id);
                let error = payload(&client.call("txn_post", &request)?)?;
                assert_eq!(error["code"], "VALIDATION");
                check_mentions(
                    error["message"].as_str().ok_or("missing hint")?,
                    &all,
                    &names,
                );
            }
        }
    }
    Ok(())
}

fn check_mentions(
    text: &str,
    all: &std::collections::BTreeSet<&str>,
    visible: &std::collections::BTreeSet<&str>,
) {
    for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        assert!(
            !all.contains(word) || visible.contains(word),
            "{word} is hidden: {text}"
        );
    }
}

#[test]
fn agent_undo_reports_closed_and_reconciled_postings_without_changing_them() -> TestResult {
    let fixture = Fixture::new(false)?;
    let credential = proposal_identity(&fixture.db, "corrector", &["ledger/read", "ledger/post"])?;
    let mut agent = fixture.client(&credential)?;
    let mut owner = fixture.client(&fixture.owner)?;
    let company = json!({"company":"acme"});
    let original = agent.success("txn_post", &posting("closed-correction"))?;
    let id = receipt(&mut agent, "closed-correction")?;
    let undo = json!({"company":"acme","receipt":id});

    owner.success(
        "period_close",
        &json!({"company":"acme","through":"2099-12-31"}),
    )?;
    let locked = payload(&agent.call("audit_undo", &undo)?)?;
    assert_eq!(locked["code"], "UNDO_INELIGIBLE");
    let message = locked["message"].as_str().ok_or("missing message")?;
    assert!(message.contains("2099-12-31"));
    assert!(message.contains("reopen the period"));
    assert!(!message.contains("period_reopen"));
    owner.success(
        "period_reopen",
        &json!({"company":"acme","reason":"Test reconciliation"}),
    )?;

    let shown = agent.success("txn_show", &json!({"company":"acme","id":original["id"]}))?;
    owner.success(
        "txn_clear",
        &json!({
            "company":"acme","target":"entry","transaction_id":original["id"],
            "entry":shown[0]["entries"][0]["id"],"date":"2026-03-02","status":"reconciled"
        }),
    )?;
    let reconciled = payload(&agent.call("audit_undo", &undo)?)?;
    assert_eq!(reconciled["code"], "UNDO_INELIGIBLE");
    assert!(
        reconciled["message"]
            .as_str()
            .ok_or("missing message")?
            .contains("reconciled clearings")
    );
    assert_eq!(
        agent
            .success("txn_list", &company)?
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

fn create_recategory(owner: &mut Client) -> TestResult {
    owner.success(
        "account_create",
        &json!({"company":"acme","code":"4100","name":"Services","account_type":"revenue"}),
    )?;
    Ok(())
}

fn assert_recategorization_receipt(
    agent: &mut Client,
    original: &Value,
    result: &Value,
) -> TestResult {
    let receipts = agent.success("audit_receipts", &json!({"company":"acme"}))?;
    let receipts = receipts["receipts"].as_array().ok_or("missing receipts")?;
    let change = receipts
        .iter()
        .find(|receipt| receipt["id"] == result["receipt_id"])
        .ok_or("missing recategorization receipt")?;
    assert_eq!(change["change"]["action"], "recategorize");
    assert_eq!(change["change"]["target"]["id"], *original);
    assert_eq!(change["change"]["related_posting"], result["id"]);
    assert_eq!(change["change"]["source_reference"], "original-category");
    assert_eq!(change["posting"]["target"]["id"], result["id"]);
    Ok(())
}

fn assert_recategorization_entries(agent: &mut Client, id: &Value) -> TestResult {
    let moved_args = json!({"company":"acme","id":id});
    let moved = agent.success("txn_show", &moved_args)?;
    let lines = moved[0]["entries"]
        .as_array()
        .ok_or("missing posted lines")?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["account_code"], "4000");
    assert_eq!(lines[0]["direction"], "debit");
    assert_eq!(lines[0]["amount"], 1250);
    assert_eq!(lines[0]["tax_category"], "old-tax");
    assert_eq!(lines[1]["account_code"], "4100");
    assert_eq!(lines[1]["direction"], "credit");
    assert_eq!(lines[1]["amount"], 1250);
    assert_eq!(lines[1]["tax_category"], "new-tax");
    Ok(())
}

fn assert_recategorization_pages(agent: &mut Client, expected_ids: &[&Value]) -> TestResult {
    let total = expected_ids.len();
    for (offset, id) in expected_ids
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
    {
        let response = agent.call(
            "txn_list",
            &json!({"company":"acme", "limit":1, "offset":offset}),
        )?;
        let page = payload(&response)?;
        let rows = page.as_array().ok_or("missing transaction rows")?;
        assert!(rows.iter().map(|row| &row["id"]).eq(id));
        assert_eq!(response["result"]["_meta"]["pagination"]["total"], total);
    }
    assert_eq!(
        agent.success("txn_list", &json!({"company":"acme", "count":true}))?,
        json!({"count":total})
    );
    Ok(())
}

fn post_partially_cleared_sale(
    agent: &mut Client,
    owner: &mut Client,
) -> Result<Value, Box<dyn Error>> {
    let mut input = posting("original-category");
    input["tax"] = json!(["4000=old-tax"]);
    let original = agent.success("txn_post", &input)?;
    let selector = json!({"company":"acme","id":original["id"]});
    let shown = agent.success("txn_show", &selector)?;
    owner.success(
        "txn_clear",
        &json!({
            "company":"acme","target":"entry","transaction_id":original["id"],
            "entry":shown[0]["entries"][0]["id"],"amount":"2.00","date":"2026-03-02",
            "status":"cleared"
        }),
    )?;
    Ok(original)
}

#[test]
fn recategorization_posts_directly_and_undoes_with_one_receipt_across_cli_and_mcp() -> TestResult {
    let fixture = Fixture::new(true)?;
    let credential = fixture.issue("recategorizer", "ledger/read,ledger/post", &[])?;
    let mut agent = fixture.client(&credential)?;
    let mut owner = fixture.client(&fixture.owner)?;
    create_recategory(&mut owner)?;
    let original = post_partially_cleared_sale(&mut agent, &mut owner)?;
    let original_selector = json!({"company":"acme","id":original["id"]});
    let before = agent.success("txn_show", &original_selector)?;

    let result = cli_data(fixture.command(&credential)?.args([
        "txn",
        "recategorize",
        "--reference",
        "original-category",
        "--to-account",
        "4100",
        "--posting-reference",
        "category-fix",
        "--tax-category",
        "new-tax",
    ]))?;
    assert_eq!(result["original_id"], original["id"]);
    assert_eq!(result["date"], "2026-03-01");
    assert_eq!(result["amount"], 1250);
    assert_eq!(result["currency"], "USD");
    assert_eq!(result["direction"], "credit");
    assert_eq!(result["from_account"], "4000");
    assert_eq!(result["to_account"], "4100");
    assert_eq!(result["skipped"], false);
    assert!(result["desktop_path"].is_string());

    assert_recategorization_entries(&mut agent, &result["id"])?;
    assert_eq!(agent.success("txn_show", &original_selector)?, before);
    assert_recategorization_pages(&mut agent, &[&original["id"], &result["id"]])?;

    let mut retry = json!({
        "company":"acme","reference":"original-category","to_account":"4100",
        "posting_reference":"category-fix","tax_category":"new-tax","on_conflict":"skip",
    });
    let retry_result = agent.success("txn_recategorize", &retry)?;
    assert_eq!(retry_result["id"], result["id"]);
    assert_eq!(retry_result["receipt_id"], result["receipt_id"]);
    assert_eq!(retry_result["skipped"], true);
    retry["description"] = json!("different intent");
    assert_eq!(
        payload(&agent.call("txn_recategorize", &retry)?)?["code"],
        "RECATEGORIZE_BLOCKED"
    );

    assert_recategorization_receipt(&mut agent, &original["id"], &result)?;

    let repeated = json!({"company":"acme","id":original["id"],"to_account":"4100"});
    let blocked = payload(&agent.call("txn_recategorize", &repeated)?)?;
    assert!(
        blocked["message"]
            .as_str()
            .unwrap_or_default()
            .contains("active recategorization")
    );
    let blocked = payload(&agent.call("txn_reverse", &original_selector)?)?;
    assert!(
        blocked["message"]
            .as_str()
            .unwrap_or_default()
            .contains("active recategorization")
    );

    let undo_args = json!({"company":"acme","receipt":result["receipt_id"]});
    let undo = agent.success("audit_undo", &undo_args)?;
    assert_eq!(undo["kind"], "reversed");
    assert_eq!(undo["transaction"], result["id"]);
    assert_eq!(
        cli_data(fixture.command(&credential)?.args([
            "audit",
            "undo",
            &result["receipt_id"].to_string(),
        ]))?,
        undo,
    );
    assert_eq!(agent.success("txn_show", &original_selector)?, before);
    let reversed = agent.success("txn_show", &json!({"company":"acme","id":undo["reversal"]}))?;
    assert_recategorization_pages(&mut agent, &[&original["id"]])?;
    assert_eq!(
        agent.success(
            "txn_list",
            &json!({"company":"acme", "count":true, "include_reversed":true}),
        )?,
        json!({"count":3})
    );
    let blocked = payload(&agent.call("txn_recategorize", &repeated)?)?;
    assert!(
        blocked["message"]
            .as_str()
            .unwrap_or_default()
            .contains("last undo")
    );
    let mut corrected = repeated;
    corrected["date"] = reversed[0]["date"].clone();
    agent.success("txn_recategorize", &corrected)?;
    agent.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn recategorization_proposals_use_existing_reviews_and_check_the_live_period() -> TestResult {
    let fixture = Fixture::new(true)?;
    let credential = fixture.proposer()?;
    let mut proposer = fixture.client(&credential)?;
    let mut owner = fixture.client(&fixture.owner)?;
    create_recategory(&mut owner)?;
    let depositor = fixture.issue("recategorization_depositor", "ledger/read,ledger/post", &[])?;
    let original = fixture
        .client(&depositor)?
        .success("txn_post", &posting("proposed-source"))?;
    let mut args = json!({
        "company":"acme","id":original["id"],"from_account":"4000","to_account":"4100",
        "posting_reference":"approved-recategorization",
    });
    let proposal = proposer.success("txn_recategorize", &args)?;
    assert_eq!(proposal["status"], "pending");
    args["on_conflict"] = json!("skip");
    assert_eq!(proposer.success("txn_recategorize", &args)?, proposal);
    let book = json!({"company":"acme"});
    assert_eq!(
        owner.success("txn_list", &book)?.as_array().map(Vec::len),
        Some(1)
    );

    owner.success(
        "period_close",
        &json!({"company":"acme","through":"2026-03-01"}),
    )?;
    let approval = json!({"company":"acme","id":proposal["proposal_id"]});
    let blocked = payload(&owner.call("txn_approve", &approval)?)?;
    assert!(
        blocked["message"]
            .as_str()
            .unwrap_or_default()
            .contains("closed period")
    );
    assert_eq!(
        owner.success("txn_proposals", &book)?[0]["status"],
        "pending"
    );
    owner.success(
        "period_reopen",
        &json!({"company":"acme","reason":"Test approval"}),
    )?;

    let applied = owner.success("txn_approve", &approval)?;
    assert_eq!(applied["status"], "approved");
    assert_eq!(applied["effect"], "recategorized");
    assert!(applied["receipt_id"].is_number());
    assert_eq!(owner.success("txn_approve", &approval)?, applied);
    assert_eq!(
        owner.success("txn_list", &book)?.as_array().map(Vec::len),
        Some(2)
    );
    owner.success(
        "audit_undo",
        &json!({"company":"acme","receipt":applied["receipt_id"]}),
    )?;
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn recategorization_refuses_ambiguous_missing_and_reconciled_lines() -> TestResult {
    let fixture = Fixture::new(true)?;
    let mut owner = fixture.client(&fixture.owner)?;
    let credential = fixture.issue(
        "recategorization_guardrails",
        "ledger/read,ledger/post",
        &[],
    )?;
    let mut agent = fixture.client(&credential)?;
    create_recategory(&mut owner)?;
    let mut transaction = posting("split-credit");
    transaction["credits"] =
        json!([{"account":"4000","amount":"5"}, {"account":"4000","amount":"7.50"}]);
    let original = agent.success("txn_post", &transaction)?;
    let shown = agent.success("txn_show", &json!({"company":"acme","id":original["id"]}))?;
    let mut request = json!({"company":"acme","id":original["id"],"to_account":"4100"});
    let ambiguous = payload(&agent.call("txn_recategorize", &request)?)?;
    assert_eq!(ambiguous["code"], "RECATEGORIZE_BLOCKED");
    assert!(
        ambiguous["message"]
            .as_str()
            .unwrap_or_default()
            .contains("entry ID")
    );
    request["entry_id"] = shown[0]["entries"][1]["id"].clone();
    request["from_account"] = json!("1000");
    assert_eq!(
        payload(&agent.call("txn_recategorize", &request)?)?["code"],
        "NOT_FOUND"
    );
    request["from_account"] = json!("4000");
    request["to_account"] = json!("4000");
    let same_account = payload(&agent.call("txn_recategorize", &request)?)?;
    assert!(
        same_account["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must differ")
    );
    request["to_account"] = json!("9999");
    assert_eq!(
        payload(&agent.call("txn_recategorize", &request)?)?["code"],
        "NOT_FOUND"
    );
    request["to_account"] = json!("4100");

    owner.success(
        "txn_clear",
        &json!({
            "company":"acme","target":"entry","transaction_id":original["id"],
            "entry":shown[0]["entries"][0]["id"],"amount":"2.00","date":"2026-03-02",
            "status":"reconciled"
        }),
    )?;
    let blocked = payload(&agent.call("txn_recategorize", &request)?)?;
    assert_eq!(blocked["code"], "RECATEGORIZE_BLOCKED");
    assert!(
        blocked["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reconciled")
    );
    assert_eq!(
        owner
            .success("txn_list", &json!({"company":"acme"}))?
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[test]
fn statement_tools_share_cli_evidence_and_undo_only_under_existing_grants() -> TestResult {
    let fixture = Fixture::new(true)?;
    let credential = fixture.issue(
        "reconciler",
        "ledger/read,ledger/post,ledger/reconcile,ledger/reverse",
        &[],
    )?;
    let mut agent = fixture.client(&credential)?;
    let mut owner = fixture.client(&fixture.owner)?;
    create_recategory(&mut owner)?;
    owner.success(
        "period_close",
        &json!({"company":"acme","through":"2026-02-28"}),
    )?;
    let posted = agent.success("txn_post", &posting("statement-sale"))?;
    let details = agent.success("txn_show", &json!({"company":"acme","id":posted["id"]}))?;
    let entry = &details[0]["entries"][0]["id"];
    agent.success("txn_clear", &json!({
        "company":"acme","target":"entry","transaction_id":posted["id"],"entry":entry,"date":"2026-03-02"
    }))?;
    let mut input = json!({
        "company":"acme","account":"1000","period_end":"2026-03-31",
        "statement_ending_balance":"12.49","entry_ids":[entry]
    });
    let disagreement = payload(&agent.call("reconcile_statement", &input)?)?;
    assert_eq!(disagreement["code"], "STATEMENT_DISCREPANCY");
    assert_eq!(disagreement["details"]["delta"], 1);
    let cli = require_exit(
        fixture.command(&credential)?.args([
            "reconcile",
            "statement",
            "--account",
            "1000",
            "--period-end",
            "2026-03-31",
            "--statement-ending-balance",
            "12.49",
        ]),
        3,
    )?;
    assert_eq!(
        serde_json::from_slice::<Value>(&cli.stderr)?["error"],
        disagreement
    );

    input["statement_ending_balance"] = json!("12.50");
    input["close_period"] = json!(true);
    assert_eq!(
        payload(&agent.call("reconcile_statement", &input)?)?["code"],
        "FORBIDDEN"
    );
    input["close_period"] = json!(false);
    let completed = agent.success("reconcile_statement", &input)?;
    assert_eq!(completed["matched"][0]["entry_id"], *entry);
    let filters = json!({"company":"acme", "account":"1000", "currency":"USD"});
    let reports = agent.success("reconcile_list", &filters)?;
    let cli_list = cli_data(fixture.command(&credential)?.args([
        "reconcile",
        "list",
        "--account",
        "1000",
        "--currency",
        "USD",
    ]))?;
    assert_eq!(cli_list, reports);
    assert_eq!(reports["statements"][0], completed);

    let reverse = payload(&agent.call(
        "txn_reverse",
        &json!({
            "company":"acme","id":posted["id"]
        }),
    )?)?;
    assert!(
        reverse["message"]
            .as_str()
            .is_some_and(|message| message.contains("reconciled"))
    );
    let move_line = payload(&agent.call(
        "txn_recategorize",
        &json!({
            "company":"acme","id":posted["id"],"to_account":"4100"
        }),
    )?)?;
    assert_eq!(move_line["code"], "RECATEGORIZE_BLOCKED");
    assert!(
        move_line["message"]
            .as_str()
            .is_some_and(|message| message.contains("reconciled"))
    );

    let undo = json!({"company":"acme","receipt":completed["receipt"]});
    assert_eq!(
        agent.success("audit_undo", &undo)?["kind"],
        "statement_unreconciled"
    );

    check_statement_cutoff_undo(&mut owner, &mut agent, input, &filters)
}

fn check_statement_cutoff_undo(
    owner: &mut Client,
    agent: &mut Client,
    mut input: Value,
    filters: &Value,
) -> TestResult {
    input["close_period"] = json!(true);
    let closed = owner.success("reconcile_statement", &input)?;
    let cutoff = owner.success("period_list", &json!({"company":"acme"}))?;
    assert_eq!(cutoff[0]["closed_through"], "2026-03-31");
    let closed_undo = json!({"company":"acme","receipt":closed["receipt"]});
    assert_eq!(
        payload(&agent.call("audit_undo", &closed_undo)?)?["code"],
        "FORBIDDEN"
    );
    let restored = owner.success("audit_undo", &closed_undo)?;
    assert_eq!(restored["kind"], "statement_unreconciled");
    assert_eq!(owner.success("audit_undo", &closed_undo)?, restored);
    let cutoff = owner.success("period_list", &json!({"company":"acme"}))?;
    assert_eq!(cutoff[0]["closed_through"], "2026-02-28");
    let history = owner.success("reconcile_list", filters)?;
    assert_eq!(
        history["statements"][0]["undo_receipt"],
        restored["undo_receipt"]
    );
    assert_eq!(
        history["unreconciled_months"][0]["period_end"],
        "2026-03-31"
    );
    let replaced = owner.success("reconcile_statement", &input)?;
    owner.success(
        "period_reopen",
        &json!({"company":"acme","reason":"Reopened independently"}),
    )?;
    owner.success(
        "period_close",
        &json!({"company":"acme","through":"2026-03-31"}),
    )?;
    let preserved_cutoff = owner.success(
        "audit_undo",
        &json!({
            "company":"acme","receipt":replaced["receipt"]
        }),
    )?;
    assert_eq!(preserved_cutoff["kind"], "statement_unreconciled");
    let cutoff = owner.success("period_list", &json!({"company":"acme"}))?;
    assert_eq!(cutoff[0]["closed_through"], "2026-03-31");
    assert_eq!(owner.success("verify", &json!({}))?["status"], "healthy");
    Ok(())
}
