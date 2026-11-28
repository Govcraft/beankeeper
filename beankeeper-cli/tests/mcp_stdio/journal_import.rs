use std::{error::Error, path::Path};

use beankeeper_cli::{
    db::{self, Actor, Db},
    identity::{Admin, Granted},
    operation::{Exposure, Operation},
};
use serde_json::{Value, json};
use tempfile::tempdir;

use super::{
    Client, TestResult, bk, identity_client, identity_ledger, payload, proposal_identity, seed,
    tool_names,
};

fn journal(reference: Option<&str>) -> Value {
    json!({
        "date": "2018-02-03",
        "description": "Archived sale",
        "currency": "USD",
        "reference": reference,
        "metadata": {"legacy": {"invoice": 48}},
        "entries": [
            {"account_code": "1000", "direction": "debit", "amount": "12.00", "memo": "bank memo"},
            {"account_code": "4000", "direction": "credit", "amount": 10, "tax_category": "sales"},
            {"account_code": "4000", "direction": "credit", "amount": "2.00"}
        ]
    })
}

fn open(path: &Path) -> Result<Client, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

fn cli_import(path: &Path, file: &Path, dry_run: bool) -> Result<Value, Box<dyn Error>> {
    let mut command = bk(path)?;
    command
        .args(["--json", "--company", "acme", "txn", "import", "--file"])
        .arg(file);
    if dry_run {
        command.arg("--dry-run");
    }
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].clone())
}

fn reject(client: &mut Client, input: &Value, detail: &str) -> TestResult {
    let response = client.call("txn_import", input)?;
    assert_eq!(response["result"]["isError"], true, "{response}");
    let data = payload(&response)?;
    assert!(
        data["message"]
            .as_str()
            .is_some_and(|message| message.contains(detail)),
        "{response}"
    );
    Ok(())
}

