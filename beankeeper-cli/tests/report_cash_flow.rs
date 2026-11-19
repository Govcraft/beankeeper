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
