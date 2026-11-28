//! Integration tests for `bk report cash-flow` (issue #24).

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const COMPANY: &str = "cf";

/// Run `bk --db <db> <args>`, returning (exit code, stdout, stderr).
fn run_bk(db_path: &str, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
    cmd.arg("--db").arg(db_path).args(args);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("bk should run: {e}"));
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run a `bk --company cf ...` command that must succeed.
fn ok(db_path: &str, args: &[&str]) -> String {
    let mut full = vec!["--company", COMPANY];
    full.extend_from_slice(args);
    let (code, stdout, stderr) = run_bk(db_path, &full);
    assert_eq!(code, 0, "bk {full:?} failed: {stderr}");
    stdout
}

fn post(db_path: &str, date: &str, debits: &[&str], credits: &[&str]) {
    let mut args = vec!["txn", "post", "-d", "t", "--date", date];
    for d in debits {
        args.extend(["--debit", d]);
    }
    for c in credits {
        args.extend(["--credit", c]);
    }
    ok(db_path, &args);
}

/// A fresh database with company `cf`, a small chart of accounts and no
/// transactions.
fn empty_ledger() -> (TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db_path = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("utf-8 path"))
        .to_string();

    let (code, _, stderr) = run_bk(&db_path, &["init"]);
    assert_eq!(code, 0, "init failed: {stderr}");
    let (code, _, stderr) = run_bk(&db_path, &["company", "create", COMPANY, "Cash Flow Co"]);
    assert_eq!(code, 0, "company create failed: {stderr}");

    for (code, name, ty) in [
        ("1000", "Checking", "asset"),
        ("1010", "Savings Account", "asset"),
        ("1100", "Accounts Receivable", "asset"),
        ("1500", "Equipment", "asset"),
        ("2000", "Loan Payable", "liability"),
        ("3000", "Owner Equity", "equity"),
        ("4000", "Sales", "revenue"),
        ("5000", "Rent", "expense"),
    ] {
        ok(&db_path, &["account", "create", code, name, "--type", ty]);
    }
    for code in ["1000", "1010"] {
        ok(&db_path, &["account", "classify", code, "cash"]);
    }
    (dir, db_path)
}

/// The standard scenario. With the default cash accounts (1000 + 1010):
/// opening 1,000.00; operating +400.00; investing -300.00; financing
/// +1,000.00; net change 1,100.00; closing 2,100.00.
fn ledger() -> (TempDir, String) {
    let (dir, db) = empty_ledger();
    // Before the range: opening cash.
    post(&db, "2025-01-05", &["1000:1000"], &["3000:1000"]);
    // Operating.
    post(&db, "2025-02-01", &["1000:500"], &["4000:500"]);
    post(&db, "2025-02-02", &["5000:200"], &["1000:200"]);
    // Investing.
    post(&db, "2025-02-03", &["1500:300"], &["1000:300"]);
    // Financing.
    post(&db, "2025-02-04", &["1000:1000"], &["2000:1000"]);
    // Cash-to-cash transfer (excluded under the default cash set).
    post(&db, "2025-02-05", &["1010:400"], &["1000:400"]);
    // Non-cash transaction (never included).
    post(&db, "2025-02-06", &["1100:50"], &["4000:50"]);
    // Split: cash 100 + rent 50 against sales 150.
    post(&db, "2025-02-07", &["1000:100", "5000:50"], &["4000:150"]);
    // After the range.
    post(&db, "2025-04-01", &["1000:999"], &["4000:999"]);
    (dir, db)
}

fn cash_flow_json(db: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "--json",
        "report",
        "cash-flow",
        "--from",
        "2025-02-01",
        "--to",
        "2025-03-31",
    ];
    args.extend_from_slice(extra);
    let stdout = ok(db, &args);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
}

/// `(code, amount)` pairs of a section.
fn lines(data: &Value, section: &str) -> Vec<(String, i64)> {
    data[section]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("{section}.lines missing"))
        .iter()
        .map(|l| {
            (
                l["code"].as_str().unwrap_or_default().to_string(),
                l["amount"].as_i64().unwrap_or_default(),
            )
        })
        .collect()
}

fn pairs(v: &[(&str, i64)]) -> Vec<(String, i64)> {
    v.iter().map(|(c, a)| ((*c).to_string(), *a)).collect()
}

