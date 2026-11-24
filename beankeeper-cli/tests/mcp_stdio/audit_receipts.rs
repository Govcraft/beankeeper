//! Exercise receipt paging and owner undo through both public transports.

use std::{error::Error, path::Path, process::Command};

use assert_cmd::prelude::*;
use serde_json::{Value, json};
use tempfile::tempdir;

use super::{
    Client, TestResult, bk, identity_client, identity_ledger, payload, posting, proposal_identity,
};

fn receipt_cli(
    path: &Path,
    passphrase_file: &Path,
    token: Option<&str>,
) -> Result<Command, Box<dyn Error>> {
    let mut command = bk(path)?;
    command
        .arg("--passphrase-file")
        .arg(passphrase_file)
        .arg("--json");
    if let Some(token) = token {
        command.env("BK_AGENT_TOKEN", token);
    }
    Ok(command)
}

fn receipt_result(command: &mut Command) -> Result<Value, Box<dyn Error>> {
    let output = command.assert().success();
    let envelope: Value = serde_json::from_slice(&output.get_output().stdout)?;
    Ok(envelope["data"].clone())
}

fn assert_receipt_error(command: &mut Command, mcp: &Value, expected_code: &str) -> TestResult {
    let output = command.assert().failure();
    let cli: Value = serde_json::from_slice(&output.get_output().stderr)?;
    assert_eq!(mcp["result"]["isError"], true, "{mcp}");
    assert_eq!(cli["error"]["code"], expected_code, "{cli}");
    let error = payload(mcp)?;
    assert_eq!(error["code"], expected_code, "{mcp}");
    assert_ne!(cli["error"]["message"].as_str().unwrap_or_default(), "");
    if expected_code.starts_with("UNDO_") {
        assert_eq!(cli["error"]["message"], error["message"]);
    }
    Ok(())
}

#[test]
fn receipt_pages_resume_across_cli_and_mcp_at_the_original_audit_head() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("receipts.db");
    let passphrase = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase)?;
    let mut reader = identity_client(&path, &passphrase, None, true)?;
    let mut writer = identity_client(&path, &passphrase, None, false)?;
    let listed = reader.request("tools/list", &json!({}))?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "audit_receipts"));
    assert!(tools.iter().all(|tool| tool["name"] != "audit_undo"));

    let initial = reader.success("audit_receipts", &json!({"company": "acme"}))?;
    assert_eq!(initial["continuation"], Value::Null);
    let cursor = initial["through"].to_string();
    writer.success("txn_post", &posting("FIRST"))?;
    writer.success("txn_post", &posting("SECOND"))?;
    let mut cli = receipt_cli(&path, &passphrase, None)?;
    let first = receipt_result(cli.args([
        "--company",
        "acme",
        "audit",
        "receipts",
        "--cursor",
        &cursor,
        "--limit",
        "1",
    ]))?;
    let mcp_first = reader.success(
        "audit_receipts",
        &json!({
            "company": "acme", "cursor": initial["through"], "limit": 1
        }),
    )?;
    assert_eq!(first, mcp_first);
    let continuation = first["continuation"]
        .as_str()
        .ok_or("missing continuation")?;
    assert_eq!(first["receipts"][0]["change"]["source_reference"], "FIRST");
    assert_eq!(
        first["receipts"][0]["change"]["summary"]["after"]["entries"][0]["amount"],
        1250
    );
    assert!(
        first["desktop_path"]
            .as_str()
            .ok_or("missing link")?
            .starts_with("/changes?company=acme&after=")
    );

    writer.success("txn_post", &posting("LATER"))?;
    let last = reader.success(
        "audit_receipts",
        &json!({
            "company": "acme", "continuation": continuation
        }),
    )?;
    let mut cli = receipt_cli(&path, &passphrase, None)?;
    assert_eq!(
        last,
        receipt_result(cli.args([
            "audit",
            "receipts",
            "--company",
            "acme",
            "--continuation",
            continuation,
        ]))?
    );
    assert_eq!(last["through"], first["through"]);
    assert_eq!(last["continuation"], Value::Null);
    assert_eq!(last["receipts"][0]["change"]["source_reference"], "SECOND");
    let fresh = reader.success(
        "audit_receipts",
        &json!({
            "company": "acme", "cursor": last["through"]
        }),
    )?;
    assert_eq!(fresh["receipts"][0]["change"]["source_reference"], "LATER");
    let empty = reader.success(
        "audit_receipts",
        &json!({
            "company": "acme", "cursor": fresh["through"]
        }),
    )?;
    assert_eq!(empty["receipts"], json!([]));
    assert_eq!(empty["through"], fresh["through"]);

    assert_invalid_paging(&path, &passphrase, &mut reader, continuation)
}

