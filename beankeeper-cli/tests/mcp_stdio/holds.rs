//! A hold recorded through one transport can be settled or undone through the other.

use assert_cmd::prelude::*;
use serde_json::{Value, json};
use std::path::Path;

use super::{
    Client, TestResult, assert_parameter_rejection, bk, identity_client, identity_ledger, payload,
    proposal_identity, seed, tempdir,
};

fn cli(path: &Path, args: &[&str]) -> Result<Value, Box<dyn std::error::Error>> {
    let mut cmd = bk(path)?;
    let output = cmd
        .args(["--json", "--company", "acme"])
        .args(args)
        .assert()
        .success();
    let value: Value = serde_json::from_slice(&output.get_output().stdout)?;
    Ok(value["data"].clone())
}

fn balances(client: &mut Client, posted: i64, available: i64, to: &str) -> TestResult {
    let accounts = client.success(
        "account_list",
        &json!({"company":"acme", "currency":"USD", "with_balances":true, "to":to}),
    )?;
    let cash = &accounts[0];
    assert_eq!(cash["code"], "1000");
    assert_eq!(cash["posted_balance"], posted);
    assert_eq!(cash["available_balance"], available);
    assert_eq!(cash["desktop_view"]["view_id"], "transactions");
    Ok(())
}

fn pharmacy_hold(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    cli(
        path,
        &["account", "create", "5000", "Expense", "--type", "expense"],
    )?;
    cli(
        path,
        &[
            "txn",
            "post",
            "-d",
            "Cash received",
            "--debit",
            "1000:633.60",
            "--credit",
            "4000:633.60",
            "--date",
            "2026-03-01",
        ],
    )?;
    cli(
        path,
        &[
            "txn",
            "post",
            "-d",
            "Pharmacy",
            "--debit",
            "5000:43.45",
            "--credit",
            "1000:43.45",
            "--status",
            "pending",
            "--reference",
            "hold-1",
            "--date",
            "2026-03-02",
        ],
    )
}

