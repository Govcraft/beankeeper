use super::*;

fn issue_proposer(path: &Path, passphrase: &Path) -> Result<Value, Box<dyn Error>> {
    let output = bk(path)?
        .arg("--passphrase-file")
        .arg(passphrase)
        .args([
            "--json",
            "agent",
            "issue",
            "--name",
            "document_proposer",
            "--generate-key",
            "--caps",
            "ledger/read,ledger/propose,ledger/post",
            "--write-mode",
            "propose-only",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    Ok(value["data"].clone())
}

#[test]
fn document_approval_freezes_terms_and_retry_reuses_the_posting() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("documents.db");
    let passphrase = directory.path().join("passphrase");
    identity_ledger(&path, &passphrase)?;
    let db = beankeeper_cli::db::Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
    let owner = proposal_identity(
        &db,
        "doc_owner",
        &[
            "ledger/read",
            "ledger/post",
            "ledger/admin",
            "ledger/approve",
        ],
    )?;
    let proposer = issue_proposer(&path, &passphrase)?;
    let mut owner = identity_client(&path, &passphrase, owner["token"].as_str(), false)?;
    let mut drafter = identity_client(&path, &passphrase, proposer["token"].as_str(), false)?;
    let counterparty = owner.success(
        "counterparty_create",
        &json!({
            "company":"acme","name":"Clearwater","role":"customer","payment_terms":30,
        }),
    )?;
    let mut terms = json!({
        "kind":"invoice","number":"INV-1","counterparty_id":counterparty["party"]["id"],
        "control_account":"1000",
        "lines":[{"description":"Service","quantity":"1","unit_price_minor":100,"account":"4000"}],
    });
    let draft = drafter.success("document_draft", &json!({"company":"acme","terms":terms}))?;
    let id = &draft["record"]["id"];
    let issue = json!({"company":"acme","id":id,"date":"2026-01-01"});
    let first = drafter.success("document_issue", &issue)?;
    assert_eq!(first["status"], "proposed");
    assert_eq!(drafter.success("document_issue", &issue)?, first);
    assert!(
        drafter.success("document_show", &json!({"company":"acme","id":id}))?["issued"].is_null()
    );
    terms["lines"][0]["unit_price_minor"] = json!(200);
    drafter.success(
        "document_amend",
        &json!({
            "company":"acme","id":id,"revision":draft["record"]["revision"],"terms":terms,
        }),
    )?;
    let stale = owner.call(
        "document_approve",
        &json!({"company":"acme","id":first["proposal_id"]}),
    )?;
    assert_eq!(payload(&stale)?["code"], "VALIDATION");
    let fresh = drafter.success("document_issue", &issue)?;
    let approve = json!({"company":"acme","id":fresh["proposal_id"]});
    let issued = owner.success("document_approve", &approve)?;
    let retry = owner.success("document_approve", &approve)?;
    assert_eq!(issued, retry);
    assert_eq!(
        issued["resolution"]["result"]["record"]["issued"]["total_minor"],
        200
    );
    let entries = owner.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(entries.as_array().map(Vec::len), Some(1));
    owner.success("verify", &json!({}))?;
    Ok(())
}
