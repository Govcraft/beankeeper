use std::{error::Error, process::Command};

use assert_cmd::prelude::*;
use beankeeper_cli::db::Db;
use serde_json::{Value, json};

use super::{
    Client, TestResult, bk, identity_client, identity_ledger, payload, posting, proposal_identity,
    tempdir,
};

const BOOKKEEPER: &[&str] = &["ledger/read", "ledger/post", "ledger/propose"];
const NAME_ERROR: &str = "account name must not be empty after trimming whitespace";

struct Fixture {
    dir: tempfile::TempDir,
    owner: Value,
    direct: Value,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let passphrase = dir.path().join("passphrase");
        identity_ledger(&path, &passphrase)?;
        let db = Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
        let owner = proposal_identity(
            &db,
            "owner",
            &["ledger/admin", "ledger/read", "ledger/approve"],
        )?;
        let direct = proposal_identity(&db, "direct_bookkeeper", BOOKKEEPER)?;
        let fixture = Self { dir, owner, direct };
        fixture
            .cli(&fixture.owner)?
            .args(["policy", "set", "--enforce", "on"])
            .assert()
            .success();
        Ok(fixture)
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("books.db")
    }

    fn passphrase(&self) -> std::path::PathBuf {
        self.dir.path().join("passphrase")
    }

    fn cli(&self, credential: &Value) -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&self.path())?;
        command
            .arg("--passphrase-file")
            .arg(self.passphrase())
            .env("BK_AGENT_TOKEN", token(credential)?)
            .args(["--company", "acme", "--json"]);
        Ok(command)
    }

    fn client(&self, credential: &Value) -> Result<Client, Box<dyn Error>> {
        identity_client(
            &self.path(),
            &self.passphrase(),
            Some(token(credential)?),
            false,
        )
    }

    fn issue_proposer(&self) -> Result<Value, Box<dyn Error>> {
        cli_data(self.cli(&self.owner)?.args([
            "agent",
            "issue",
            "--name",
            "propose_bookkeeper",
            "--generate-key",
            "--caps",
            &BOOKKEEPER.join(","),
            "--write-mode",
            "propose-only",
        ]))
    }
}

fn token(credential: &Value) -> Result<&str, Box<dyn Error>> {
    credential["token"]
        .as_str()
        .ok_or_else(|| "missing token".into())
}

fn cli_data(command: &mut Command) -> Result<Value, Box<dyn Error>> {
    let output = command.assert().success();
    let envelope: Value = serde_json::from_slice(&output.get_output().stdout)?;
    Ok(envelope["data"].clone())
}

fn rename(name: &str) -> Value {
    json!({"company": "acme", "code": "1000", "name": name})
}

fn assert_rename_schema(agent: &mut Client) -> TestResult {
    let listed = agent.request("tools/list", &json!({}))?;
    let rename = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "account_rename")
        .ok_or("rename tool unavailable")?;
    let schema = &rename["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["company", "code", "name"]));
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert_eq!(
        schema["properties"]["name"]["examples"],
        json!(["Operating Cash"])
    );
    let schema = schema.to_string();
    assert!(
        !schema.contains("$ref") && !schema.contains("$defs"),
        "{rename}"
    );
    Ok(())
}