#[test]
fn classifies_operating_investing_financing() {
    let (_dir, db) = ledger();
    let data = &cash_flow_json(&db, &[])["data"];

    assert_eq!(
        lines(data, "operating"),
        pairs(&[("4000", 65_000), ("5000", -25_000)])
    );
    assert_eq!(data["operating"]["total"], 40_000);
    assert_eq!(lines(data, "investing"), pairs(&[("1500", -30_000)]));
    assert_eq!(data["investing"]["total"], -30_000);
    assert_eq!(lines(data, "financing"), pairs(&[("2000", 100_000)]));
    assert_eq!(data["financing"]["total"], 100_000);
}

#[test]
fn excludes_cash_to_cash_transfers_and_non_cash_transactions() {
    let (_dir, db) = ledger();
    let data = &cash_flow_json(&db, &[])["data"];

    let all_codes: Vec<String> = ["operating", "investing", "financing"]
        .iter()
        .flat_map(|s| lines(data, s))
        .map(|(c, _)| c)
        .collect();
    assert!(!all_codes.contains(&"1010".to_string()), "transfer leaked");
    assert!(
        !all_codes.contains(&"1000".to_string()),
        "cash account listed"
    );
    assert!(
        !all_codes.contains(&"1100".to_string()),
        "non-cash txn leaked"
    );
}

#[test]
fn opening_plus_net_change_equals_closing() {
    let (_dir, db) = ledger();
    let data = &cash_flow_json(&db, &[])["data"];

    assert_eq!(data["opening_cash"], 100_000);
    assert_eq!(data["net_change"], 110_000);
    assert_eq!(data["closing_cash"], 210_000);
    assert_eq!(data["reconciled"], true);
    assert_eq!(data["difference"], 0);

    // Cross-check closing cash against `report balance` for each cash account.
    let cash_codes: Vec<&str> = data["cash_accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("cash_accounts missing"))
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert_eq!(cash_codes, vec!["1000", "1010"]);
    let closing: i64 = cash_codes
        .iter()
        .map(|code| {
            let out = ok(
                &db,
                &[
                    "--json",
                    "report",
                    "balance",
                    "--account",
                    code,
                    "--to",
                    "2025-03-31",
                ],
            );
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("json: {e}"));
            v["data"]["debit_total"].as_i64().unwrap_or_default()
                - v["data"]["credit_total"].as_i64().unwrap_or_default()
        })
        .sum();
    assert_eq!(data["closing_cash"], closing);
}

#[test]
fn explicit_cash_account_treats_other_cash_as_investing() {
    let (_dir, db) = ledger();
    let data = &cash_flow_json(&db, &["--cash-account", "1000"])["data"];

    assert_eq!(
        lines(data, "investing"),
        pairs(&[("1010", -40_000), ("1500", -30_000)])
    );
    assert_eq!(data["opening_cash"], 100_000);
    assert_eq!(data["net_change"], 70_000);
    assert_eq!(data["closing_cash"], 170_000);
    assert_eq!(data["reconciled"], true);
    assert_eq!(data["cash_accounts"][0]["code"], "1000");
    assert_eq!(data["cash_accounts"].as_array().map(Vec::len), Some(1));
}

#[test]
fn explicit_cash_account_must_exist_and_be_an_asset() {
    let (_dir, db) = ledger();
    let base = [
        "--company",
        COMPANY,
        "report",
        "cash-flow",
        "--from",
        "2025-02-01",
        "--to",
        "2025-03-31",
        "--cash-account",
    ];
    let mut missing = base.to_vec();
    missing.push("9999");
    assert_eq!(run_bk(&db, &missing).0, 5);
    let mut liability = base.to_vec();
    liability.push("2000");
    assert_eq!(run_bk(&db, &liability).0, 3);
}

#[test]
fn json_envelope_shape() {
    let (_dir, db) = ledger();
    let json = cash_flow_json(&db, &[]);

    assert_eq!(json["ok"], true);
    assert_eq!(json["meta"]["command"], "report.cash-flow");
    assert_eq!(json["meta"]["company"], COMPANY);
    let data = &json["data"];
    assert_eq!(data["from"], "2025-02-01");
    assert_eq!(data["to"], "2025-03-31");
    assert_eq!(data["currency"], "USD");
    for key in [
        "cash_accounts",
        "operating",
        "investing",
        "financing",
        "net_change",
        "opening_cash",
        "closing_cash",
        "reconciled",
        "difference",
    ] {
        assert!(data.get(key).is_some(), "data.{key} missing");
    }
    let line = &data["operating"]["lines"][0];
    for key in ["code", "name", "account_type", "amount"] {
        assert!(line.get(key).is_some(), "line.{key} missing");
    }
    assert_eq!(line["account_type"], "revenue");
}

