use beankeeper_cli::identity::{IssuedCredential, WriteMode};
use secrecy::ExposeSecret as _;

use super::{
    Admin, Granted, IssueRequest, Ledger, Server, TestResult, Value, identity, json, payload,
    posting, token,
};
use std::time::Duration;

fn call(server: &mut Server, bearer: &str, name: &str, arguments: Value) -> TestResult<Value> {
    let response = server.call(name, arguments, Some(bearer))?;
    assert_ne!(response["result"]["isError"], true, "{name}: {response}");
    payload(&response)
}

fn fails(
    server: &mut Server,
    bearer: &str,
    name: &str,
    arguments: Value,
    code: &str,
) -> TestResult<Value> {
    let response = server.call(name, arguments, Some(bearer))?;
    assert_eq!(response["result"]["isError"], true, "{name}: {response}");
    let data = payload(&response)?;
    assert_eq!(data["code"], code, "{data}");
    Ok(data)
}

fn clear(reference: &str) -> Value {
    json!({
        "company": "acme",
        "account": "1000",
        "reference": reference,
        "date": "2026-11-30",
        "statement": "NOV",
    })
}

fn statement(balance: &str) -> Value {
    json!({
        "company": "acme",
        "account": "1000",
        "period_end": "2026-11-30",
        "statement_ending_balance": balance,
        "statement_ref": "NOV",
    })
}

fn issue(
    ledger: &Ledger,
    name: &str,
    capability: &str,
    mode: WriteMode,
) -> TestResult<IssuedCredential> {
    let db = ledger.db()?;
    Ok(Granted::<Admin>::with_db(&db, &ledger.owner, |grant| {
        identity::issue_credential_with_mode(
            grant,
            &IssueRequest {
                name,
                capabilities: &["ledger/read".into(), capability.into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
            mode,
        )
    })?)
}

#[test]
fn signed_direct_agent_can_import_clear_and_reconcile_a_month() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enforce()?;
    let direct = token(&ledger.writer)?;
    let read = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(direct), "Standard agent")?;
    for tool in ["txn_clear", "reconcile_statement"] {
        assert!(server.tools(Some(direct))?.iter().any(|name| name == tool));
        assert!(!server.tools(Some(read))?.iter().any(|name| name == tool));
    }
    fails(&mut server, read, "txn_clear", clear("nov"), "FORBIDDEN")?;
    fails(
        &mut server,
        read,
        "reconcile_statement",
        statement("12.50"),
        "FORBIDDEN",
    )?;
    let imported = call(
        &mut server,
        direct,
        "txn_import",
        json!({
            "company":"acme",
            "transactions":[{
                "date":"2026-11-20","description":"Synthetic settlement", "currency":"USD",
                "reference":"nov",
                "entries":[
                    {"account_code":"1000", "direction":"debit", "amount":"12.50"},
                    {"account_code":"4000", "direction":"credit", "amount":"12.50"}
                ]
            }]
        }),
    )?;
    assert_eq!(imported["imported"], 1);
    assert_eq!(
        call(&mut server, direct, "txn_clear", clear("nov"))?["action"],
        "cleared"
    );
    let wrong = fails(
        &mut server,
        direct,
        "reconcile_statement",
        statement("11.00"),
        "STATEMENT_DISCREPANCY",
    )?;
    assert_eq!(wrong["details"]["delta"], 150);
    let mut close = statement("12.50");
    close["close_period"] = json!(true);
    fails(
        &mut server,
        direct,
        "reconcile_statement",
        close,
        "FORBIDDEN",
    )?;

    let result = call(
        &mut server,
        direct,
        "reconcile_statement",
        statement("12.50"),
    )?;
    assert_eq!(result["statement_ending_balance"], 1250);
    assert_eq!(result["matched"][0]["signed_amount"], 1250);
    assert!(result["receipt"].as_i64().is_some_and(|id| id > 0));
    assert_eq!(result["period_close"], Value::Null);

    let readback = ledger
        .command()
        .env("BK_AGENT_TOKEN", direct)
        .args(["--json", "--company", "acme", "reconcile", "list"])
        .output()?;
    assert!(readback.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&readback.stdout)?["data"]["statements"][0],
        result,
    );
    server.finish(&[direct, read])
}