#[test]
fn bookkeeper_renames_only_the_label_and_owner_undo_restores_it() -> TestResult {
    let fixture = Fixture::new()?;
    let mut bookkeeper = fixture.client(&fixture.direct)?;
    let mut owner = fixture.client(&fixture.owner)?;
    assert_rename_schema(&mut bookkeeper)?;
    bookkeeper.success("txn_post", &posting("rename-history"))?;
    let balances = json!({"company":"acme", "with_balances":true, "currency":"USD"});
    let history = json!({"company":"acme", "account":"1000"});
    let mut expected = bookkeeper.success("account_list", &balances)?;
    let posted = bookkeeper.success("txn_list", &history)?;
    assert_eq!(posted.as_array().map(Vec::len), Some(1));

    let result = bookkeeper.success("account_rename", &rename("  Operating Cash  "))?;
    assert_eq!(result["status"], "renamed");
    assert_eq!(result["code"], "1000");
    assert_eq!(result["name"], "Operating Cash");
    assert_eq!(result["previous_name"], "Cash");
    let receipt = result["receipt_id"].to_string();
    assert!(
        result["desktop_path"]
            .as_str()
            .ok_or("missing Desktop link")?
            .ends_with(&format!("#receipt-{receipt}"))
    );
    assert_eq!(
        result,
        cli_data(fixture.cli(&fixture.direct)?.args([
            "account",
            "rename",
            "1000",
            "Operating Cash"
        ]))?
    );
    expected[0]["name"] = json!("Operating Cash");
    assert_eq!(bookkeeper.success("account_list", &balances)?, expected);
    assert_eq!(bookkeeper.success("txn_list", &history)?, posted);
    let receipts = owner.success(
        "audit_receipts",
        &json!({"company":"acme", "cursor": receipt.parse::<i64>()? - 1}),
    )?;
    let entries = receipts["receipts"].as_array().ok_or("missing receipts")?;
    assert_eq!(entries.len(), 1, "{receipts}");
    let summary = &entries[0]["change"]["summary"];
    assert_eq!(
        summary,
        &json!({"kind":"account_renamed", "before":{"name":"Cash"}, "after":{"name":"Operating Cash"}})
    );

    let undo = owner.success(
        "audit_undo",
        &json!({"company":"acme", "receipt":result["receipt_id"]}),
    )?;
    assert_eq!(undo["kind"], "account_name_restored");
    assert_eq!(undo["name"], "Cash");
    assert_eq!(
        undo,
        cli_data(
            fixture
                .cli(&fixture.owner)?
                .args(["audit", "undo", &receipt])
        )?
    );
    expected[0]["name"] = json!("Cash");
    assert_eq!(bookkeeper.success("account_list", &balances)?, expected);
    assert_eq!(bookkeeper.success("txn_list", &history)?, posted);
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn account_name_validation_and_duplicates_match_across_transports() -> TestResult {
    let fixture = Fixture::new()?;
    let mut bookkeeper = fixture.client(&fixture.direct)?;
    let mut owner = fixture.client(&fixture.owner)?;
    assert_eq!(
        bookkeeper.success("account_rename", &rename("  Cash  "))?["status"],
        "unchanged"
    );
    let duplicate = payload(&bookkeeper.call("account_rename", &rename("Sales"))?)?;
    assert_eq!(duplicate["code"], "DUPLICATE_ACCOUNT_NAME", "{duplicate}");
    assert!(
        duplicate["message"]
            .as_str()
            .ok_or("missing message")?
            .contains("4000")
    );
    let cli = fixture
        .cli(&fixture.direct)?
        .args(["account", "rename", "1000", "Sales"])
        .assert()
        .code(3);
    let error: Value = serde_json::from_slice(&cli.get_output().stderr)?;
    assert_eq!(error["error"], duplicate);

    let cli = fixture
        .cli(&fixture.direct)?
        .args(["account", "rename", "1000", "  "])
        .assert()
        .code(2);
    assert!(String::from_utf8_lossy(&cli.get_output().stderr).contains(NAME_ERROR));
    for response in [
        bookkeeper.call("account_rename", &rename("  "))?,
        owner.call(
            "account_create",
            &json!({"company":"acme", "code":"1010", "name":"  ", "account_type":"asset"}),
        )?,
        bookkeeper.call(
            "ledger_setup_account_add",
            &json!({"target":{"company":"acme","reference":"invalid-name"}, "code":"1010", "name":"  ", "account_type":"asset"}),
        )?,
    ] {
        let invalid = payload(&response)?;
        assert_eq!(invalid["code"], "VALIDATION", "{invalid}");
        let message = invalid["message"].as_str().ok_or("missing message")?;
        assert!(message.contains(NAME_ERROR), "{invalid}");
        assert!(
            !message.contains("deserialize") && !message.contains("--"),
            "{invalid}"
        );
    }
    Ok(())
}

#[test]
fn propose_only_bookkeeper_uses_normal_sign_off_for_rename() -> TestResult {
    let fixture = Fixture::new()?;
    let proposer = fixture.issue_proposer()?;
    let mut agent = fixture.client(&proposer)?;
    let mut owner = fixture.client(&fixture.owner)?;
    assert_rename_schema(&mut agent)?;
    let proposal =
        cli_data(
            fixture
                .cli(&proposer)?
                .args(["account", "rename", "1000", "Main Checking"]),
        )?;
    assert_eq!(proposal["status"], "proposed");
    assert_eq!(
        proposal,
        agent.success("account_rename", &rename("Main Checking"))?
    );
    let account = json!({"company":"acme", "code":"1000"});
    assert_eq!(agent.success("account_show", &account)?[0]["name"], "Cash");
    let stale = agent.success("account_rename", &rename("Older proposal"))?;
    let review = json!({"company":"acme", "id":proposal["proposal_id"]});
    let proposals = owner.success("txn_proposals", &json!({"company":"acme"}))?;
    assert_eq!(proposals[0]["id"], proposal["proposal_id"]);
    assert_eq!(proposals[0]["status"], "pending");
    assert_eq!(proposals[0]["kind"], "account_rename");
    let denied = agent.call("txn_approve", &review)?;
    assert!(
        denied.get("error").is_some() || denied["result"]["isError"] == true,
        "{denied}"
    );

    let approved = owner.success("txn_approve", &review)?;
    assert_eq!(approved["status"], "approved", "{approved}");
    let id = proposal["proposal_id"]
        .as_str()
        .ok_or("missing proposal ID")?;
    assert_eq!(
        approved,
        cli_data(fixture.cli(&fixture.owner)?.args(["txn", "approve", id]))?
    );
    assert_eq!(
        agent.success("account_show", &account)?[0]["name"],
        "Main Checking"
    );
    let stale_review = json!({"company":"acme", "id":stale["proposal_id"]});
    let invalid = payload(&owner.call("txn_approve", &stale_review)?)?;
    assert_eq!(invalid["code"], "VALIDATION", "{invalid}");
    assert_eq!(
        agent.success("txn_reject", &stale_review)?["status"],
        "withdrawn"
    );

    let restored = owner.success(
        "audit_undo",
        &json!({"company":"acme", "receipt":approved["receipt_id"]}),
    )?;
    assert_eq!(restored["name"], "Cash");
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn undo_restores_the_exact_legacy_label_and_verify_checks_current_label() -> TestResult {
    let fixture = Fixture::new()?;
    let legacy_label = "  Cash\t ";
    let conn = rusqlite::Connection::open(fixture.path())?;
    conn.pragma_update(None, "key", "mcp-test-passphrase")?;
    conn.execute(
        "UPDATE accounts SET name = ?1 WHERE company_slug = 'acme' AND code = '1000'",
        [legacy_label],
    )?;
    let receipt = cli_data(
        fixture
            .cli(&fixture.direct)?
            .args(["account", "rename", "1000", "Checking"]),
    )?;
    assert_eq!(receipt["previous_name"], legacy_label);
    let mut owner = fixture.client(&fixture.owner)?;
    let mut bookkeeper = fixture.client(&fixture.direct)?;
    let newer = bookkeeper.success("account_rename", &rename("Main Checking"))?;
    let original_undo = json!({"company":"acme", "receipt":receipt["receipt_id"]});
    let stale = payload(&owner.call("audit_undo", &original_undo)?)?;
    assert_eq!(stale["code"], "UNDO_INELIGIBLE");
    owner.success(
        "audit_undo",
        &json!({"company":"acme", "receipt":newer["receipt_id"]}),
    )?;
    assert_eq!(
        bookkeeper.success("account_rename", &rename("Checking"))?,
        receipt
    );
    let restored = owner.success(
        "audit_undo",
        &json!({"company":"acme", "receipt":receipt["receipt_id"]}),
    )?;
    assert_eq!(restored["name"], legacy_label);
    let current: String = conn.query_row(
        "SELECT name FROM accounts WHERE company_slug = 'acme' AND code = '1000'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(current, legacy_label);
    owner.success("verify", &json!({}))?;

    conn.execute(
        "UPDATE accounts SET name = 'Different' WHERE company_slug = 'acme' AND code = '1000'",
        [],
    )?;
    let error = payload(&owner.call("verify", &json!({}))?)?;
    assert_eq!(error["code"], "DATABASE", "{error}");
    assert!(
        error["message"]
            .as_str()
            .ok_or("missing message")?
            .contains("account label")
    );
    Ok(())
}
