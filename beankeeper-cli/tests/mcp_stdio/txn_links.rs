//! Same-book transaction links use the same contract on the CLI and MCP.

use std::{error::Error, path::Path, process::Command};

use assert_cmd::prelude::*;
use beankeeper_cli::db::{Db, verification::VerifiedLedger};
use serde_json::{Value, json};

use super::{
    Client, TestResult, assert_parameter_rejection, bk, identity_client, identity_ledger, payload,
    posting, proposal_identity, seed, tempdir,
};

fn data(command: &mut Command, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = command.args(args).assert().success();
    let value: Value = serde_json::from_slice(&output.get_output().stdout)?;
    Ok(value["data"].clone())
}

fn link_request(source: &Value, target: &Value, kind: &str) -> Value {
    json!({"company":"acme","source_id":source,"target_id":target,"kind":kind})
}

fn open_client(path: &Path) -> Result<Client, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

#[test]
fn signed_bookkeeper_links_and_undoes_from_either_transport() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("links.db");
    let passphrase_file = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let db = Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
    let identity = proposal_identity(
        &db,
        "bookkeeper",
        &["ledger/read", "ledger/post", "ledger/reverse"],
    )?;
    let token = identity["token"].as_str().ok_or("missing credential")?;
    let mut client = identity_client(&path, &passphrase_file, Some(token), false)?;
    let command = || -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&path)?;
        command
            .arg("--passphrase-file")
            .arg(&passphrase_file)
            .env("BK_AGENT_TOKEN", token)
            .args(["--json", "--company", "acme"]);
        Ok(command)
    };

    let charge = client.success("txn_post", &posting("charge"))?["id"].clone();
    let refund = client.success("txn_post", &posting("refund"))?["id"].clone();
    let linked = data(
        &mut command()?,
        &[
            "txn",
            "link",
            &refund.to_string(),
            &charge.to_string(),
            "--kind",
            "refund",
        ],
    )?;
    assert_eq!(linked["source_id"], refund);
    assert_eq!(linked["target_id"], charge);
    assert_eq!(linked["kind"], "refund");
    assert!(
        linked["desktop_path"]
            .as_str()
            .is_some_and(|path| path.starts_with("/changes?company=acme"))
    );
    for id in [&charge, &refund] {
        let shown = client.success("txn_show", &json!({"company":"acme","id":id}))?;
        assert_eq!(
            shown,
            data(&mut command()?, &["txn", "show", &id.to_string()])?
        );
        assert_eq!(shown[0]["links"][0]["id"], linked["id"]);
        assert_eq!(shown[0]["links"][0]["receipt"], linked["receipt"]);
    }
    let listed = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(listed, data(&mut command()?, &["txn", "list"])?);
    assert_eq!(listed[0]["links"], listed[1]["links"]);
    let receipt = &linked["receipt"];
    let undo = client.success("audit_undo", &json!({"company":"acme","receipt":receipt}))?;
    assert_eq!(undo["kind"], "transaction_link_undone");
    assert_eq!(undo["link"], linked["id"]);
    assert_eq!(
        undo,
        data(&mut command()?, &["audit", "undo", &receipt.to_string()])?
    );

    let relinked = client.success("txn_link", &link_request(&refund, &charge, "refund"))?;
    assert_ne!(relinked["id"], linked["id"]);
    assert_ne!(relinked["receipt"], linked["receipt"]);
    let second = data(
        &mut command()?,
        &["audit", "undo", &relinked["receipt"].to_string()],
    )?;
    assert_eq!(second["link"], relinked["id"]);
    for id in [&charge, &refund] {
        let shown = client.success("txn_show", &json!({"company":"acme","id":id}))?;
        assert_eq!(shown[0]["links"], json!([]));
    }
    let receipts = client.success("audit_receipts", &json!({"company":"acme"}))?;
    let events = receipts["receipts"]
        .as_array()
        .ok_or("missing audit events")?;
    assert!(events.iter().any(|event| {
        event["id"] == *receipt && event["change"]["summary"]["kind"] == "transaction_link_created"
    }));
    assert!(events.iter().any(|event| {
        event["change"]["undo_of"] == *receipt
            && event["change"]["summary"]["kind"] == "transaction_link_undone"
    }));
    VerifiedLedger::read(&db)?;
    Ok(())
}

#[test]
fn only_distinct_same_book_transactions_can_form_an_acyclic_graph() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("links.db");
    seed(&path)?;
    let mut client = open_client(&path)?;
    client.success("company_create", &json!({"slug":"other","name":"Other"}))?;
    for (code, name, kind) in [("1000", "Cash", "asset"), ("4000", "Sales", "revenue")] {
        client.success(
            "account_create",
            &json!({"company":"other","code":code,"name":name,"account_type":kind}),
        )?;
    }
    let a = client.success("txn_post", &posting("a"))?["id"].clone();
    let b = client.success("txn_post", &posting("b"))?["id"].clone();
    let c = client.success("txn_post", &posting("c"))?["id"].clone();
    let mut other = posting("other");
    other["company"] = json!("other");
    let other = client.success("txn_post", &other)?["id"].clone();

    let tools = client.request("tools/list", &json!({}))?;
    let link = tools["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "txn_link")
        .ok_or("missing txn_link")?;
    assert_eq!(
        link["inputSchema"]["properties"]["kind"]["enum"],
        json!([
            "transfer",
            "refund",
            "payment",
            "adjustment",
            "recategorization",
            "related"
        ])
    );
    let unknown = client.call("txn_link", &link_request(&a, &b, "other"))?;
    assert_parameter_rejection(&unknown, "unknown variant");
    client.success("txn_link", &link_request(&a, &b, "related"))?;
    client.success("txn_link", &link_request(&a, &b, "transfer"))?;
    client.success("txn_link", &link_request(&b, &c, "payment"))?;
    for (source, target, kind, code, message) in [
        (&a, &a, "related", "VALIDATION", "cannot link to itself"),
        (&a, &b, "related", "VALIDATION", "already exists"),
        (&b, &a, "refund", "VALIDATION", "would create a cycle"),
        (&c, &a, "adjustment", "VALIDATION", "would create a cycle"),
        (&a, &other, "payment", "NOT_FOUND", "not found in company"),
    ] {
        let error = payload(&client.call("txn_link", &link_request(source, target, kind))?)?;
        assert_eq!(error["code"], code);
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|text| text.contains(message))
        );
    }
    let duplicate = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "txn",
            "link",
            &a.to_string(),
            &b.to_string(),
            "--kind",
            "related",
        ])
        .output()?;
    assert!(!duplicate.status.success());
    let error: Value = serde_json::from_slice(&duplicate.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("already exists"))
    );

    let audit = client.success(
        "audit_log",
        &json!({"company":"acme","action":"link_create"}),
    )?;
    assert_eq!(audit.as_array().map(Vec::len), Some(3));
    VerifiedLedger::read(&Db::open_existing(&path, None)?)?;
    Ok(())
}
