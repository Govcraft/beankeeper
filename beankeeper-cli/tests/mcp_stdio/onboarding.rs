use super::{
    Client, TestResult, bk, identity_client, identity_ledger, payload, proposal_identity, tempdir,
    tool_names,
};
use assert_cmd::prelude::*;
use beankeeper_cli::db::Db;
use serde_json::{Value, json};
use std::{error::Error, path::Path, process::Command};

const SETTINGS: &str =
    r#"{"purpose":"personal","base_currency":"USD","fiscal_year_start":"01-01"}"#;
const FIRST_CSV: &str =
    "date,description,amount,reference\n2026-01-02,First deposit,12,bank-first\n";
const OVERLAP_CSV: &str = "date,description,amount,reference\n2026-01-02,First deposit,12,bank-first\n2026-01-03,Second deposit,3,bank-second\n2026-01-04,Third deposit,7,bank-third\n";

fn target(company: &str, reference: &str) -> Value {
    json!({"company": company, "reference": reference})
}

fn import(reference: &str, content: &str) -> Value {
    json!({"company":"home", "reference":reference, "bank":"1234", "suspense":"5000",
        "currency":"USD", "format":"csv", "content":content, "csv":{"reference_col":"reference"}})
}

fn opening() -> Value {
    json!({"company":"home", "reference":"opening", "date":"2026-01-01", "currency":"USD",
        "debits":[{"account":"1234","amount":"100"}],
        "credits":[{"account":"3000","amount":"100"}]})
}

fn undo_args(company: &str, outcome: &Value) -> Value {
    json!({"company": company, "receipt": outcome["receipt_id"]})
}

fn undo(owner: &mut Client, company: &str, outcome: &Value) -> Result<Value, Box<dyn Error>> {
    assert!(outcome["receipt_id"].is_i64(), "{outcome}");
    let args = undo_args(company, outcome);
    let undone = owner.success("audit_undo", &args)?;
    assert_eq!(undone["kind"], "onboarding_reverted");
    assert!(undone["undo_receipt"].is_i64(), "{undone}");
    assert_eq!(owner.success("audit_undo", &args)?, undone);
    Ok(undone)
}

fn cli(path: &Path, passfile: &Path, token: &str) -> Result<Command, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.arg("--passphrase-file").arg(passfile);
    command.args(["--json", "--agent-token", token]);
    Ok(command)
}

