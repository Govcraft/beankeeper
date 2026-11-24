//! Signed bookkeepers correct their work through the CLI and MCP.

use std::{error::Error, path::PathBuf, process::Command};

use assert_cmd::prelude::*;
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

    fn client(&self, credential: &Value) -> Result<Client, Box<dyn Error>> {
        identity_client(
            &self.path,
            &self.passphrase,
            credential["token"].as_str(),
            false,
        )
    }

    fn issue(&self, name: &str, grants: &str, arguments: &[&str]) -> Result<Value, Box<dyn Error>> {
        let issue = self
            .command(&self.owner)?
            .args([
                "agent",
                "issue",
                "--name",
                name,
                "--generate-key",
                "--caps",
                grants,
            ])
            .args(arguments)
            .assert()
            .success();
        let wire: Value = serde_json::from_slice(&issue.get_output().stdout)?;
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

fn cli_data(command: &mut Command) -> Result<Value, Box<dyn Error>> {
    let output = command.assert().success();
    let wire: Value = serde_json::from_slice(&output.get_output().stdout)?;
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
            for admin in [
                "account_create",
                "company_create",
                "period_reopen",
                "ledger_setup_begin",
            ] {
                assert!(!names.contains(admin), "{admin}");
            }
            fixture
                .command(&credential)?
                .args(["account", "create", "5555", "Other", "--type", "asset"])
                .assert()
                .code(7);
            fixture
                .command(&credential)?
                .args(["backup", "--output"])
                .arg(fixture.path.with_extension("backup"))
                .assert()
                .code(7);

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
            let first = cli_data(fixture.command(&credential)?.args([
                "txn",
                "reverse",
                &posted["id"].to_string(),
                "--date",
                "2026-03-02",
                "--reference",
                label,
            ]))?;
            let retry = agent.success(
                "txn_reverse",
                &json!({"company":"acme","id":posted["id"],"reference":label}),
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
        bk(&fixture.path)?
            .arg("--passphrase-file")
            .arg(&fixture.passphrase)
            .args([
                "--company",
                "acme",
                "txn",
                "approve",
                pending["proposal_id"].as_str().ok_or("proposal ID")?,
            ])
            .assert()
            .code(6);
        let approved = owner.success("txn_approve", &approval)?;
        assert_eq!(approved["status"], "approved");
        assert_eq!(approved, owner.success("txn_approve", &approval)?);
    }
    assert_eq!(
        owner
            .success("txn_list", &company)?
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
