use super::*;

fn profile(path: &Path, currency: &str, passphrase: Option<&str>) -> TestResult {
    let db = rusqlite::Connection::open(path)?;
    if let Some(passphrase) = passphrase {
        db.pragma_update(None, "key", passphrase)?;
    }
    db.execute(
        "INSERT INTO setup_book_profiles(company_slug, purpose, base_currency, fiscal_year_start)
         VALUES ('acme', 'personal', ?1, '01-01')",
        [currency],
    )?;
    Ok(())
}

fn data(mut command: Command, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = command
        .args(["--json", "--company", "acme"])
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    Ok(response["data"].clone())
}

fn create(client: &mut Client, code: &str, currency: &str, kind: &str) -> TestResult {
    client.success(
        "account_create",
        &json!({
            "company":"acme", "code":code, "name":format!("{currency} {kind}"),
            "account_type":kind, "native_currency":currency
        }),
    )?;
    Ok(())
}

fn movement(account: &str, direction: &str, minor: i64, rate: &str) -> Value {
    json!({
        "company":"acme", "account":account, "offset_account":"1000",
        "direction":direction, "native_minor":minor, "rate":rate,
        "date":"2026-01-01", "description":"Foreign balance", "reference":format!("fx-{account}")
    })
}

fn revaluation(account: &str, date: &str, rate: &str) -> Value {
    json!({
        "company":"acme", "account":account, "fx_gain_loss_account":"4000",
        "as_of":date, "rate":rate
    })
}

fn balance(client: &mut Client, account: &str) -> Result<Value, Box<dyn Error>> {
    let accounts = client.success(
        "account_list",
        &json!({
            "company":"acme", "with_balances":true
        }),
    )?;
    Ok(accounts
        .as_array()
        .ok_or("expected account list")?
        .iter()
        .find(|row| row["code"] == account)
        .ok_or("expected account")?["fx"]
        .clone())
}

fn assert_cli_changes(cli_path: &Path, posted: &Value, marked: &Value) -> TestResult {
    data(
        bk(cli_path)?,
        &[
            "account",
            "create",
            "1100",
            "EUR asset",
            "--type",
            "asset",
            "--native-currency",
            "EUR",
        ],
    )?;

    let cli = data(
        bk(cli_path)?,
        &[
            "fx",
            "post",
            "--account",
            "1100",
            "--offset-account",
            "1000",
            "--direction",
            "debit",
            "--native-minor",
            "10000",
            "--rate",
            "0.8",
            "--date",
            "2026-01-01",
            "--description",
            "Foreign balance",
            "--reference",
            "fx-1100",
        ],
    )?;
    assert_eq!(
        posted["postings"][0]["functional_change"],
        cli["postings"][0]["functional_change"]
    );
    assert_eq!(
        posted["postings"][0]["functional_change"]["minor_units"],
        8000
    );

    let cli_marked = data(
        bk(cli_path)?,
        &[
            "fx",
            "revalue",
            "--account",
            "1100",
            "--fx-gain-loss-account",
            "4000",
            "--as-of",
            "2026-01-31",
            "--rate",
            "0.85",
        ],
    )?;
    assert_eq!(
        marked["postings"][0]["functional_change"],
        cli_marked["postings"][0]["functional_change"]
    );
    Ok(())
}