fn assert_invalid_paging(
    path: &Path,
    passphrase: &Path,
    reader: &mut Client,
    continuation: &str,
) -> TestResult {
    for (args, mut input) in [
        (vec!["--limit", "0"], json!({"limit": 0})),
        (vec!["--limit", "101"], json!({"limit": 101})),
        (vec!["--cursor", "-1"], json!({"cursor": -1})),
        (
            vec!["--continuation", "acme:3:3"],
            json!({"continuation": "acme:3:3"}),
        ),
        (
            vec!["--continuation", "other:1:2"],
            json!({"continuation": "other:1:2"}),
        ),
        (
            vec!["--continuation", "acme:1:99999"],
            json!({"continuation": "acme:1:99999"}),
        ),
        (
            vec!["--cursor", "0", "--continuation", continuation],
            json!({"cursor": 0, "continuation": continuation}),
        ),
        (vec!["--cursor", "99999"], json!({"cursor": 99999})),
    ] {
        input["company"] = json!("acme");
        let mut cli = receipt_cli(path, passphrase, None)?;
        assert_receipt_error(
            cli.args(["--company", "acme", "audit", "receipts"])
                .args(args),
            &reader.call("audit_receipts", &input)?,
            "VALIDATION",
        )?;
    }
    Ok(())
}

#[test]
fn receipt_undo_shares_owner_grant_outcomes_and_named_errors() -> TestResult {
    use beankeeper_cli::db::Db;

    let dir = tempdir()?;
    let path = dir.path().join("undo.db");
    let passphrase = dir.path().join("passphrase");
    let reader = identity_ledger(&path, &passphrase)?;
    let secret = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&secret))?;
    let owner = proposal_identity(&db, "receipt_owner", &["ledger/admin", "ledger/read"])?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    let reader_token = reader["token"].as_str().ok_or("missing reader token")?;
    let mut owner_client = identity_client(&path, &passphrase, Some(owner_token), false)?;
    let mut local_client = identity_client(&path, &passphrase, None, false)?;
    let book = json!({"company": "acme"});
    local_client.success("company_create", &json!({"slug":"other","name":"Other"}))?;
    let initial = owner_client.success("audit_receipts", &book)?;
    let cash = initial["receipts"]
        .as_array()
        .ok_or("missing receipts")?
        .iter()
        .find(|row| row["change"]["target"]["id"] == "1000")
        .ok_or("missing cash receipt")?["id"]
        .clone();
    let cash_input = json!({"company": "acme", "receipt": cash});
    assert_owner_required(&path, &passphrase, reader_token, &cash)?;

    let posted = local_client.success("txn_post", &posting("ORIGINAL"))?;
    let blocked = local_client.success("txn_post", &posting("BLOCKED"))?;
    let changes = owner_client.success(
        "audit_receipts",
        &json!({
            "company": "acme", "cursor": initial["through"]
        }),
    )?;
    let original = &changes["receipts"][0]["id"];
    let mut cli = receipt_cli(&path, &passphrase, Some(owner_token))?;
    let reversal =
        receipt_result(cli.args(["audit", "undo", "--company", "acme", &original.to_string()]))?;
    assert_eq!(reversal["kind"], "reversed");
    assert_eq!(reversal["transaction"], posted["id"]);
    assert_eq!(
        reversal,
        owner_client.success(
            "audit_undo",
            &json!({
                "company": "acme", "receipt": original
            })
        )?
    );

    local_client.success(
        "txn_reverse",
        &json!({"company":"acme", "id": blocked["id"]}),
    )?;
    let archived = owner_client.success("audit_undo", &cash_input)?;
    assert_eq!(archived["kind"], "archived");
    assert_eq!(archived["account"], "1000");
    let mut cli = receipt_cli(&path, &passphrase, Some(owner_token))?;
    assert_eq!(
        archived,
        receipt_result(cli.args(["--company", "acme", "audit", "undo", &cash.to_string(),]))?
    );

    for (company, receipt, code) in [
        ("acme", json!(0), "VALIDATION"),
        ("other", original.clone(), "NOT_FOUND"),
        ("acme", reversal["undo_receipt"].clone(), "UNDO_UNSUPPORTED"),
        (
            "acme",
            changes["receipts"][1]["id"].clone(),
            "UNDO_INELIGIBLE",
        ),
    ] {
        let mut cli = receipt_cli(&path, &passphrase, Some(owner_token))?;
        assert_receipt_error(
            cli.args(["--company", company, "audit", "undo", &receipt.to_string()]),
            &owner_client.call(
                "audit_undo",
                &json!({"company": company, "receipt": receipt}),
            )?,
            code,
        )?;
    }
    let undos = owner_client.success(
        "audit_receipts",
        &json!({
            "company":"acme", "cursor": changes["through"]
        }),
    )?;
    assert_eq!(undos["receipts"][0]["change"]["undo_of"], *original);
    assert_eq!(
        undos["receipts"][0]["posting"]["target"]["id"],
        reversal["reversal"]
    );
    assert_eq!(undos["receipts"][2]["change"]["undo_of"], cash);
    assert_eq!(
        undos["receipts"]
            .as_array()
            .ok_or("missing receipts")?
            .len(),
        3
    );
    owner_client.success("verify", &json!({}))?;
    Ok(())
}

