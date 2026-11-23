use super::{Client, TestResult, bk, identity_client, proposal_identity, seed, tempdir};

use agent_uri_attestation::SigningKey;
use beankeeper_cli::{
    db::{Actor, Db},
    identity::{Admin, Granted, init_root},
};
use serde_json::{Value, json};
use std::{error::Error, path::Path, process::Command};

type JsonData = Result<Value, Box<dyn Error>>;
type JsonCli<'a> = dyn Fn(&[&str]) -> JsonData + 'a;

fn data(command: &mut Command) -> JsonData {
    let result = command.output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(serde_json::from_slice::<Value>(&result.stdout)?["data"].clone())
}

fn json_cli(path: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = bk(path)?;
    command.args(["--json", "--actor", "parity"]);
    Ok(command)
}

fn plain_client(path: &Path) -> Result<Client, Box<dyn Error>> {
    let mut command = json_cli(path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

#[test]
fn period_changes_and_listing_match_cli_with_the_same_actor() -> TestResult {
    let directory = tempdir()?;
    let cli_path = directory.path().join("cli.db");
    let mcp_path = directory.path().join("mcp.db");
    seed(&cli_path)?;
    seed(&mcp_path)?;
    let mut mcp = plain_client(&mcp_path)?;

    for (args, tool, input) in [
        (
            ["close", "--through", "2026-07-31"],
            "period_close",
            json!({"company":"acme", "through":"2026-07-31"}),
        ),
        (
            ["reopen", "--reason", "  July adjustment  "],
            "period_reopen",
            json!({"company":"acme", "reason":"  July adjustment  "}),
        ),
    ] {
        let expected = data(
            json_cli(&cli_path)?
                .args(["--company", "acme", "period"])
                .args(args),
        )?;
        let result = mcp.success(tool, &input)?;
        assert_eq!(
            normalize_period(expected)?,
            normalize_period(result.clone())?
        );
        assert_eq!(
            mcp.success("period_list", &json!({"company":"acme"}))?,
            result,
        );
        assert_eq!(
            data(json_cli(&mcp_path)?.args(["--company", "acme", "period", "list"]))?,
            result,
        );
    }
    Ok(())
}

fn normalize_period(mut outcome: Value) -> Result<Value, Box<dyn Error>> {
    let changed = outcome[0]["changed_at"]
        .as_str()
        .ok_or("missing period audit timestamp")?;
    chrono::DateTime::parse_from_rfc3339(changed)?;
    outcome[0]["changed_at"] = json!("<change timestamp>");
    Ok(outcome)
}

fn seed_investment(path: &Path) -> TestResult {
    seed(path)?;
    data(json_cli(path)?.args([
        "--company",
        "acme",
        "account",
        "create",
        "1200",
        "Investments",
        "--type",
        "asset",
    ]))?;
    Ok(())
}

#[test]
fn securities_and_investment_operations_match_cli() -> TestResult {
    let directory = tempdir()?;
    let cli_path = directory.path().join("cli.db");
    let mcp_path = directory.path().join("mcp.db");
    seed_investment(&cli_path)?;
    seed_investment(&mcp_path)?;
    let mut mcp = plain_client(&mcp_path)?;
    let cli = |args: &[&str]| data(json_cli(&cli_path)?.args(["--company", "acme"]).args(args));

    let (buy_cli, buy_mcp) = assert_security_purchase_parity(&cli, &mut mcp)?;
    assert_investment_sale_parity(&cli, &mut mcp, &buy_cli, &buy_mcp)
}

fn assert_security_purchase_parity(
    cli: &JsonCli<'_>,
    mcp: &mut Client,
) -> Result<(Value, Value), Box<dyn Error>> {
    let create = cli(&[
        "security",
        "create",
        "TEST",
        "--name",
        "Parity Fund",
        "--currency",
        "BHD",
    ])?;
    assert_eq!(
        mcp.success(
            "security_create",
            &json!({"company":"acme","symbol":"TEST","name":"Parity Fund","currency":"BHD"})
        )?,
        create
    );
    assert_eq!(
        mcp.success("security_list", &json!({"company":"acme"}))?,
        cli(&["security", "list"])?
    );
    let buy_cli = cli(&[
        "investment",
        "buy",
        "TEST",
        "--quantity",
        "1.5",
        "--cost",
        "15.123",
        "--date",
        "2026-01-01",
        "--asset",
        "1200",
        "--cash",
        "1000",
        "--reference",
        "trade-buy",
    ])?;
    let buy_mcp = mcp.success(
        "investment_buy",
        &json!({
            "company":"acme", "symbol":"TEST", "quantity":"1.5", "cost":"15.123",
            "date":"2026-01-01", "asset":"1200", "cash":"1000", "reference":"trade-buy",
        }),
    )?;
    let buy = |mut value: Value| {
        mask_id(&mut value["lot_id"], "lot_")?;
        Ok::<_, Box<dyn Error>>(value)
    };
    assert_eq!(buy(buy_cli.clone())?, buy(buy_mcp.clone())?);

    let mut price_cli = cli(&[
        "security",
        "price",
        "TEST",
        "--date",
        "2026-01-02",
        "--price",
        "12.345",
    ])?;
    let mut price_mcp = mcp.success(
        "security_price",
        &json!({
            "company":"acme", "symbol":"TEST", "date":"2026-01-02", "price":"12.345",
        }),
    )?;
    mask_id(&mut price_cli["id"], "price_")?;
    mask_id(&mut price_mcp["id"], "price_")?;
    assert_eq!(price_mcp, price_cli);

    Ok((buy_cli, buy_mcp))
}

fn assert_investment_sale_parity(
    cli: &JsonCli<'_>,
    mcp: &mut Client,
    buy_cli: &Value,
    buy_mcp: &Value,
) -> TestResult {
    let lot_cli = buy_cli["lot_id"].as_str().ok_or("missing CLI lot")?;
    let lot_mcp = buy_mcp["lot_id"].as_str().ok_or("missing MCP lot")?;
    let sell_cli = cli(&[
        "investment",
        "sell",
        "TEST",
        "--quantity",
        "0.5",
        "--proceeds",
        "7.000",
        "--date",
        "2026-01-03",
        "--asset",
        "1200",
        "--cash",
        "1000",
        "--gain",
        "4000",
        "--method",
        "specific",
        "--lot",
        lot_cli,
    ])?;
    let sell_mcp = mcp.success(
        "investment_sell",
        &json!({
            "company":"acme", "symbol":"TEST", "quantity":"0.5", "proceeds":"7.000",
            "date":"2026-01-03", "asset":"1200", "cash":"1000", "gain":"4000",
            "method":"specific", "lot":lot_mcp,
        }),
    )?;
    let sell = |mut value: Value, lot: &str| {
        assert_eq!(value["relief"][0]["lot_id"], lot);
        mask_id(&mut value["relief"][0]["lot_id"], "lot_")?;
        Ok::<_, Box<dyn Error>>(value)
    };
    assert_eq!(sell(sell_cli, lot_cli)?, sell(sell_mcp, lot_mcp)?);
    let lots = |mut value: Value| {
        mask_id(&mut value[0]["id"], "lot_")?;
        Ok::<_, Box<dyn Error>>(value)
    };
    assert_eq!(
        lots(mcp.success(
            "investment_lots",
            &json!({"company":"acme", "symbol":"TEST"})
        )?)?,
        lots(cli(&["investment", "lots", "TEST"])?)?
    );
    assert_eq!(
        mcp.success(
            "investment_holdings",
            &json!({"company":"acme","as_of":"2026-01-03"})
        )?,
        cli(&["investment", "holdings", "--as-of", "2026-01-03"])?
    );
    Ok(())
}

fn mask_id(value: &mut Value, prefix: &str) -> TestResult {
    let id = value.as_str().ok_or("expected generated ID string")?;
    assert!(id.starts_with(prefix), "expected a {prefix} ID, got {id}");
    *value = json!(prefix);
    Ok(())
}

#[test]
fn setup_book_mutation_receipts_match_cli_on_exact_retries() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    let passphrase = directory.path().join("passphrase");
    std::fs::write(&passphrase, "typed-request-fixture")?;
    let db = Db::open(&path, Some(&"typed-request-fixture".into()))?;
    Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), init_root)?;
    let owner = proposal_identity(&db, "owner", &["ledger/admin"])?;
    let owner_token = owner["token"].as_str().ok_or("missing owner token")?;
    let mut owner_client = identity_client(&path, &passphrase, Some(owner_token), false)?;
    let key = SigningKey::generate().verifying_key().to_base64();
    let started = owner_client.success(
        "ledger_setup_begin",
        &json!({"name":"setup", "public_key":key}),
    )?;
    let setup_token = started["credential"]["token"]
        .as_str()
        .ok_or("missing setup token")?;
    let generation = started["session"]["generation"]
        .as_i64()
        .ok_or("missing generation")?;
    let mut mcp = identity_client(&path, &passphrase, Some(setup_token), false)?;
    let setup_cli = |args: &[&str]| {
        data(
            bk(&path)?
                .arg("--passphrase-file")
                .arg(&passphrase)
                .args(["--agent-token", setup_token, "--json", "setup"])
                .args(args),
        )
    };

    assert_setup_book_create_parity(&setup_cli, &mut mcp, generation)?;
    data(
        bk(&path)?
            .arg("--passphrase-file")
            .arg(&passphrase)
            .args(["--json", "company", "create", "second", "Second"]),
    )?;
    assert_setup_profile_and_catalog_parity(&setup_cli, &mut mcp, generation)?;
    assert_embedded_setup_parity(&setup_cli, &mut mcp, &db)?;

    let owner_cli = |args: &[&str]| {
        data(
            bk(&path)?
                .arg("--passphrase-file")
                .arg(&passphrase)
                .args(["--agent-token", owner_token, "--json", "setup"])
                .args(args),
        )
    };
    assert_setup_lifecycle_parity(&owner_cli, &mut owner_client, generation, &key, started)
}