#[test]
fn existing_direct_bookkeeper_can_onboard_and_owner_can_undo() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("onboarding.db");
    let passfile = dir.path().join("passphrase");
    let reader = identity_ledger(&path, &passfile)?;
    let passphrase = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let owner = proposal_identity(&db, "owner", &["ledger/admin", "ledger/read"])?;
    let old_preset = proposal_identity(
        &db,
        "managed",
        &["ledger/read", "ledger/post", "ledger/propose"],
    )?;
    let unrelated = proposal_identity(&db, "custom", &["ledger/post"])?;
    let token = old_preset["token"]
        .as_str()
        .ok_or("missing bookkeeper token")?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    cli(&path, &passfile, owner_token)?
        .args(["policy", "set", "--enforce", "on"])
        .assert()
        .success();
    let issued = cli(&path, &passfile, owner_token)?
        .args([
            "agent",
            "issue",
            "--name",
            "proposer",
            "--generate-key",
            "--caps",
            "ledger/post",
            "--write-mode",
            "propose-only",
        ])
        .assert()
        .success();
    let proposer = serde_json::from_slice::<Value>(&issued.get_output().stdout)?;
    let mut agent = identity_client(&path, &passfile, Some(token), false)?;
    let mut owner = identity_client(&path, &passfile, Some(owner_token), false)?;
    assert_tool_access(&mut agent, true)?;
    for credential in [reader, unrelated, proposer["data"].clone()] {
        let mut client = identity_client(&path, &passfile, credential["token"].as_str(), false)?;
        assert_tool_access(&mut client, false)?;
    }

    let settings: Value = serde_json::from_str(SETTINGS)?;
    let home = agent.success(
        "ledger_setup_book_create",
        &json!({
            "target":target("home", "create"), "name":"Home", "settings":settings
        }),
    )?;
    assert_eq!(home["profile"]["company"], "home");
    assert_catalog_and_profile_undo(&mut agent, &mut owner, &settings)?;
    let (account, opening, imported) = post_and_import(&mut agent, &path, &passfile, token)?;

    let listed = agent.request("tools/list", &json!({}))?;
    assert!(!tool_names(&listed)?.contains("audit_undo"));
    let receipt = imported["receipt_id"].to_string();
    cli(&path, &passfile, token)?
        .args(["--company", "home", "audit", "undo", &receipt])
        .assert()
        .code(7);
    let reverted = undo(&mut owner, "home", &imported)?;
    assert_eq!(reverted["child_receipts"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        undo(&mut owner, "home", &opening)?["child_receipts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        undo(&mut owner, "home", &account)?["child_receipts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        payload(&owner.call("audit_undo", &undo_args("home", &home))?)?["code"],
        "UNDO_INELIGIBLE"
    );
    let reversals = owner.success("audit_log", &json!({"company":"home","action":"reverse"}))?;
    let reversals = reversals.as_array().ok_or("missing reversals")?;
    assert_eq!(reversals.len(), 3);
    let preexisting = imported["preexisting"][0]["transaction_id"].to_string();
    assert!(
        reversals
            .iter()
            .all(|event| event["entity_id"] != preexisting)
    );
    owner.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn existing_propose_only_bookkeeper_can_onboard_and_owner_can_approve() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("propose-onboarding.db");
    let passfile = dir.path().join("passphrase");
    identity_ledger(&path, &passfile)?;
    let passphrase = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let owner = proposal_identity(
        &db,
        "owner",
        &["ledger/admin", "ledger/read", "ledger/approve"],
    )?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    let issued = cli(&path, &passfile, owner_token)?
        .args([
            "agent",
            "issue",
            "--name",
            "managed_proposer",
            "--generate-key",
            "--caps",
            "ledger/read,ledger/post,ledger/propose",
            "--write-mode",
            "propose-only",
        ])
        .assert()
        .success();
    let credential = serde_json::from_slice::<Value>(&issued.get_output().stdout)?;
    assert_eq!(
        credential["data"]["capabilities"],
        json!([
            "ledger/read",
            "ledger/propose",
            "ledger/propose/propose-only"
        ])
    );
    let token = credential["data"]["token"]
        .as_str()
        .ok_or("missing bookkeeper token")?;
    cli(&path, &passfile, owner_token)?
        .args(["policy", "set", "--enforce", "on"])
        .assert()
        .success();
    let mut reviewer = identity_client(&path, &passfile, Some(owner_token), false)?;
    let mut agent = identity_client(&path, &passfile, Some(token), false)?;
    assert_tool_access(&mut agent, true)?;

    let settings: Value = serde_json::from_str(SETTINGS)?;
    let home = agent.success(
        "ledger_setup_book_create",
        &json!({
            "target": target("home", "create"), "name": "Home", "settings": settings
        }),
    )?;
    assert_eq!(home["profile"]["company"], "home");
    assert_catalog_and_profile_undo(&mut agent, &mut reviewer, &settings)?;
    let account = agent.success("ledger_setup_account_add", &json!({
        "target": target("home", "bank"), "code": "1234", "name": "Savings", "account_type": "asset"
    }))?;
    assert_eq!(account["name"], "Savings");
    assert!(account["receipt_id"].is_i64());
    assert_proposed_opening(&mut agent, &mut reviewer, cli(&path, &passfile, token)?)?;
    assert_proposed_import(&mut agent, &mut reviewer, cli(&path, &passfile, token)?)?;
    reviewer.success("verify", &json!({}))?;
    Ok(())
}

fn assert_proposed_opening(agent: &mut Client, reviewer: &mut Client, cli: Command) -> TestResult {
    let company = json!({"company": "home"});
    let opened = agent.success("ledger_setup_opening_post", &opening())?;
    assert!(opened["transaction_id"].is_null());
    assert_eq!(opening_from_cli(cli)?, opened);
    assert_eq!(agent.success("txn_list", &company)?, json!([]));
    let pending = agent.success("txn_proposals", &company)?;
    assert_eq!(pending[0]["id"], opened["proposal_id"]);
    assert_eq!(pending[0]["status"], "pending");
    let receipts = reviewer.success("audit_receipts", &company)?;
    let receipts = receipts["receipts"].as_array().ok_or("missing receipts")?;
    assert!(receipts.iter().any(|receipt| {
        receipt["id"] == opened["receipt_id"]
            && receipt["change"]["summary"]["kind"] == "onboarding_proposed"
    }));
    assert_eq!(
        payload(&reviewer.call("audit_undo", &undo_args("home", &opened))?)?["code"],
        "UNDO_UNSUPPORTED"
    );
    let approval = json!({"company":"home", "id":opened["proposal_id"]});
    let denied = agent.call("txn_approve", &approval)?;
    assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    let approved = reviewer.success("txn_approve", &approval)?;
    assert_eq!(approved["status"], "approved");
    assert!(approved["transaction_id"].is_i64());
    assert_eq!(
        agent.success("ledger_setup_opening_post", &opening())?,
        opened
    );
    Ok(())
}

fn assert_proposed_import(agent: &mut Client, reviewer: &mut Client, cli: Command) -> TestResult {
    let company = json!({"company": "home"});
    let first = agent.success("ledger_setup_statement_import", &import("first", FIRST_CSV))?;
    let first_id = &first["proposals"][0]["proposal_id"];
    let approved = reviewer.success("txn_approve", &json!({"company":"home", "id":first_id}))?;
    let request = import("overlap", OVERLAP_CSV);
    let before = agent.success("txn_list", &company)?;
    let imported = agent.success("ledger_setup_statement_import", &request)?;
    assert!(imported["created"].is_null());
    assert_eq!(
        imported["preexisting"],
        json!([{
            "reference": "bank-first", "transaction_id": approved["transaction_id"]
        }])
    );
    assert_eq!(statement_from_cli(cli)?, imported);
    assert_eq!(agent.success("txn_list", &company)?, before);

    let conflict = import(
        "conflict",
        "date,description,amount,reference\n2026-01-02,Unused,9,new-first\n2026-01-03,Second deposit,3,bank-second\n",
    );
    assert_eq!(
        payload(&agent.call("ledger_setup_statement_import", &conflict)?)?["code"],
        "VALIDATION"
    );
    let pending = agent.success("txn_proposals", &company)?;
    assert_eq!(pending.as_array().map(Vec::len), Some(2));
    let proposals = imported["proposals"]
        .as_array()
        .ok_or("missing import proposals")?;
    assert_eq!(proposals.len(), 2);
    for (proposal, expected) in proposals.iter().zip(["bank-second", "bank-third"]) {
        assert_eq!(proposal["reference"], expected);
        let approval = json!({"company":"home", "id":proposal["proposal_id"]});
        let approved = reviewer.success("txn_approve", &approval)?;
        assert_eq!(approved["status"], "approved");
        assert_eq!(approved["reference"], expected);
    }
    assert_eq!(
        agent
            .success("txn_list", &company)?
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    assert_eq!(
        agent.success("ledger_setup_statement_import", &request)?,
        imported
    );
    assert_eq!(agent.success("txn_proposals", &company)?, json!([]));
    Ok(())
}

fn assert_tool_access(client: &mut Client, allowed: bool) -> TestResult {
    let response = client.request("tools/list", &json!({}))?;
    let tools = tool_names(&response)?;
    for name in [
        "ledger_setup_book_create",
        "ledger_setup_book_initialize",
        "ledger_setup_chart_apply",
        "ledger_setup_account_add",
        "ledger_setup_opening_post",
        "ledger_setup_statement_import",
    ] {
        assert_eq!(tools.contains(name), allowed, "{name}");
    }
    if allowed {
        assert!(!tools.contains("ledger_setup_begin"));
        assert!(!tools.contains("ledger_setup_opening_reserve"));
    }
    Ok(())
}

fn assert_catalog_and_profile_undo(
    agent: &mut Client,
    owner: &mut Client,
    settings: &Value,
) -> TestResult {
    let profile = agent.success(
        "ledger_setup_book_initialize",
        &json!({
            "target":target("acme","initialize"), "settings":settings,
        }),
    )?;
    undo(owner, "acme", &profile)?;
    assert!(
        agent.success("ledger_setup_book_show", &json!({"company":"acme"}))?["profile"].is_null()
    );
    agent.success(
        "ledger_setup_book_initialize",
        &json!({
            "target":target("acme","initialize-again"), "settings":settings,
        }),
    )?;

    let empty = agent.success(
        "ledger_setup_book_create",
        &json!({
            "target":target("empty","create"), "name":"Empty", "settings":settings,
        }),
    )?;
    let empty_chart = agent.success(
        "ledger_setup_chart_apply",
        &json!({
            "target":target("empty","chart"), "template":"personal-v1"
        }),
    )?;
    assert_eq!(
        undo(owner, "empty", &empty_chart)?["child_receipts"]
            .as_array()
            .map(Vec::len),
        Some(5)
    );
    assert_eq!(undo(owner, "empty", &empty)?["book_archived"], true);
    let companies = agent.success("company_list", &json!({}))?;
    assert!(
        companies
            .as_array()
            .ok_or("missing companies")?
            .iter()
            .all(|book| book["slug"] != "empty")
    );

    let chart = agent.success(
        "ledger_setup_chart_apply",
        &json!({
            "target":target("home","chart"), "template":"personal-v1"
        }),
    )?;
    assert_eq!(chart["accounts"].as_array().map(Vec::len), Some(5));
    assert!(chart["receipt_id"].is_i64());
    Ok(())
}

fn post_and_import(
    agent: &mut Client,
    path: &Path,
    passfile: &Path,
    token: &str,
) -> Result<(Value, Value, Value), Box<dyn Error>> {
    let account = agent.success("ledger_setup_account_add", &json!({
        "target":target("home","bank"), "code":"1234", "name":" Savings ", "account_type":"asset"
    }))?;
    assert_eq!(account["name"], "Savings");
    let mut unbalanced = opening();
    unbalanced["credits"][0]["amount"] = json!("99");
    assert_eq!(
        payload(&agent.call("ledger_setup_opening_post", &unbalanced)?)?["code"],
        "VALIDATION"
    );
    let opened = agent.success("ledger_setup_opening_post", &opening())?;
    assert_eq!(opening_from_cli(cli(path, passfile, token)?)?, opened);

    let first = agent.success("ledger_setup_statement_import", &import("first", FIRST_CSV))?;
    let imported = agent.success(
        "ledger_setup_statement_import",
        &import("overlap", OVERLAP_CSV),
    )?;
    assert_eq!(imported["created"].as_array().map(Vec::len), Some(2));
    assert_eq!(imported["preexisting"][0], first["created"][0]);
    assert_eq!(statement_from_cli(cli(path, passfile, token)?)?, imported);
    assert_eq!(
        payload(&agent.call(
            "ledger_setup_statement_import",
            &import("overlap", FIRST_CSV)
        )?)?["code"],
        "VALIDATION"
    );
    assert_conflicting_import_rolls_back(agent)?;
    assert_ofx_import(agent)?;
    Ok((account, opened, imported))
}

fn opening_from_cli(mut command: Command) -> Result<Value, Box<dyn Error>> {
    let retry = command
        .args([
            "setup",
            "opening-post",
            "home",
            "--reference",
            "opening",
            "--date",
            "2026-01-01",
            "--currency",
            "USD",
            "--debit",
            "1234:100",
            "--credit",
            "3000:100",
        ])
        .assert()
        .success();
    Ok(serde_json::from_slice::<Value>(&retry.get_output().stdout)?["data"].clone())
}

fn statement_from_cli(mut command: Command) -> Result<Value, Box<dyn Error>> {
    command.args([
        "setup",
        "statement-import",
        "home",
        "--reference",
        "overlap",
        "--bank",
        "1234",
        "--suspense",
        "5000",
        "--currency",
        "USD",
        "--statement-format",
        "csv",
        "--reference-col",
        "reference",
    ]);
    let retry = assert_cmd::Command::from_std(command)
        .write_stdin(OVERLAP_CSV)
        .assert()
        .success();
    Ok(serde_json::from_slice::<Value>(&retry.get_output().stdout)?["data"].clone())
}

fn assert_conflicting_import_rolls_back(agent: &mut Client) -> TestResult {
    let before = agent.success("txn_list", &json!({"company":"home"}))?;
    let input = import(
        "conflict",
        "date,description,amount,reference
2026-01-02,Unused,9,new-first
2026-01-02,Changed,12,bank-first
",
    );
    let failure = payload(&agent.call("ledger_setup_statement_import", &input)?)?;
    assert_eq!(failure["code"], "VALIDATION");
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|message| message.contains("bank-first"))
    );
    assert_eq!(
        agent.success("txn_list", &json!({"company":"home"}))?,
        before
    );
    Ok(())
}

fn assert_ofx_import(agent: &mut Client) -> TestResult {
    let content = concat!(
        "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\n",
        "ENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n",
        "<OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS>",
        "<DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>",
        "<BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>",
        "<STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>123<ACCTID>456<ACCTTYPE>CHECKING</BANKACCTFROM>",
        "<BANKTRANLIST><DTSTART>20260101<DTEND>20260131",
        "<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260105<TRNAMT>10<FITID>NEW-OFX",
        "<NAME>OFX deposit</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>10<DTASOF>20260131",
        "</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>",
    );
    let receipt = agent.success(
        "ledger_setup_statement_import",
        &json!({
            "company":"home", "reference":"ofx", "bank":"1234", "suspense":"5000",
            "currency":"USD", "format":"ofx", "content":content,
        }),
    )?;
    assert_eq!(receipt["format"], "ofx");
    assert!(receipt["created"][0]["transaction_id"].is_i64());
    assert_eq!(receipt["preexisting"], json!([]));
    Ok(())
}
