//! Regression tests for issues #19 and #21.
//!
//! #19: `report balance-sheet` must balance (Assets = Liabilities + Equity)
//! once revenue/expense activity exists, by including a computed
//! retained-earnings equity line.
//!
//! #21: `report income-statement` and `report balance-sheet` must render a
//! sectioned statement (sections, subtotals, net income / balanced flag) in
//! table, JSON and CSV output, while `report trial-balance` keeps its layout.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::tempdir;

const COMPANY: &str = "stmt-co";

/// Run `bk` against `db_path` and return `(success, stdout)`.
fn run_bk(db_path: &str, args: &[&str]) -> (bool, String) {
    let mut cmd =
        Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.arg("--db").arg(db_path);
    cmd.args(args);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("bk should execute: {e}"));
    let stdout =
        String::from_utf8(output.stdout).unwrap_or_else(|e| panic!("stdout is valid utf-8: {e}"));
    (output.status.success(), stdout)
}

/// Run a `bk --company COMPANY ...` command that must succeed.
fn bk_ok(db_path: &str, args: &[&str]) -> String {
    let mut full = vec!["--company", COMPANY];
    full.extend_from_slice(args);
    let (ok, stdout) = run_bk(db_path, &full);
    assert!(ok, "bk {full:?} should succeed; stdout: {stdout}");
    stdout
}

/// Create a fresh company with one account of each type and post:
/// - 2026-01-10: owner invests 1,000.00 (cash / owner equity)
/// - 2026-02-01: sale of 300.00 (cash / sales)
/// - 2026-02-15: rent of 120.00 (rent / cash)
/// - 2026-03-01: buy supplies on credit 50.00 (rent / payable)
fn setup() -> (tempfile::TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("create tempdir: {e}"));
    let db_path = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("path is utf-8"))
        .to_string();

    let (ok, _) = run_bk(&db_path, &["init"]);
    assert!(ok, "init should succeed");
    let (ok, _) = run_bk(&db_path, &["company", "create", COMPANY, "Statement Co"]);
    assert!(ok, "company create should succeed");

    for (code, name, ty) in [
        ("1000", "Cash", "asset"),
        ("2000", "Accounts Payable", "liability"),
        ("3000", "Owner Equity", "equity"),
        ("4000", "Sales", "revenue"),
        ("5000", "Rent", "expense"),
    ] {
        bk_ok(&db_path, &["account", "create", code, name, "--type", ty]);
    }

    for (desc, date, debit, credit) in [
        ("invest", "2026-01-10", "1000:1000", "3000:1000"),
        ("sale", "2026-02-01", "1000:300", "4000:300"),
        ("rent", "2026-02-15", "5000:120", "1000:120"),
        ("on credit", "2026-03-01", "5000:50", "2000:50"),
    ] {
        bk_ok(
            &db_path,
            &[
                "txn", "post", "-d", desc, "--date", date, "--debit", debit, "--credit", credit,
            ],
        );
    }

    (dir, db_path)
}

fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("valid JSON ({e}): {stdout}"))
}

fn section<'a>(data: &'a Value, key: &str) -> &'a Value {
    data["sections"]
        .as_array()
        .unwrap_or_else(|| panic!("sections is an array: {data}"))
        .iter()
        .find(|s| s["key"] == key)
        .unwrap_or_else(|| panic!("section {key} present: {data}"))
}