fn assert_setup_lifecycle_parity(
    owner_cli: &JsonCli<'_>,
    mcp: &mut Client,
    generation: i64,
    public_key: &str,
    started_mcp: Value,
) -> TestResult {
    let revoke_mcp = mcp.success("ledger_setup_revoke", &json!({"generation":generation}))?;
    assert_eq!(owner_cli(&["revoke", &generation.to_string()])?, revoke_mcp);
    let started_cli = owner_cli(&["begin", "setup", "--public-key", public_key])?;
    assert_eq!(
        normalized_setup_start(started_cli)?,
        normalized_setup_start(started_mcp)?
    );
    Ok(())
}

fn normalized_setup_start(mut started: Value) -> JsonData {
    let credential = &started["credential"];
    let issued = chrono::DateTime::parse_from_rfc3339(
        credential["issued_at"]
            .as_str()
            .ok_or("missing issuance time")?,
    )?;
    let expires = chrono::DateTime::parse_from_rfc3339(
        credential["expires_at"].as_str().ok_or("missing expiry")?,
    )?;
    assert_eq!((expires - issued).num_minutes(), 15);
    for field in ["agent_uri", "token_jti", "expires_at"] {
        assert_eq!(started["session"][field], started["credential"][field]);
        started["session"][field] = json!("<generated>");
        started["credential"][field] = json!("<generated>");
    }
    assert!(started["credential"]["token"].is_string());
    started["credential"]["token"] = json!("<credential>");
    started["credential"]["issued_at"] = json!("<generated>");
    assert!(started["session"]["generation"].is_i64());
    started["session"]["generation"] = json!("<generation>");
    Ok(started)
}