#[test]
fn table_and_csv_render() {
    let (_dir, db) = ledger();
    let table = ok(
        &db,
        &[
            "report",
            "cash-flow",
            "--from",
            "2025-02-01",
            "--to",
            "2025-03-31",
        ],
    );
    assert!(table.contains("Cash Flow Statement"));
    assert!(table.contains("Operating Activities"));
    assert!(table.contains("Financing Activities"));
    assert!(table.contains("2,100.00"));
    assert!(table.contains("[ok] RECONCILED"));

    let csv = ok(
        &db,
        &[
            "--format",
            "csv",
            "report",
            "cash-flow",
            "--from",
            "2025-02-01",
            "--to",
            "2025-03-31",
        ],
    );
    let mut rows = csv.lines();
    assert_eq!(
        rows.next(),
        Some("row_type,section,code,name,account_type,amount")
    );
    assert!(csv.contains("line,investing,1500,Equipment,asset,-30000"));
    assert!(csv.contains("summary,,,closing_cash,,210000"));
}

#[test]
fn no_cash_accounts_is_a_validation_error() {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir.path().join("b.db").to_string_lossy().into_owned();
    assert_eq!(run_bk(&db, &["init"]).0, 0);
    assert_eq!(run_bk(&db, &["company", "create", COMPANY, "X"]).0, 0);
    ok(
        &db,
        &[
            "account",
            "create",
            "1100",
            "Receivables",
            "--type",
            "asset",
        ],
    );

    let (code, _, stderr) = run_bk(
        &db,
        &[
            "--company",
            COMPANY,
            "report",
            "cash-flow",
            "--from",
            "2025-01-01",
            "--to",
            "2025-12-31",
        ],
    );
    assert_eq!(code, 3, "expected validation error, stderr: {stderr}");
    assert!(stderr.contains("--cash-account"), "stderr: {stderr}");
}

#[test]
fn currency_defaults_and_ambiguity() {
    let (_dir, db) = empty_ledger();
    // No transactions: defaults to USD with an empty statement.
    let data = &cash_flow_json(&db, &[])["data"];
    assert_eq!(data["currency"], "USD");
    assert_eq!(data["net_change"], 0);

    post(&db, "2025-02-01", &["1000:10"], &["4000:10"]);
    ok(
        &db,
        &[
            "txn",
            "post",
            "-d",
            "eur",
            "--date",
            "2025-02-02",
            "--currency",
            "EUR",
            "--debit",
            "1000:7",
            "--credit",
            "4000:7",
        ],
    );

    // Two currencies without --currency: validation error.
    let (code, _, _) = run_bk(
        &db,
        &[
            "--company",
            COMPANY,
            "report",
            "cash-flow",
            "--from",
            "2025-02-01",
            "--to",
            "2025-03-31",
        ],
    );
    assert_eq!(code, 3);

    // Explicit currency filters to that currency only.
    let data = &cash_flow_json(&db, &["--currency", "EUR"])["data"];
    assert_eq!(data["currency"], "EUR");
    assert_eq!(lines(data, "operating"), pairs(&[("4000", 700)]));
}

#[test]
fn rejects_inverted_range() {
    let (_dir, db) = empty_ledger();
    let (code, _, _) = run_bk(
        &db,
        &[
            "--company",
            COMPANY,
            "report",
            "cash-flow",
            "--from",
            "2025-03-01",
            "--to",
            "2025-02-01",
        ],
    );
    assert_eq!(code, 3);
}