#[test]
fn fx_cli_mcp_balance_and_audit_undo_agree() -> TestResult {
    let temp = tempdir()?;
    let cli_path = temp.path().join("cli.db");
    seed(&cli_path)?;
    profile(&cli_path, "GBP", None)?;

    let mcp_path = temp.path().join("mcp.db");
    let key_file = temp.path().join("passphrase");
    identity_ledger(&mcp_path, &key_file)?;
    profile(&mcp_path, "GBP", Some("mcp-test-passphrase"))?;
    let db = beankeeper_cli::db::Db::open_existing(&mcp_path, Some(&"mcp-test-passphrase".into()))?;
    let owner = proposal_identity(&db, "fx_owner", &["ledger/admin", "ledger/read"])?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    let mut mcp = identity_client(&mcp_path, &key_file, None, false)?;
    let mut signed = identity_client(&mcp_path, &key_file, Some(owner_token), false)?;
    create(&mut mcp, "1100", "EUR", "asset")?;
    let wrong_currency = mcp.call("txn_post", &posting("wrong-currency"))?;
    assert_eq!(payload(&wrong_currency)?["code"], "FX_INVALID");
    let posted = mcp.success("fx_post", &movement("1100", "debit", 10_000, "0.8"))?;
    let input = revaluation("1100", "2026-01-31", "0.85");
    let marked = mcp.success("fx_revalue", &input)?;
    assert_cli_changes(&cli_path, &posted, &marked)?;
    let receipt = &marked["postings"][0]["undo_receipt"];
    assert!(receipt.is_number());
    assert!(marked["postings"][0]["desktop_path"].as_str().is_some());

    let current = balance(&mut mcp, "1100")?;
    assert_eq!(current["native_balance"]["minor_units"], 10000);
    assert_eq!(current["functional_balance"]["minor_units"], 8500);
    assert_eq!(current["last_revaluation"]["rate"], "0.85");
    assert_eq!(current["last_revaluation"]["date"], "2026-01-31");
    let sheet = data(
        bk(&cli_path)?,
        &["report", "balance-sheet", "--as-of", "2026-01-31"],
    )?;
    assert_eq!(
        sheet["fx_balances"][0]["functional_balance"],
        current["functional_balance"]
    );
    assert_eq!(sheet["balanced"], true);
    let summary = data(
        bk(&cli_path)?,
        &["summary", "--from", "2026-01-01", "--to", "2026-01-31"],
    )?;
    assert_eq!(
        summary["fx_balances"][0]["native_balance"],
        current["native_balance"]
    );

    let repeat = mcp.success("fx_revalue", &input)?;
    assert!(repeat["postings"][0]["undo_receipt"].is_null());
    let undone = signed.success("audit_undo", &json!({"company":"acme", "receipt":receipt}))?;
    assert_eq!(undone["kind"], "reversed");
    let restored = balance(&mut mcp, "1100")?;
    assert_eq!(restored["native_balance"]["minor_units"], 10000);
    assert_eq!(restored["functional_balance"]["minor_units"], 8000);
    assert!(restored["last_revaluation"].is_null());
    mcp.success("verify", &json!({}))?;
    Ok(())
}

#[test]
fn fx_uses_each_pair_scale_and_honors_closed_periods() -> TestResult {
    let temp = tempdir()?;
    let path = temp.path().join("book.db");
    seed(&path)?;
    profile(&path, "KWD", None)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    create(&mut client, "2100", "JPY", "liability")?;
    create(&mut client, "1100", "EUR", "asset")?;
    client.success("fx_post", &movement("2100", "credit", 3, "0.0005"))?;
    client.success("fx_post", &movement("1100", "debit", 100, "0.3"))?;
    let fx = client.success("fx_revalue", &revaluation("2100", "2026-01-31", "0.0015"))?;
    assert_eq!(fx["postings"][0]["functional_change"]["minor_units"], 2);
    let position = balance(&mut client, "2100")?;
    assert_eq!(position["native_balance"]["minor_units"], 3);
    assert_eq!(position["functional_balance"]["minor_units"], 4);
    assert_eq!(position["functional_balance"]["currency"], "KWD");
    // All accounts can reuse a distinct latest rate for each pair.
    let all = client.success(
        "fx_revalue",
        &json!({
            "company":"acme", "as_of":"2026-02-01", "fx_gain_loss_account":"4000"
        }),
    )?;
    assert_eq!(all["postings"][0]["rate"], "0.3");
    assert_eq!(all["postings"][1]["rate"], "0.0015");
    assert!(all["postings"][1]["transaction_id"].is_null());

    bk(&path)?
        .args([
            "--company",
            "acme",
            "period",
            "close",
            "--through",
            "2026-01-31",
        ])
        .assert()
        .success();
    let error = client.call("fx_revalue", &revaluation("2100", "2026-01-31", "0.01"))?;
    assert_eq!(error["result"]["isError"], true, "{error}");
    assert_eq!(payload(&error)?["code"], "VALIDATION");
    assert_eq!(
        balance(&mut client, "2100")?["functional_balance"]["minor_units"],
        4
    );
    Ok(())
}

#[test]
fn fx_schema_upgrade_preserves_historical_postings() -> TestResult {
    let temp = tempdir()?;
    let path = temp.path().join("legacy.db");
    seed(&path)?;
    let expected = data(
        bk(&path)?,
        &[
            "txn",
            "post",
            "--description",
            "Legacy",
            "--currency",
            "MXN",
            "--debit",
            "1000:8.53",
            "--credit",
            "4000:8.53",
            "--date",
            "2026-01-01",
        ],
    )?;
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch("DROP TABLE fx_postings; DROP TABLE fx_accounts;")?;
    let preceding = i64::from(beankeeper_cli::db::current_schema_version()) - 1;
    conn.execute("DELETE FROM schema_version WHERE version > ?1", [preceding])?;
    drop(conn);

    let actual = data(bk(&path)?, &["txn", "show", &expected["id"].to_string()])?;
    assert_eq!(actual[0]["currency"], "MXN");
    bk(&path)?.args(["verify"]).assert().success();
    Ok(())
}