fn assert_setup_book_create_parity(
    setup_cli: &JsonCli<'_>,
    mcp: &mut Client,
    generation: i64,
) -> TestResult {
    let generation_arg = generation.to_string();
    let settings =
        json!({"purpose":"personal", "base_currency":"BHD", "fiscal_year_start":"01-01"});
    let target = |company, reference| json!({"company":company,"generation":generation,"reference":reference});
    let request = json!({"target":target("home", "create"), "settings":settings, "name":"Home"});
    assert_eq!(
        mcp.success("ledger_setup_book_create", &request)?,
        setup_cli(&[
            "book-create",
            "home",
            "Home",
            "--generation",
            &generation_arg,
            "--reference",
            "create",
            "--purpose",
            "personal",
            "--base-currency",
            "BHD",
            "--fiscal-year-start",
            "01-01"
        ])?
    );
    Ok(())
}

fn assert_setup_profile_and_catalog_parity(
    setup_cli: &JsonCli<'_>,
    mcp: &mut Client,
    generation: i64,
) -> TestResult {
    let generation_arg = generation.to_string();
    let settings =
        json!({"purpose":"personal", "base_currency":"BHD", "fiscal_year_start":"01-01"});
    let target = |company, reference| json!({"company":company,"generation":generation,"reference":reference});
    let request = json!({"target":target("second", "profile"), "settings":settings});
    let initialized = mcp.success("ledger_setup_book_initialize", &request)?;
    assert_eq!(
        initialized,
        setup_cli(&[
            "book-initialize",
            "second",
            "--generation",
            &generation_arg,
            "--reference",
            "profile",
            "--purpose",
            "personal",
            "--base-currency",
            "BHD",
            "--fiscal-year-start",
            "01-01"
        ])?
    );
    let revision = initialized["profile"]["revision"]
        .as_i64()
        .ok_or("missing profile revision")?;
    let request = json!({"target":target("second", "revision"), "supersedes":revision,
        "settings":{"purpose":"personal", "base_currency":"BHD", "fiscal_year_start":"04-01"}});
    assert_eq!(
        mcp.success("ledger_setup_book_update", &request)?,
        setup_cli(&[
            "book-update",
            "second",
            "--generation",
            &generation_arg,
            "--reference",
            "revision",
            "--supersedes",
            &revision.to_string(),
            "--purpose",
            "personal",
            "--base-currency",
            "BHD",
            "--fiscal-year-start",
            "04-01"
        ])?
    );
    assert_eq!(
        mcp.success(
            "ledger_setup_chart_apply",
            &json!({"target":target("home", "chart"),"template":"personal-v1"})
        )?,
        setup_cli(&[
            "chart-apply",
            "home",
            "personal-v1",
            "--generation",
            &generation_arg,
            "--reference",
            "chart"
        ])?
    );
    assert_eq!(
        mcp.success("ledger_setup_account_add", &json!({"target":target("home", "account"),"code":"1234","name":" Spare ","account_type":"asset"}))?,
        setup_cli(&["account-add", "home", "1234", " Spare ", "--type", "asset",
            "--generation", &generation_arg, "--reference", "account"])?
    );

    Ok(())
}