#[test]
fn typed_cash_classes_include_wallets_and_equivalents_independently_of_names()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, db) = empty_ledger();
    for (code, name, class, amount) in [
        ("1020", "Travel wallet", "cash", "125"),
        ("1030", "Money-market fund", "cash_equivalent", "75"),
    ] {
        ok(&db, &["account", "create", code, name, "--type", "asset"]);
        ok(&db, &["account", "classify", code, class]);
        post(
            &db,
            "2025-02-12",
            &[&format!("{code}:{amount}")],
            &[&format!("3000:{amount}")],
        );
    }
    // An English cash keyword is insufficient to classify a new asset.
    ok(
        &db,
        &[
            "account",
            "create",
            "1040",
            "Bank shares",
            "--type",
            "asset",
        ],
    );
    post(&db, "2025-02-12", &["1040:90"], &["3000:90"]);
    let before = cash_flow_json(&db, &[]);
    assert_eq!(before["data"]["closing_cash"], 20_000);
    let codes: Vec<_> = before["data"]["cash_accounts"]
        .as_array()
        .ok_or("cash accounts")?
        .iter()
        .filter_map(|account| account["code"].as_str())
        .collect();
    assert_eq!(codes, ["1000", "1010", "1020", "1030"]);
    let summary: Value =
        serde_json::from_str(&ok(&db, &["summary", "--json", "--to", "2025-03-31"]))?;
    assert_eq!(summary["data"]["standing"]["cash_on_hand"], 20_000);
    // Explicit selection still replaces the typed default set.
    assert_eq!(
        cash_flow_json(&db, &["--cash-account", "1040"])["data"]["closing_cash"],
        9_000
    );
    Ok(())
}

#[test]
fn classification_rejects_non_assets_and_unknown_classes() {
    let (_dir, db) = empty_ledger();
    for (code, class) in [
        ("2000", "cash"),
        ("4000", "cash_equivalent"),
        ("1000", "liquid"),
    ] {
        let (status, _, stderr) = run_bk(
            &db,
            &["--company", COMPANY, "account", "classify", code, class],
        );
        assert_ne!(status, 0, "invalid class accepted: {code} {class}");
        assert!(stderr.contains("cash"), "{stderr}");
    }
    assert_eq!(
        cash_flow_json(&db, &[])["data"]["cash_accounts"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}

#[test]
fn cash_forecast_schedules_moves_a_receipt_and_skips_cash_transfers()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, path) = empty_ledger();
    post(&path, "2026-02-01", &["1000:100"], &["3000:100"]);
    post(&path, "2026-02-07", &["1000:120"], &["4000:120"]);
    post(&path, "2026-02-09", &["1010:10"], &["1000:10"]);
    ok(
        &path,
        &[
            "schedule",
            "create",
            "-d",
            "Rent",
            "--start",
            "2026-02-08",
            "--debit",
            "5000:150",
            "--credit",
            "1000:150",
        ],
    );
    let read = |adjust: Option<&str>| -> Result<Value, serde_json::Error> {
        let mut args = vec![
            "--json",
            "report",
            "cash-forecast",
            "--as-of",
            "2026-02-01",
            "--through",
            "2026-02-20",
            "--cadence",
            "monthly",
        ];
        if let Some(adjustment) = adjust {
            args.extend(["--adjust", adjustment]);
        }
        serde_json::from_str(&ok(&path, &args))
    };
    let baseline = read(None)?;
    let data = &baseline["data"];
    assert_eq!(data["periods"][0]["opening"], 10_000);
    assert_eq!(data["periods"][0]["closing"], 7_000);
    assert_eq!(data["events"].as_array().map(Vec::len), Some(2));
    let receipt = data["events"]
        .as_array()
        .ok_or("missing cash forecast evidence")?
        .iter()
        .find(|event| event["source"]["kind"] == "posted")
        .and_then(|event| event["source_id"].as_str())
        .ok_or("missing receipt source")?;
    let scenario = read(Some(&format!("{receipt}=move:2026-02-15")))?;
    assert_eq!(scenario["data"]["first_shortfall"], Value::Null);
    assert_eq!(
        scenario["data"]["scenario"]["first_shortfall"],
        "2026-02-08"
    );
    assert_eq!(scenario["data"]["scenario"]["periods"][0]["closing"], 7_000);
    Ok(())
}

