use super::*;

#[test]
fn mcp_classification_and_cli_share_cash_totals() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for (code, name, class) in [
        ("1020", "Travel wallet", "cash"),
        ("1030", "Money-market fund", "cash_equivalent"),
    ] {
        client.success(
            "account_create",
            &json!({
                "company":"acme", "code":code, "name":name, "account_type":"asset"
            }),
        )?;
        client.success(
            "account_classify",
            &json!({
                "company":"acme", "code":code, "cash_class":class
            }),
        )?;
        bk(&path)?
            .args([
                "--company",
                "acme",
                "txn",
                "post",
                "-d",
                "Cash class fixture",
                "--date",
                "2025-02-12",
                "--debit",
                &format!("{code}:50"),
                "--credit",
                "4000:50",
            ])
            .assert()
            .success();
    }
    let summary = client.success(
        "financial_summary",
        &json!({"company":"acme", "to":"2025-03-31"}),
    )?;
    assert_eq!(summary["data"]["standing"]["cash_on_hand"], 10_000);
    let report = client.success(
        "report",
        &json!({
            "company":"acme", "kind":"cash-flow", "from":"2025-02-01", "to":"2025-03-31"
        }),
    )?;
    assert_eq!(report["closing_cash"], 10_000);
    let output = bk(&path)?
        .args([
            "--company",
            "acme",
            "--json",
            "report",
            "cash-flow",
            "--from",
            "2025-02-01",
            "--to",
            "2025-03-31",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report, cli["data"]);
    for (code, class) in [("4000", "cash"), ("1020", "liquid")] {
        let result = client.call(
            "account_classify",
            &json!({
                "company":"acme", "code":code, "cash_class":class
            }),
        )?;
        assert_eq!(result["result"]["isError"], true, "{result}");
    }
    Ok(())
}

#[test]
fn signed_owner_undo_restores_classification_with_a_new_audited_entry() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    let passphrase_file = directory.path().join("passphrase");
    identity_ledger(&path, &passphrase_file)?;
    let secret = secrecy::SecretString::from("mcp-test-passphrase");
    let db = beankeeper_cli::db::Db::open_existing(&path, Some(&secret))?;
    let owner = proposal_identity(
        &db,
        "cash_class_owner",
        &["ledger/admin", "ledger/read", "ledger/post"],
    )?;
    let mut client = identity_client(
        &path,
        &passphrase_file,
        Some(owner["token"].as_str().ok_or("owner token")?),
        false,
    )?;
    client.success("txn_post", &posting("classified-cash"))?;
    let summary_input = json!({"company":"acme", "to":"2026-03-31"});
    let before = client.success("financial_summary", &summary_input)?;
    assert_eq!(before["data"]["standing"]["cash_on_hand"], 0);
    let classified = client.success(
        "account_classify",
        &json!({
            "company":"acme", "code":"1000", "cash_class":"cash_equivalent"
        }),
    )?;
    assert_eq!(classified["previous"], "non_cash");
    assert_eq!(classified["cash_class"], "cash_equivalent");
    assert_eq!(
        client.success("financial_summary", &summary_input)?["data"]["standing"]["cash_on_hand"],
        1250
    );
    client.success(
        "account_rename",
        &json!({
            "company":"acme", "code":"1000", "name":"Travel reserve"
        }),
    )?;
    assert_eq!(
        client.success("financial_summary", &summary_input)?["data"]["standing"]["cash_on_hand"],
        1250
    );
    let undo_input = json!({"company":"acme", "receipt":classified["receipt_id"]});
    let undone = client.success("audit_undo", &undo_input)?;
    assert_eq!(undone["kind"], "account_classification_restored");
    assert_eq!(undone["cash_class"], "non_cash");
    assert_ne!(undone["undo_receipt"], classified["receipt_id"]);
    assert_eq!(
        client.success("financial_summary", &summary_input)?["data"]["standing"]["cash_on_hand"],
        0
    );
    assert_eq!(client.success("audit_undo", &undo_input)?, undone);
    let audit = client.success(
        "audit_log",
        &json!({"company":"acme", "action":"account_classify"}),
    )?;
    assert_eq!(audit.as_array().map(Vec::len), Some(2));
    client.success("verify", &json!({}))?;
    Ok(())
}