#[test]
fn pending_settlement_and_reversal_have_dated_balances_and_exact_retries() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("holds.db");
    seed(&path)?;
    let hold = pharmacy_hold(&path)?;
    assert_eq!(hold["status"], "pending");
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut mcp = Client::spawn(command)?;
    mcp.initialize()?;
    balances(&mut mcp, 63_360, 59_015, "2026-03-02")?;
    let displayed = mcp.success("txn_show", &json!({"company":"acme","id":hold["id"]}))?;
    assert_eq!(displayed[0]["entries"][0]["posting_status"], "pending");
    let trial = cli(&path, &["report", "trial-balance", "--to", "2026-03-02"])?;
    assert_eq!(trial["accounts"][0]["credit_total"], 0);

    let mut request = json!({
        "company":"acme", "hold":{"reference":"hold-1"}, "amount":"43.88",
        "date":"2026-03-04", "reference":"posted-1"
    });
    let result = mcp.success("txn_settle_hold", &request)?;
    let posted_id = result["transaction_id"].to_string();
    assert_eq!(result["status"], "created");
    assert_eq!(
        result["settlement"],
        json!({"hold_id":hold["id"], "amount_delta":43})
    );
    assert_eq!(result["amount_delta_display"], "0.43");
    assert_eq!(result["desktop_view"]["view_id"], "transactions");
    balances(&mut mcp, 63_360, 59_015, "2026-03-02")?;
    balances(&mut mcp, 58_972, 58_972, "2026-03-04")?;

    let retry = cli(
        &path,
        &[
            "txn",
            "settle-hold",
            "--hold-reference",
            "hold-1",
            "--amount",
            "43.88",
            "--date",
            "2026-03-04",
            "--reference",
            "posted-1",
        ],
    )?;
    assert_eq!(retry["status"], "skipped");
    assert_eq!(retry["transaction_id"], result["transaction_id"]);
    request["reference"] = json!("double-settle");
    let error = mcp.call("txn_settle_hold", &request)?;
    assert_eq!(payload(&error)?["code"], "VALIDATION");
    assert!(
        payload(&error)?["message"]
            .as_str()
            .is_some_and(|text| text.contains("already closed"))
    );

    cli(
        &path,
        &["txn", "reverse", &posted_id, "--date", "2026-03-05"],
    )?;
    balances(&mut mcp, 58_972, 58_972, "2026-03-04")?;
    balances(&mut mcp, 63_360, 59_015, "2026-03-05")?;
    request["hold"] = json!({"id":hold["id"]}).to_string().into();
    request["date"] = json!("2026-03-06");
    request["amount"] = json!("42.99");
    let second = mcp.success("txn_settle_hold", &request)?;
    assert_eq!(second["settlement"]["amount_delta"], -46);
    balances(&mut mcp, 59_061, 59_061, "2026-03-06")?;
    mcp.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn propose_only_settlement_is_approved_then_undone_from_its_receipt() -> TestResult {
    use beankeeper_cli::db::Db;
    let dir = tempdir()?;
    let path = dir.path().join("proposal.db");
    let passphrase = dir.path().join("passphrase");
    identity_ledger(&path, &passphrase)?;
    let secret = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&secret))?;
    let owner = proposal_identity(
        &db,
        "owner",
        &["ledger/admin", "ledger/read", "ledger/approve"],
    )?;
    let issued = bk(&path)?
        .arg("--passphrase-file")
        .arg(&passphrase)
        .args([
            "--json",
            "agent",
            "issue",
            "--name",
            "proposer",
            "--generate-key",
            "--caps",
            "ledger/read,ledger/propose",
            "--write-mode",
            "propose-only",
        ])
        .assert()
        .success();
    let agent: Value = serde_json::from_slice(&issued.get_output().stdout)?;
    let mut owner = identity_client(&path, &passphrase, owner["token"].as_str(), false)?;
    let mut local = identity_client(&path, &passphrase, None, false)?;
    let mut agent = identity_client(&path, &passphrase, agent["data"]["token"].as_str(), false)?;
    let draft = agent.success("txn_post", &json!({
        "company":"acme", "description":"Authorization", "status":"pending", "date":"2026-03-01",
        "reference":"hold-proposed", "debits":[{"account":"4000","amount":"15.00"}],
        "credits":[{"account":"1000","amount":"15.00"}]
    }))?;
    assert_eq!(draft["payload"]["purpose"], "pending");
    balances(&mut local, 0, 0, "2026-03-01")?;
    let hold = owner.success("txn_approve", &json!({"company":"acme", "id":draft["id"]}))?;
    balances(&mut local, 0, -1_500, "2026-03-01")?;
    let mut request = json!({
        "company":"acme","hold":{"id":hold["transaction_id"]},"amount":"15.43","date":"2026-03-02",
        "reference":"posted-proposed"
    });
    let proposal = agent.success("txn_settle_hold", &request)?;
    assert_eq!(proposal["status"], "pending");
    assert_eq!(
        proposal["payload"]["purpose"]["settle"],
        hold["transaction_id"]
    );
    balances(&mut local, 0, -1_500, "2026-03-02")?;

    request["reference"] = json!("competing-proposal");
    let competing = agent.success("txn_settle_hold", &request)?;
    let posted = owner.success(
        "txn_approve",
        &json!({"company":"acme", "id":proposal["id"]}),
    )?;
    assert_eq!(posted["status"], "approved");
    let rejected = owner.call(
        "txn_approve",
        &json!({"company":"acme", "id":competing["id"]}),
    )?;
    assert_eq!(payload(&rejected)?["code"], "VALIDATION");
    balances(&mut local, -1_543, -1_543, "2026-03-02")?;
    let receipts = owner.success("audit_receipts", &json!({"company":"acme"}))?;
    let receipt = receipts["receipts"]
        .as_array()
        .ok_or("missing receipts")?
        .iter()
        .find(|receipt| receipt["change"]["source_reference"] == "posted-proposed")
        .ok_or("missing settlement receipt")?;
    let undone = owner.success(
        "audit_undo",
        &json!({"company":"acme", "receipt":receipt["id"]}),
    )?;
    assert!(undone.is_object());
    balances(&mut local, 0, -1_500, "9999-12-31")?;
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn pending_holds_reject_clearing_and_invalid_settlement_inputs() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("invalid-holds.db");
    seed(&path)?;
    let mut cmd = bk(&path)?;
    cmd.args(["mcp", "serve"]);
    let mut mcp = Client::spawn(cmd)?;
    mcp.initialize()?;
    let hold = mcp.success("txn_post", &json!({
        "company":"acme", "description":"Authorization", "status":"pending", "date":"2026-03-01",
        "reference":"hold-invalid", "debits":[{"account":"4000","amount":"15.00"}],
        "credits":[{"account":"1000","amount":"15.00"}]
    }))?;
    let mut request = json!({
        "company":"acme","hold":{"id":hold["id"],"reference":"hold-invalid"},
        "amount":"15.43","date":"2026-03-02","reference":"posted-invalid"
    });
    assert_parameter_rejection(&mcp.call("txn_settle_hold", &request)?, "untagged enum");
    request["hold"] = json!({"id":hold["id"]});
    request["amount"] = json!("0.00");
    assert_parameter_rejection(&mcp.call("txn_settle_hold", &request)?, "positive");
    request["amount"] = json!("15.433");
    let failed = mcp.call("txn_settle_hold", &request)?;
    assert_eq!(payload(&failed)?["code"], "VALIDATION");
    let displayed = mcp.success("txn_show", &json!({"company":"acme","id":hold["id"]}))?;
    let clearing = mcp.call(
        "txn_clear",
        &json!({
            "company":"acme", "target":"entry", "transaction_id":hold["id"],
            "entry":displayed[0]["entries"][1]["id"], "date":"2026-03-02"
        }),
    )?;
    assert_eq!(clearing["result"]["isError"], true);
    mcp.success(
        "txn_reverse",
        &json!({
            "company":"acme", "id":hold["id"], "date":"2026-03-02"
        }),
    )?;
    balances(&mut mcp, 0, 0, "2026-03-02")?;
    Ok(())
}

#[test]
fn competing_writers_cannot_settle_the_same_hold_twice() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("competing-holds.db");
    seed(&path)?;
    let hold = cli(
        &path,
        &[
            "txn",
            "post",
            "-d",
            "Authorization",
            "--debit",
            "4000:15",
            "--credit",
            "1000:15",
            "--date",
            "2026-03-01",
            "--status",
            "pending",
        ],
    )?;
    let start = |reference: &str| -> Result<_, Box<dyn std::error::Error>> {
        Ok(bk(&path)?
            .args([
                "--json",
                "--company",
                "acme",
                "txn",
                "settle-hold",
                "--id",
                &hold["id"].to_string(),
                "--amount",
                "15.43",
                "--date",
                "2026-03-02",
                "--reference",
                reference,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?)
    };
    let first = start("first")?;
    let second = start("second")?;
    let outcomes = [first.wait_with_output()?, second.wait_with_output()?];
    assert_eq!(
        outcomes.iter().filter(|out| out.status.success()).count(),
        1
    );
    let error = outcomes
        .iter()
        .find(|out| !out.status.success())
        .ok_or("missing error")?;
    let error: Value = serde_json::from_slice(&error.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION", "{error}");
    let rows = cli(&path, &["txn", "list"])?;
    assert_eq!(rows.as_array().ok_or("missing rows")?.len(), 2);
    Ok(())
}