#[test]
fn one_invoice_is_accrual_income_at_issue_and_cash_income_as_paid()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, path) = empty_ledger();
    let read = |args: &[&str]| -> Result<Value, serde_json::Error> {
        let mut command = vec!["--json"];
        command.extend_from_slice(args);
        serde_json::from_str(&ok(&path, &command))
    };
    let party = read(&[
        "counterparty",
        "create",
        "--name",
        "Customer",
        "--role",
        "customer",
    ])?;
    let terms = serde_json::json!({
        "kind":"invoice", "number":"CASH-1", "counterparty_id":party["data"]["party"]["id"],
        "control_account":"1100", "due_date":"2026-02-15",
        "lines":[{"description":"Service","quantity":"1","unit_price_minor":10000,"account":"4000"}]
    })
    .to_string();
    let draft = read(&["document", "draft", "--terms", &terms])?;
    let id = draft["data"]["record"]["id"]
        .as_str()
        .ok_or("missing invoice")?;
    read(&["document", "issue", id, "--date", "2026-01-15"])?;
    let payment = read(&[
        "txn",
        "post",
        "-d",
        "Deposit",
        "--date",
        "2026-02-09",
        "--debit",
        "1000:40",
        "--credit",
        "1100:40",
    ])?;
    let payment_id = payment["data"]["id"].to_string();
    read(&[
        "counterparty",
        "associate",
        party["data"]["party"]["id"]
            .as_str()
            .ok_or("missing counterparty")?,
        &payment_id,
    ])?;

    let allocation = serde_json::json!([{"document_id":id,"amount_minor":4000}]).to_string();
    read(&[
        "document",
        "allocate",
        &payment_id,
        "--date",
        "2026-02-09",
        "--allocations",
        &allocation,
    ])?;
    let accrual = read(&[
        "report",
        "income-statement",
        "--from",
        "2026-01-01",
        "--to",
        "2026-01-31",
    ])?;
    assert_eq!(accrual["data"]["net_income"], 10_000);
    let cash = read(&[
        "report",
        "income-statement",
        "--basis",
        "cash",
        "--from",
        "2026-02-01",
        "--to",
        "2026-02-28",
        "--compare",
        "previous",
    ])?;
    assert_eq!(cash["data"]["current"]["net_income"], 4_000);
    assert_eq!(cash["data"]["prior"]["net_income"], 0);
    assert_eq!(cash["data"]["net_income_change"], 4_000);
    let forecast = read(&[
        "report",
        "cash-forecast",
        "--as-of",
        "2026-02-01",
        "--through",
        "2026-02-28",
    ])?;
    assert_invoice_forecast(&forecast)
}

fn assert_invoice_forecast(forecast: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let events = forecast["data"]["events"]
        .as_array()
        .ok_or("missing forecast events")?;
    assert!(
        events
            .iter()
            .any(|event| event["source"]["kind"] == "open_item" && event["movement"] == 6_000)
    );
    assert!(
        events
            .iter()
            .any(|event| event["source"]["kind"] == "posted" && event["movement"] == 4_000)
    );
    Ok(())
}

#[test]
fn a_designated_card_uses_the_charge_date_and_omits_its_cash_settlement()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, path) = empty_ledger();
    ok(
        &path,
        &[
            "account",
            "create",
            "2100",
            "Business Card",
            "--type",
            "liability",
        ],
    );
    post(&path, "2026-03-01", &["5000:40"], &["2100:40"]);
    post(&path, "2026-04-01", &["2100:40"], &["1000:40"]);
    let result: Value = serde_json::from_str(&ok(
        &path,
        &[
            "--json",
            "report",
            "income-statement",
            "--basis",
            "cash",
            "--card-account",
            "2100",
            "--from",
            "2026-04-01",
            "--to",
            "2026-04-30",
            "--compare",
            "previous",
        ],
    ))?;
    assert_eq!(result["data"]["current"]["net_income"], 0);
    assert_eq!(result["data"]["prior"]["net_income"], -4000);
    assert_eq!(
        result["data"]["prior"]["recognition"][0]["date"],
        "2026-03-01"
    );
    Ok(())
}

type ForecastTest = Result<(), Box<dyn std::error::Error>>;

fn linked_forecast(db: &str, extra: &[&str]) -> Result<Value, serde_json::Error> {
    let mut args = vec![
        "--json",
        "report",
        "cash-forecast",
        "--as-of",
        "2026-02-01",
        "--through",
        "2026-02-20",
        "--cadence",
        "monthly",
    ];
    args.extend_from_slice(extra);
    let envelope: Value = serde_json::from_str(&ok(db, &args))?;
    Ok(envelope["data"].clone())
}