#[test]
fn approval_mode_freezes_clearings_and_statement_totals() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = issue(
        &ledger,
        "statement_agent",
        "ledger/propose",
        WriteMode::ProposeOnly,
    )?;
    let reviewer = issue(
        &ledger,
        "statement_reviewer",
        "ledger/approve",
        WriteMode::Direct,
    )?;
    ledger.enforce()?;

    let propose = agent.token().expose_secret();
    let approve = reviewer.token().expose_secret();
    let direct = token(&ledger.writer)?;
    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(propose), "Approval-mode agent")?;
    call(&mut server, direct, "txn_post", posting("queued-line"))?;
    let draft = json!({
        "company":"acme", "account":"1000", "date":"2026-11-30", "statement":"NOV",
        "batch":[{"reference":"queued-line","amount":"12.50"}],
    });
    let pending = call(&mut server, propose, "txn_clear", draft.clone())?;
    let review_clear = json!({"company":"acme", "id":pending["proposal_id"]});
    assert_eq!(pending["status"], "pending");
    assert_eq!(call(&mut server, propose, "txn_clear", draft)?, pending);
    let db = ledger.db()?;
    let clearings = || -> Result<i64, rusqlite::Error> {
        db.read()
            .query_row("SELECT count(*) FROM entry_clearings", [], |row| row.get(0))
    };
    assert_eq!(clearings()?, 0);
    let queue = json!({"company":"acme"});
    let queued = call(&mut server, approve, "txn_proposals", queue.clone())?;
    let change = &queued[0]["payload"]["correction"]["change"];
    assert_eq!(change["action"], "clear_entries");
    assert_eq!(change["entries"][0]["before"], json!([]));
    assert_eq!(change["entries"][0]["after"][0]["amount_minor"], 1250);
    assert_eq!(change["entries"][0]["after"][0]["id"], Value::Null);
    fails(
        &mut server,
        propose,
        "txn_approve",
        review_clear.clone(),
        "FORBIDDEN",
    )?;
    assert_eq!(
        call(&mut server, approve, "txn_approve", review_clear)?["status"],
        "approved"
    );
    assert_eq!(clearings()?, 1);

    let pending = call(
        &mut server,
        propose,
        "reconcile_statement",
        statement("12.50"),
    )?;
    let queued = call(&mut server, approve, "txn_proposals", queue)?;
    let review = &queued[0]["payload"]["correction"]["change"]["statement"];
    assert_eq!(review["statement_ending_balance_minor"], 1250);
    assert_eq!(review["matched"][0]["signed_amount_minor"], 1250);
    assert!(review["matched"][0]["clearing_id"].is_i64());
    assert_eq!(
        db.read().query_row(
            "SELECT count(*) FROM statement_reconciliations",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    let decision = json!({"company":"acme", "id":pending["proposal_id"]});
    let resolved = call(&mut server, approve, "txn_approve", decision.clone())?;
    assert_eq!(resolved["status"], "approved");
    assert_eq!(resolved["result"]["statement_ending_balance"], 1250);
    assert_eq!(
        call(&mut server, approve, "txn_approve", decision)?,
        resolved
    );
    server.finish(&[direct, propose, approve])
}

#[test]
fn approval_refuses_a_changed_clearing_basis_and_leaves_the_books_untouched() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = issue(
        &ledger,
        "statement_agent",
        "ledger/propose",
        WriteMode::ProposeOnly,
    )?;
    let reviewer = issue(
        &ledger,
        "statement_reviewer",
        "ledger/approve",
        WriteMode::Direct,
    )?;
    ledger.enforce()?;
    let propose = agent.token().expose_secret();
    let approve = reviewer.token().expose_secret();
    let direct = token(&ledger.writer)?;

    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(propose), "Approval-mode agent")?;
    call(&mut server, direct, "txn_post", posting("changed"))?;
    let pending = call(&mut server, propose, "txn_clear", clear("changed"))?;
    let mut partial = clear("changed");
    partial["statement"] = json!("OTHER");
    partial["amount"] = json!("2.50");
    call(&mut server, direct, "txn_clear", partial)?;
    let decision = json!({"company":"acme", "id":pending["proposal_id"]});
    let stale = fails(&mut server, approve, "txn_approve", decision, "VALIDATION")?;
    assert!(
        stale["message"]
            .as_str()
            .is_some_and(|message| message.contains("submit a new proposal")),
        "{stale}",
    );
    let db = ledger.db()?;
    let count: i64 = db
        .read()
        .query_row("SELECT count(*) FROM entry_clearings", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    server.finish(&[direct, propose, approve])
}

#[test]
fn approval_rejects_a_different_set_of_matches_with_the_same_statement_balance() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = issue(
        &ledger,
        "statement_agent",
        "ledger/propose",
        WriteMode::ProposeOnly,
    )?;
    let reviewer = issue(
        &ledger,
        "statement_reviewer",
        "ledger/approve",
        WriteMode::Direct,
    )?;
    ledger.enforce()?;
    let propose = agent.token().expose_secret();
    let approve = reviewer.token().expose_secret();
    let direct = token(&ledger.writer)?;

    let mut server = Server::start(&ledger, false, None)?;
    server.initialize(Some(direct), "Approval-mode agent")?;
    for reference in ["old-match", "new-match"] {
        call(&mut server, direct, "txn_post", posting(reference))?;
    }
    call(&mut server, direct, "txn_clear", clear("old-match"))?;
    let pending = call(
        &mut server,
        propose,
        "reconcile_statement",
        statement("12.50"),
    )?;

    let mut remove = clear("old-match");
    remove["status"] = json!("uncleared");
    call(&mut server, direct, "txn_clear", remove)?;
    call(&mut server, direct, "txn_clear", clear("new-match"))?;
    let decision = json!({"company":"acme", "id":pending["proposal_id"]});
    let stale = fails(&mut server, approve, "txn_approve", decision, "VALIDATION")?;
    assert!(
        stale["message"]
            .as_str()
            .is_some_and(|message| message.contains("submit a new proposal")),
        "{stale}",
    );
    let db = ledger.db()?;
    let count: i64 = db.read().query_row(
        "SELECT count(*) FROM statement_reconciliations",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);
    server.finish(&[direct, propose, approve])
}
