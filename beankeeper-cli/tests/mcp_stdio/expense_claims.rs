use super::*;
use base64::prelude::{BASE64_STANDARD, Engine as _};

#[test]
fn claim_review_accepts_only_an_independent_mcp_actor() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["--actor", "submitter", "mcp", "serve"]);
    let mut submitter = Client::spawn(command)?;
    submitter.initialize()?;

    for (code, name, account_type) in [
        ("2100", "Reimbursements", "liability"),
        ("5100", "Supplies", "expense"),
    ] {
        submitter.success(
            "account_create",
            &json!({"company":"acme","code":code,"name":name,"account_type":account_type}),
        )?;
    }
    let party = submitter.success(
        "counterparty_create",
        &json!({"company":"acme","name":"Synthetic staff member","role":"vendor"}),
    )?;
    let receipt=submitter.success("expense_claim_receipt_intake",&json!({
        "company":"acme","filename":"synthetic.pdf","content_base64":BASE64_STANDARD.encode(b"%PDF-1.7\n%%EOF")
    }))?;
    let draft=submitter.success("expense_claim_create",&json!({
        "company":"acme","reference":"draft",
        "terms":{"claimant":party["party"]["id"],"receipt":receipt["id"],"merchant":"Fictional supply shop",
            "incurred_on":"2026-02-01","currency":"USD","amount_minor":1200,
            "splits":[{"account":"5100","amount_minor":1200}],
            "funding":{"kind":"reimbursable","payable":"2100"}}
    }))?;
    let id = draft["id"].as_str().ok_or("claim id")?;
    let frozen = submitter.success(
        "expense_claim_submit",
        &json!({
            "company":"acme","id":id,"revision":1,"reference":"submitted"
        }),
    )?;
    assert_eq!(frozen["status"], "submitted");
    let approval = json!({"company":"acme","id":id,"revision":2,"reference":"approved"});
    assert_eq!(
        submitter.call("expense_claim_approve", &approval)?["result"]["isError"],
        true
    );

    let mut reviewer = bk(&path)?;
    reviewer.args(["--actor", "reviewer", "mcp", "serve"]);
    let mut reviewer = Client::spawn(reviewer)?;
    reviewer.initialize()?;
    let approved = reviewer.success("expense_claim_approve", &approval)?;
    assert_eq!(approved["status"], "approved");

    let posted=reviewer.success("txn_post",&json!({
        "company":"acme","description":"Reimburse synthetic claim","date":"2026-02-03","currency":"USD",
        "debits":[{"account":"2100","amount":"12.00"}],
        "credits":[{"account":"1000","amount":"12.00"}]
    }))?;
    let payment = reviewer.success(
        "expense_claim_record_payment",
        &json!({
            "company":"acme","id":id,"revision":3,"reference":"applied","transaction":posted["id"]
        }),
    )?;
    assert_eq!(payment["status"], "reimbursed");
    Ok(())
}