fn forecast_obligations(
    path: &str,
    schedule_amount: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    post(path, "2026-02-01", &["1000:70"], &["3000:70"]);
    let party: Value = serde_json::from_str(&ok(
        path,
        &[
            "--json",
            "counterparty",
            "create",
            "--name",
            "Rent vendor",
            "--role",
            "vendor",
        ],
    ))?;
    let terms = serde_json::json!({
        "kind":"bill", "number":"RENT-1", "counterparty_id":party["data"]["party"]["id"],
        "control_account":"2000", "due_date":"2026-02-08",
        "lines":[{"description":"Rent","quantity":"1","unit_price_minor":10000,"account":"5000"}],
    });
    let draft: Value = serde_json::from_str(&ok(
        path,
        &["--json", "document", "draft", "--terms", &terms.to_string()],
    ))?;
    let bill = draft["data"]["record"]["id"]
        .as_str()
        .ok_or("missing bill ID")?;
    ok(path, &["document", "issue", bill, "--date", "2026-02-01"]);
    ok(
        path,
        &[
            "schedule",
            "create",
            "-d",
            "February cash payment",
            "--start",
            "2026-02-08",
            "--debit",
            &format!("5000:{schedule_amount}"),
            "--credit",
            &format!("1000:{schedule_amount}"),
        ],
    );
    Ok(format!("open-item:{bill}"))
}

fn forecast_schedule(data: &Value) -> Result<&str, Box<dyn std::error::Error>> {
    data["events"]
        .as_array()
        .ok_or("missing forecast evidence")?
        .iter()
        .find(|event| event["source"]["kind"] == "scheduled")
        .and_then(|event| event["source_id"].as_str())
        .ok_or_else(|| "missing scheduled source".into())
}

#[test]
fn forecast_reports_equal_amount_and_date_as_advisory_until_linked() -> ForecastTest {
    let (_directory, db) = empty_ledger();
    let document = forecast_obligations(&db, "100")?;
    let baseline = linked_forecast(&db, &[])?;
    let schedule = forecast_schedule(&baseline)?;
    assert_eq!(baseline["events"].as_array().map(Vec::len), Some(2));
    assert_eq!(baseline["periods"][0]["closing"], -13_000);
    assert_eq!(
        baseline["possible_overlaps"][0]["scheduled_source_id"],
        schedule
    );
    assert_eq!(
        baseline["possible_overlaps"][0]["document_source_id"],
        document
    );
    let link = format!("{schedule}={document}");
    let linked = linked_forecast(&db, &["--link-source", &link])?;
    assert_eq!(linked["events"].as_array().map(Vec::len), Some(1));
    assert_eq!(linked["events"][0]["source_id"], schedule);
    assert_eq!(linked["periods"][0]["closing"], -3_000);
    assert_eq!(linked["possible_overlaps"], serde_json::json!([]));
    assert_eq!(linked["linked_sources"][0]["link"], link);
    assert_eq!(linked["linked_sources"][0]["covered"], -10_000);
    Ok(())
}

#[test]
fn forecast_link_preserves_partial_bill_and_uses_the_scheduled_cash_date() -> ForecastTest {
    let (_directory, db) = empty_ledger();
    let document = forecast_obligations(&db, "60")?;
    let before = ok(&db, &["--json", "verify"]);
    let baseline = linked_forecast(&db, &[])?;
    let schedule = forecast_schedule(&baseline)?;
    let link = format!("{schedule}={document}");
    let adjustment = format!("{schedule}=move:2026-02-15");
    let linked = linked_forecast(&db, &["--link-source", &link, "--adjust", &adjustment])?;
    assert_eq!(linked["periods"][0]["closing"], -3_000);
    assert_eq!(linked["first_shortfall"], "2026-02-08");
    assert_eq!(linked["scenario"]["first_shortfall"], "2026-02-15");
    let residual = linked["events"]
        .as_array()
        .ok_or("missing linked events")?
        .iter()
        .find(|event| event["source_id"] == document)
        .ok_or("missing residual bill")?;
    assert_eq!(residual["movement"], -4_000);
    assert_eq!(residual["on"], "2026-02-08");
    assert_eq!(linked["linked_sources"][0]["covered"], -6_000);
    let after = ok(&db, &["--json", "verify"]);
    let before: Value = serde_json::from_str(&before)?;
    let after: Value = serde_json::from_str(&after)?;
    assert_eq!(before["data"], after["data"]);
    Ok(())
}