fn account_amount(section: &Value, code: &str) -> i64 {
    section["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("accounts is an array: {section}"))
        .iter()
        .find(|a| a["code"] == code)
        .and_then(|a| a["amount"].as_i64())
        .unwrap_or_else(|| panic!("account {code} with amount in {section}"))
}

#[test]
fn balance_sheet_balances_after_pnl_activity() {
    let (_dir, db_path) = setup();

    let table = bk_ok(&db_path, &["--no-color", "report", "balance-sheet"]);
    assert!(
        table.contains("[ok] BALANCED"),
        "table should be balanced:\n{table}"
    );
    assert!(!table.contains("UNBALANCED"), "{table}");
    assert!(table.contains("Retained Earnings (computed)"), "{table}");
    assert!(table.contains("Total Liabilities + Equity"), "{table}");

    // JSON carries the same balanced flag.
    let json = parse_json(&bk_ok(&db_path, &["--json", "report", "balance-sheet"]));
    assert_eq!(json["data"]["balanced"], true, "{json}");
}

#[test]
fn balance_sheet_json_structure() {
    let (_dir, db_path) = setup();
    let json = parse_json(&bk_ok(&db_path, &["--json", "report", "balance-sheet"]));

    assert_eq!(json["ok"], true);
    assert_eq!(json["meta"]["command"], "report.balance-sheet");
    let data = &json["data"];

    let keys: Vec<&str> = data["sections"]
        .as_array()
        .unwrap_or_else(|| panic!("sections array"))
        .iter()
        .filter_map(|s| s["key"].as_str())
        .collect();
    assert_eq!(keys, ["assets", "liabilities", "equity"]);

    // Cash: 1000 + 300 - 120 = 1180.00
    let assets = section(data, "assets");
    assert_eq!(account_amount(assets, "1000"), 118_000);
    assert_eq!(assets["total"], 118_000);

    let liabilities = section(data, "liabilities");
    assert_eq!(account_amount(liabilities, "2000"), 5_000);
    assert_eq!(liabilities["total"], 5_000);

    // Equity: owner 1000 + computed earnings (300 - 170 = 130).
    let equity = section(data, "equity");
    assert_eq!(account_amount(equity, "3000"), 100_000);
    let computed = equity["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("equity accounts"))
        .iter()
        .find(|a| a["computed"] == true)
        .unwrap_or_else(|| panic!("computed earnings line: {equity}"));
    assert_eq!(computed["code"], Value::Null);
    assert_eq!(computed["name"], "Retained Earnings (computed)");
    assert_eq!(computed["amount"], 13_000);
    assert_eq!(equity["total"], 113_000);

    assert_eq!(data["total_assets"], 118_000);
    assert_eq!(data["total_liabilities"], 5_000);
    assert_eq!(data["total_equity"], 113_000);
    assert_eq!(data["computed_earnings"], 13_000);
    assert_eq!(data["total_liabilities_and_equity"], 118_000);
    assert_eq!(data["difference"], 0);
    assert_eq!(data["balanced"], true);

    // As of an earlier date only the investment and sale count.
    let json = parse_json(&bk_ok(
        &db_path,
        &["--json", "report", "balance-sheet", "--to", "2026-02-01"],
    ));
    let data = &json["data"];
    assert_eq!(data["as_of"], "2026-02-01");
    assert_eq!(data["total_assets"], 130_000);
    assert_eq!(data["computed_earnings"], 30_000);
    assert_eq!(data["balanced"], true);
}

#[test]
fn income_statement_json_structure() {
    let (_dir, db_path) = setup();
    let json = parse_json(&bk_ok(
        &db_path,
        &[
            "--json",
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-12-31",
        ],
    ));

    assert_eq!(json["meta"]["command"], "report.income-statement");
    let data = &json["data"];
    assert_eq!(data["from"], "2026-01-01");
    assert_eq!(data["to"], "2026-12-31");

    let revenue = section(data, "revenue");
    assert_eq!(revenue["normal_balance"], "credit");
    assert_eq!(account_amount(revenue, "4000"), 30_000);
    assert_eq!(revenue["total"], 30_000);

    let expenses = section(data, "expenses");
    assert_eq!(expenses["normal_balance"], "debit");
    assert_eq!(account_amount(expenses, "5000"), 17_000);
    assert_eq!(expenses["total"], 17_000);

    assert_eq!(data["total_revenue"], 30_000);
    assert_eq!(data["total_expenses"], 17_000);
    assert_eq!(data["net_income"], 13_000);

    // A period with only an expense is a net loss (negative net_income).
    let json = parse_json(&bk_ok(
        &db_path,
        &[
            "--json",
            "report",
            "income-statement",
            "--from",
            "2026-02-10",
            "--to",
            "2026-02-28",
        ],
    ));
    assert_eq!(json["data"]["net_income"], -12_000);

    let table = bk_ok(
        &db_path,
        &[
            "--no-color",
            "report",
            "income-statement",
            "--from",
            "2026-02-10",
            "--to",
            "2026-02-28",
        ],
    );
    assert!(table.contains("Total Revenue"), "{table}");
    assert!(table.contains("Total Expenses"), "{table}");
    assert!(table.contains("Net Loss: 120.00"), "{table}");
}

#[test]
fn statements_csv_structure() {
    let (_dir, db_path) = setup();

    let csv = bk_ok(&db_path, &["--format", "csv", "report", "income-statement"]);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines,
        [
            "section,line_type,code,name,type,amount",
            "revenue,account,4000,Sales,revenue,30000",
            "revenue,subtotal,,Total Revenue,,30000",
            "expenses,account,5000,Rent,expense,17000",
            "expenses,subtotal,,Total Expenses,,17000",
            "summary,net_income,,Net Income,,13000",
        ]
    );

    let csv = bk_ok(&db_path, &["--format", "csv", "report", "balance-sheet"]);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines,
        [
            "section,line_type,code,name,type,amount",
            "assets,account,1000,Cash,asset,118000",
            "assets,subtotal,,Total Assets,,118000",
            "liabilities,account,2000,Accounts Payable,liability,5000",
            "liabilities,subtotal,,Total Liabilities,,5000",
            "equity,account,3000,Owner Equity,equity,100000",
            "equity,computed,,Retained Earnings (computed),equity,13000",
            "equity,subtotal,,Total Equity,,113000",
            "summary,total,,Total Liabilities + Equity,,118000",
            "summary,difference,,Difference,,0",
            "summary,balanced,,Balanced,,true",
        ]
    );
}

#[test]
fn trial_balance_keeps_flat_layout() {
    let (_dir, db_path) = setup();

    let json = parse_json(&bk_ok(&db_path, &["--json", "report", "trial-balance"]));
    let data = &json["data"];
    assert!(data["accounts"].is_array(), "{data}");
    assert!(data.get("sections").is_none(), "{data}");
    assert_eq!(data["total_debits"], 147_000);
    assert_eq!(data["total_credits"], 147_000);
    assert_eq!(data["balanced"], true);

    let csv = bk_ok(&db_path, &["--format", "csv", "report", "trial-balance"]);
    assert!(
        csv.starts_with("code,name,type,debit_total,credit_total"),
        "{csv}"
    );
}