fn assert_embedded_setup_parity(setup_cli: &JsonCli<'_>, mcp: &mut Client, db: &Db) -> TestResult {
    // Confirm the embedded API exposes the same readiness facts as the agent interfaces.
    use beankeeper_cli::{
        commands::setup::{SetupRequest, SetupResponse},
        request::{LedgerRequest, LedgerResponse},
    };
    let response =
        LedgerRequest::Setup(SetupRequest::Status).execute(db, &Actor::new("desktop".parse()?))?;
    let LedgerResponse::Setup(_, _, SetupResponse::Status(status)) = response else {
        return Err("unexpected embedded setup result".into());
    };
    let mcp_status = mcp.success("ledger_setup_status", &json!({}))?;
    assert_eq!(serde_json::to_value(status)?, mcp_status);
    assert_eq!(setup_cli(&["status"])?, mcp_status);
    assert_eq!(
        mcp.success("ledger_setup_book_show", &json!({"company":"home"}))?,
        setup_cli(&["book-show", "home"])?
    );
    Ok(())
}

pub(super) fn assert_cli_statement_receipts(
    path: &Path,
    passphrase: &Path,
    approver_token: &str,
    approval: &Value,
    expected: &Value,
) -> TestResult {
    let batch = approval["batch_id"].as_i64().ok_or("missing batch ID")?;
    let digest = approval["preview_sha256"]
        .as_str()
        .ok_or("missing digest")?;
    let company = approval["company"].as_str().ok_or("missing company")?;
    let cli = || {
        let mut command = bk(path)?;
        command.arg("--passphrase-file").arg(passphrase).args([
            "--json",
            "--agent-token",
            approver_token,
            "setup",
        ]);
        Ok::<_, Box<dyn Error>>(command)
    };
    assert_eq!(
        data(cli()?.args([
            "statement-commit",
            company,
            &batch.to_string(),
            "--preview-sha256",
            digest,
        ]))?,
        *expected
    );
    assert_eq!(
        data(cli()?.args(["statement-receipt", company, &batch.to_string()]))?,
        *expected
    );
    Ok(())
}