#[test]
fn journal_import_discovers_and_matches_cli_preview_commit_and_replay() -> TestResult {
    let dir = tempdir()?;
    let remote = dir.path().join("remote.db");
    let local = dir.path().join("local.db");
    seed(&remote)?;
    seed(&local)?;

    assert_eq!(
        Operation::TxnImport.exposure(),
        Exposure::Both("txn_import")
    );
    assert_eq!(Operation::TxnImport.cli_path(), "txn.import");
    let file = dir.path().join("journals.json");
    let batch = vec![journal(Some(" archive:48 ")), journal(None)];
    std::fs::write(&file, serde_json::to_vec(&batch)?)?;

    let mut remote_client = open(&remote)?;
    let listed = remote_client.request("tools/list", &json!({}))?;
    let tool = listed["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "txn_import"))
        .ok_or("missing txn_import tool")?;
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    let schema = &tool["inputSchema"]["properties"]["transactions"];
    assert_eq!(schema["minItems"], 1);
    assert_eq!(schema["maxItems"], 500);
    for field in ["account_code", "tax_category", "direction"] {
        assert!(tool["inputSchema"].to_string().contains(field), "{tool}");
    }

    let mut input = json!({"company":"acme", "transactions":batch, "dry_run":true});
    let preview = remote_client.success("txn_import", &input)?;
    assert_eq!(preview, cli_import(&local, &file, true)?);
    assert_eq!(preview["imported"], 2);
    assert!(preview["transactions"][0].get("id").is_none());
    assert_eq!(
        remote_client.success("txn_list", &json!({"company":"acme"}))?,
        json!([])
    );

    input["dry_run"] = false.into();
    let posted = remote_client.success("txn_import", &input)?;
    assert_eq!(posted, cli_import(&local, &file, false)?);
    assert_eq!(posted["transactions"][0]["reference"], "archive:48");
    assert!(
        posted["transactions"][1]["reference"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("json:sha256:"))
    );

    let replay = remote_client.success("txn_import", &input)?;
    assert_eq!(replay, cli_import(&remote, &file, false)?);
    assert_eq!(replay["skipped"], 2);
    let mut local_client = open(&local)?;
    assert_eq!(local_client.success("txn_import", &input)?, replay);
    let selector = json!({"company":"acme", "id":posted["transactions"][0]["id"]});
    let mcp = remote_client.success("txn_show", &selector)?;
    let cli = local_client.success("txn_show", &selector)?;
    for field in ["entries", "metadata", "currency", "date"] {
        assert_eq!(mcp[0][field], cli[0][field], "{field}");
    }
    assert_eq!(mcp[0]["date"], "2018-02-03");
    assert_eq!(mcp[0]["currency"], "USD");
    let metadata: Value = serde_json::from_str(
        mcp[0]["metadata"]
            .as_str()
            .ok_or("missing journal metadata")?,
    )?;
    assert_eq!(metadata["legacy"]["invoice"], 48);
    let entries = mcp[0]["entries"]
        .as_array()
        .ok_or("missing journal entries")?;
    assert_eq!(entries.len(), 3);
    assert!(
        entries
            .iter()
            .any(|entry| entry["memo"] == "bank memo" && entry["amount"] == 1200)
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["tax_category"] == "sales" && entry["amount"] == 1000)
    );
    remote_client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn journal_import_rejects_bad_batches_and_rolls_back_prior_rows() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut client = open(&path)?;

    let first = journal(Some("first"));
    let mut bad = journal(Some("last"));
    bad["entries"][2]["account_code"] = "9000".into();
    reject(
        &mut client,
        &json!({"company":"acme", "transactions":[first, bad]}),
        "import transaction 2",
    )?;
    let duplicate = json!({"company":"acme", "transactions":[first, first], "on_conflict":"error"});
    reject(&mut client, &duplicate, "import transaction 2")?;

    let mut fractional = journal(Some("fractional"));
    fractional["entries"][0]["amount"] = serde_json::from_str("12.34")?;
    let response = client.call(
        "txn_import",
        &json!({"company":"acme", "transactions":[fractional]}),
    )?;
    super::assert_parameter_rejection(&response, "InputAmount");
    reject(
        &mut client,
        &json!({"company":"acme", "transactions":[]}),
        "1 to 500",
    )?;
    reject(
        &mut client,
        &json!({"company":"acme", "transactions":vec![first.clone(); 501]}),
        "1 to 500",
    )?;
    let mut too_many_lines = first.clone();
    too_many_lines["entries"] = json!(vec![first["entries"][0].clone(); 5_001]);
    reject(
        &mut client,
        &json!({"company":"acme", "transactions":[too_many_lines]}),
        "5000 total entries",
    )?;
    let mut oversize = first;
    oversize["description"] = "a".repeat(1_048_576).into();
    reject(
        &mut client,
        &json!({"company":"acme", "transactions":[oversize]}),
        "1048576 JSON bytes",
    )?;
    assert_eq!(
        client.success("txn_list", &json!({"company":"acme"}))?,
        json!([])
    );
    let audit = client.success(
        "audit_log",
        &json!({"company":"acme", "entity":"transaction"}),
    )?;
    assert_eq!(audit, json!([]));
    client.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn journal_import_requires_direct_authority_and_is_hidden_in_read_only_mode() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let passphrase_file = dir.path().join("passphrase");
    let reader = identity_ledger(&path, &passphrase_file)?;
    let passphrase = secrecy::SecretString::from("mcp-test-passphrase");
    let ledger = Db::open_existing(&path, Some(&passphrase))?;
    let direct = proposal_identity(&ledger, "direct_import", &["ledger/read", "ledger/post"])?;
    let proposer_output = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args([
            "--json",
            "agent",
            "issue",
            "--name",
            "proposal_import",
            "--generate-key",
            "--caps",
            "ledger/read,ledger/post,ledger/propose",
            "--write-mode",
            "propose-only",
        ])
        .output()?;
    assert!(
        proposer_output.status.success(),
        "{}",
        String::from_utf8_lossy(&proposer_output.stderr)
    );
    let proposer: Value = serde_json::from_slice(&proposer_output.stdout)?;
    Granted::<Admin>::with_db(&ledger, &Actor::new("fixture".parse()?), |grant| {
        db::audit::set_policy(grant, true)
    })?;

    let mut input = json!({"company":"acme", "transactions":[journal(Some("archive:48"))]});
    for (token, server_read_only) in [
        (reader["token"].as_str(), false),
        (proposer["data"]["token"].as_str(), false),
        (direct["token"].as_str(), true),
    ] {
        let mut client = identity_client(&path, &passphrase_file, token, server_read_only)?;
        let listed = client.request("tools/list", &json!({}))?;
        assert!(!tool_names(&listed)?.contains("txn_import"));
        for dry_run in [false, true] {
            input["dry_run"] = dry_run.into();
            let response = client.call("txn_import", &input)?;
            assert!(response.get("error").is_some() || response["result"]["isError"] == true);
        }
        assert_eq!(
            client.success("txn_list", &json!({"company":"acme"}))?,
            json!([])
        );
    }
    let mut writer = identity_client(&path, &passphrase_file, direct["token"].as_str(), false)?;
    let listed = writer.request("tools/list", &json!({}))?;
    assert!(tool_names(&listed)?.contains("txn_import"));
    assert_eq!(writer.success("txn_import", &input)?["dry_run"], true);
    input["dry_run"] = false.into();
    assert_eq!(writer.success("txn_import", &input)?["imported"], 1);
    Ok(())
}