fn assert_owner_required(
    path: &Path,
    passphrase: &Path,
    reader_token: &str,
    cash: &Value,
) -> TestResult {
    let cash_input = json!({"company": "acme", "receipt": cash});
    for (token, code) in [(None, "UNAUTHORIZED"), (Some(reader_token), "FORBIDDEN")] {
        let mut client = identity_client(path, passphrase, token, false)?;
        let mut cli = receipt_cli(path, passphrase, token)?;
        assert_receipt_error(
            cli.args(["audit", "undo", "--company", "acme", &cash.to_string()]),
            &client.call("audit_undo", &cash_input)?,
            code,
        )?;
    }
    Ok(())
}

#[test]
fn annotation_receipts_restore_exact_metadata_through_cli_and_mcp() -> TestResult {
    use beankeeper_cli::db::Db;

    let dir = tempdir()?;
    let path = dir.path().join("annotation-undo.db");
    let passphrase = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase)?;
    let secret = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&secret))?;
    let owner = proposal_identity(&db, "annotation_owner", &["ledger/admin", "ledger/read"])?;
    let token = owner["token"].as_str().ok_or("missing owner token")?;
    let mut agent = identity_client(&path, &passphrase, None, false)?;
    let mut owner = identity_client(&path, &passphrase, Some(token), false)?;

    let exact = r#"{ "status": "cleared", "large": 123456789012345678901234567890 }"#;
    for (reference, before) in [
        ("with-metadata", json!(exact)),
        ("without-metadata", Value::Null),
    ] {
        let mut input = posting(reference);
        input["metadata"] = before.clone();
        input["meta"] = json!({});
        let id = agent.success("txn_post", &input)?["id"].clone();
        let set = agent.success(
            "txn_meta_set",
            &json!({
                "company":"acme", "id":id, "values":{"status":"pending"}
            }),
        )?;
        let original = &set["receipt"];
        assert!(original.is_i64(), "{set}");
        assert!(
            set["desktop_path"]
                .as_str()
                .is_some_and(|path| path.ends_with(&format!("#receipt-{original}")))
        );

        let unset = receipt_result(receipt_cli(&path, &passphrase, None)?.args([
            "--company",
            "acme",
            "txn",
            "meta",
            "unset",
            "--reference",
            reference,
            "status",
        ]))?;
        let removed = &unset["receipt"];
        assert!(removed.is_i64(), "{unset}");
        let original_input = json!({"company":"acme", "receipt":original});
        let stale = owner.call("audit_undo", &original_input)?;
        assert_eq!(payload(&stale)?["code"], "UNDO_INELIGIBLE");

        let restored =
            owner.success("audit_undo", &json!({"company":"acme", "receipt":removed}))?;
        assert_eq!(restored["kind"], "annotations_restored");
        assert_eq!(restored["transaction"], id);
        let show = json!({"company":"acme", "id":id});
        assert_eq!(
            agent.success("txn_show", &show)?[0]["metadata"],
            set["metadata"]
        );

        let undo = receipt_result(receipt_cli(&path, &passphrase, Some(token))?.args([
            "--company",
            "acme",
            "audit",
            "undo",
            &original.to_string(),
        ]))?;
        assert_eq!(undo["kind"], "annotations_restored");
        assert_eq!(undo, owner.success("audit_undo", &original_input)?);
        assert_eq!(agent.success("txn_show", &show)?[0]["metadata"], before);
    }

    let restores = agent.success("audit_log", &json!({"action":"metadata_restore"}))?;
    assert_eq!(restores.as_array().map(Vec::len), Some(4));
    agent.success("verify", &json!({}))?;
    Ok(())
}
