use std::{error::Error, path::Path};

use assert_cmd::prelude::*;
use beankeeper_cli::db::Db;
use serde_json::{Value, json};

use super::{
    Client, TestResult, assert_parameter_rejection, bk, identity_ledger, proposal_identity,
    tempdir, tool_names,
};

fn client(
    path: &Path,
    passphrase: &Path,
    identity: &Value,
    read_only: bool,
) -> Result<Client, Box<dyn Error>> {
    let mut command = bk(path)?;
    command
        .arg("--passphrase-file")
        .arg(passphrase)
        .env(
            "BK_AGENT_TOKEN",
            identity["token"].as_str().ok_or("missing token")?,
        )
        .env_remove("BK_PLAID_CLIENT_ID")
        .env_remove("BK_PLAID_SECRET")
        .env_remove("BK_PLAID_ACCESS_TOKEN")
        .args(["mcp", "serve"]);
    if read_only {
        command.arg("--read-only");
    }
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

fn assert_no_administration(client: &mut Client, connect: &Value, feed: &Value) -> TestResult {
    let listed = client.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    assert!(names.contains("bank_feed_show"));
    for (tool, arguments) in [
        ("bank_feed_connect", connect),
        ("bank_feed_sync", feed),
        ("bank_feed_disconnect", feed),
    ] {
        assert!(!names.contains(tool));
        let response = client.call(tool, arguments)?;
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{tool}: {response}"
        );
    }
    Ok(())
}

#[test]
fn bank_feed_administration_uses_the_shared_signed_catalog_and_host_credentials() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    let passphrase = directory.path().join("key");
    let reader = identity_ledger(&path, &passphrase)?;
    let db = Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
    let admin = proposal_identity(&db, "feed_admin", &["ledger/admin", "ledger/read"])?;
    let poster = proposal_identity(&db, "feed_poster", &["ledger/post", "ledger/read"])?;
    drop(db);

    let connect = json!({"company":"acme","account":"1000",
        "provider_account":"synthetic-fixture","currency":"USD"});
    let mut write = client(&path, &passphrase, &admin, false)?;
    let listed = write.request("tools/list", &json!({}))?;
    let names = tool_names(&listed)?;
    for name in [
        "bank_feed_connect",
        "bank_feed_sync",
        "bank_feed_disconnect",
    ] {
        assert!(names.contains(name));
    }
    let mut invalid = connect.clone();
    invalid["access_token"] = json!("host-secret-fixture");
    let invalid = write.call("bank_feed_connect", &invalid)?;
    assert_parameter_rejection(&invalid, "unknown field");

    let connected = write.success("bank_feed_connect", &connect)?;
    assert_eq!(connected["connected"], true);
    assert_eq!(connected["environment"], "sandbox");
    assert_eq!(connected.get("provider_account"), None);
    let feed = connected["id"].as_str().ok_or("missing feed ID")?;
    let args = json!({"company":"acme","feed":feed});
    let mut read = client(&path, &passphrase, &reader, false)?;
    assert_no_administration(&mut read, &connect, &args)?;
    let mut read_only = client(&path, &passphrase, &admin, true)?;
    assert_no_administration(&mut read_only, &connect, &args)?;

    let mut direct = client(&path, &passphrase, &poster, false)?;
    let direct_catalog = direct.request("tools/list", &json!({}))?;
    let direct_tools = tool_names(&direct_catalog)?;
    assert!(direct_tools.contains("bank_feed_sync"));
    assert!(!direct_tools.contains("bank_feed_connect"));
    assert!(!direct_tools.contains("bank_feed_disconnect"));
    let directory = direct.success("bank_feed_connections", &json!({"company":"acme"}))?;
    assert_eq!(directory["connections"], json!([connected]));
    assert_eq!(directory["next_after"], Value::Null);
    let after = direct.success(
        "bank_feed_connections",
        &json!({"company":"acme","after":feed}),
    )?;
    assert_eq!(after["connections"], json!([]));
    let missing = direct.call("bank_feed_sync", &args)?;
    assert!(
        missing
            .to_string()
            .contains("bank provider credentials are missing")
    );
    assert_eq!(
        read.success("bank_feed_connections", &json!({"company":"acme"}))?,
        directory
    );

    let scoped = write.call("bank_feed_sync", &args)?;
    assert_eq!(scoped["result"]["isError"], true);
    assert!(
        scoped
            .to_string()
            .contains("bank provider credentials are missing")
    );
    assert_eq!(write.success("bank_feed_show", &args)?, connected);

    let disconnected = write.success("bank_feed_disconnect", &args)?;
    assert_eq!(disconnected["connected"], false);
    let shown = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args(["--json", "--company", "acme", "bank-feed", "show", feed])
        .assert()
        .success();
    let cli: Value = serde_json::from_slice(&shown.get_output().stdout)?;
    assert_eq!(cli["data"], disconnected);
    Ok(())
}
