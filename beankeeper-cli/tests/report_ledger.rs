//! Integration tests for `bk report ledger` (issue #23): general ledger /
//! account statement with opening balance, running balances, and closing
//! balance per account.

use std::error::Error;
use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::commands::report_ledger::{LedgerOptions, build_report};
use beankeeper_cli::db::connection::Db;
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const CO: &str = "ledger-co";

/// Run `bk --db <db> <args...>` and return the raw output.
fn bk(db: &str, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::cargo_bin("bk")?
        .arg("--db")
        .arg(db)
        .args(args)
        .output()?)
}

/// Run a `bk` command that must succeed, returning stdout.
fn bk_ok(db: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = bk(db, args)?;
    if !out.status.success() {
        return Err(format!(
            "bk {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Post a balanced two-leg transaction.
fn post(db: &str, date: &str, desc: &str, debit: &str, credit: &str, extra: &[&str]) -> TestResult {
    let mut args = vec![
        "--company",
        CO,
        "txn",
        "post",
        "-d",
        desc,
        "--debit",
        debit,
        "--credit",
        credit,
        "--date",
        date,
    ];
    args.extend_from_slice(extra);
    bk_ok(db, &args)?;
    Ok(())
}

/// Create a database with one company and a small chart of accounts:
///
/// - 1000 Cash (asset), 2000 Loan (liability, unused), 4000 Sales (revenue),
///   5000 Rent (expense)
///
/// and three USD transactions, posted out of date order:
///
/// 1. 2025-01-10  Dr 1000 / Cr 4000  100.00  (ref INV-1)
/// 2. 2025-01-05  Dr 5000 / Cr 1000   30.00
/// 3. 2025-01-20  Dr 1000 / Cr 4000   50.00
fn setup() -> Result<(TempDir, String), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .ok_or("non-utf8 temp path")?
        .to_string();

    bk_ok(&db, &["init"])?;
    bk_ok(&db, &["company", "create", CO, "Ledger Co"])?;
    for (code, name, ty) in [
        ("1000", "Cash", "asset"),
        ("2000", "Loan", "liability"),
        ("4000", "Sales", "revenue"),
        ("5000", "Rent", "expense"),
    ] {
        bk_ok(
            &db,
            &[
                "--company",
                CO,
                "account",
                "create",
                code,
                name,
                "--type",
                ty,
            ],
        )?;
    }

    post(
        &db,
        "2025-01-10",
        "Invoice 1",
        "1000:100",
        "4000:100",
        &["-r", "INV-1"],
    )?;
    post(&db, "2025-01-05", "January rent", "5000:30", "1000:30", &[])?;
    post(&db, "2025-01-20", "Invoice 2", "1000:50", "4000:50", &[])?;

    Ok((dir, db))
}

/// Run the ledger report as JSON and return the parsed envelope.
fn ledger_json(db: &str, extra: &[&str]) -> Result<Value, Box<dyn Error>> {
    let mut args = vec!["--json", "--company", CO, "report", "ledger"];
    args.extend_from_slice(extra);
    Ok(serde_json::from_str(&bk_ok(db, &args)?)?)
}

/// Find the statement for `code` in a ledger envelope.
fn account<'a>(env: &'a Value, code: &str) -> Result<&'a Value, Box<dyn Error>> {
    env["data"]["accounts"]
        .as_array()
        .ok_or("accounts is not an array")?
        .iter()
        .find(|a| a["code"] == code)
        .ok_or_else(|| format!("account {code} missing from ledger").into())
}

/// Collect a numeric field from every entry of an account statement.
fn entry_field(acct: &Value, field: &str) -> Result<Vec<i64>, Box<dyn Error>> {
    acct["entries"]
        .as_array()
        .ok_or("entries is not an array")?
        .iter()
        .map(|e| {
            e[field]
                .as_i64()
                .ok_or_else(|| format!("{field} not an int").into())
        })
        .collect()
}

#[test]
fn running_balance_is_ordered_by_date_and_debit_normal() -> TestResult {
    let (_dir, db) = setup()?;
    let env = ledger_json(&db, &["--account", "1000"])?;
    let cash = account(&env, "1000")?;

    assert_eq!(cash["normal_balance"], "debit");
    assert_eq!(cash["opening_balance"], 0);

    let dates: Vec<&str> = cash["entries"]
        .as_array()
        .ok_or("entries")?
        .iter()
        .filter_map(|e| e["date"].as_str())
        .collect();
    assert_eq!(dates, ["2025-01-05", "2025-01-10", "2025-01-20"]);

    assert_eq!(entry_field(cash, "debit")?, [0, 10_000, 5_000]);
    assert_eq!(entry_field(cash, "credit")?, [3_000, 0, 0]);
    assert_eq!(entry_field(cash, "balance")?, [-3_000, 7_000, 12_000]);
    assert_eq!(cash["total_debits"], 15_000);
    assert_eq!(cash["total_credits"], 3_000);
    assert_eq!(cash["closing_balance"], 12_000);
    Ok(())
}

#[test]
fn opening_balance_reflects_activity_before_from() -> TestResult {
    let (_dir, db) = setup()?;
    let env = ledger_json(
        &db,
        &[
            "--account",
            "1000",
            "--from",
            "2025-1-10",
            "--to",
            "2025-1-15",
        ],
    )?;
    let cash = account(&env, "1000")?;

    // The 2025-01-05 rent payment falls before --from.
    assert_eq!(cash["opening_balance"], -3_000);
    assert_eq!(entry_field(cash, "balance")?, [7_000]);
    assert_eq!(cash["closing_balance"], 7_000);
    assert_eq!(env["data"]["from"], "2025-01-10");
    assert_eq!(env["data"]["to"], "2025-01-15");
    Ok(())
}

#[test]
fn embedded_bounds_use_canonical_sql_dates_and_keep_open_ends() -> TestResult {
    let (_dir, path) = setup()?;
    let db = Db::open_existing(Path::new(&path), None)?;
    let opts = LedgerOptions {
        accounts: &["1000".to_string()],
        from: Some("2025-1-10".parse()?),
        to: Some("2025-1-10".parse()?),
        currency: None,
        filter_metadata: &[],
    };

    let bounded = build_report(db.read(), CO, &opts)?;
    let cash = bounded.accounts.first().ok_or("missing cash account")?;
    assert_eq!(bounded.from, Some("2025-01-10".parse()?));
    assert_eq!(bounded.to, Some("2025-01-10".parse()?));
    assert_eq!(cash.opening_balance, -3_000);
    assert_eq!(cash.entries.len(), 1);
    assert_eq!(
        cash.entries.first().ok_or("missing invoice")?.date,
        "2025-01-10".parse()?
    );
    assert_eq!(cash.closing_balance, 7_000);

    let no_start = build_report(db.read(), CO, &LedgerOptions { from: None, ..opts })?;
    let cash = no_start.accounts.first().ok_or("missing cash account")?;
    assert_eq!(no_start.from, None);
    assert_eq!(cash.opening_balance, 0);
    assert_eq!(cash.entries.len(), 2);
    assert_eq!(cash.closing_balance, 7_000);

    let no_end = build_report(db.read(), CO, &LedgerOptions { to: None, ..opts })?;
    let cash = no_end.accounts.first().ok_or("missing cash account")?;
    assert_eq!(no_end.to, None);
    assert_eq!(cash.entries.len(), 2);
    assert_eq!(cash.closing_balance, 12_000);
    Ok(())
}

#[test]
fn credit_normal_account_balances_increase_with_credits() -> TestResult {
    let (_dir, db) = setup()?;
    let env = ledger_json(&db, &["--account", "4000"])?;
    let sales = account(&env, "4000")?;

    assert_eq!(sales["type"], "revenue");
    assert_eq!(sales["normal_balance"], "credit");
    assert_eq!(entry_field(sales, "balance")?, [10_000, 15_000]);
    assert_eq!(sales["closing_balance"], 15_000);
    Ok(())
}

#[test]
fn json_shape_and_default_account_selection() -> TestResult {
    let (_dir, db) = setup()?;
    let env = ledger_json(&db, &[])?;

    assert_eq!(env["ok"], true);
    assert_eq!(env["meta"]["command"], "report.ledger");
    assert_eq!(env["meta"]["company"], CO);
    assert_eq!(env["data"]["currency"], "USD");
    assert!(env["data"]["from"].is_null());
    assert_eq!(
        env["data"]["to"],
        chrono::Local::now().date_naive().to_string()
    );

    // Only accounts with activity: 2000 (Loan) has none.
    let codes: Vec<&str> = env["data"]["accounts"]
        .as_array()
        .ok_or("accounts")?
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert_eq!(codes, ["1000", "4000", "5000"]);

    let cash = account(&env, "1000")?;
    for key in [
        "code",
        "name",
        "type",
        "normal_balance",
        "opening_balance",
        "entries",
        "total_debits",
        "total_credits",
        "closing_balance",
    ] {
        assert!(cash.get(key).is_some(), "account is missing key {key}");
    }
    let invoice = &cash["entries"][1];
    for key in [
        "date",
        "transaction_id",
        "entry_id",
        "description",
        "reference",
        "memo",
        "debit",
        "credit",
        "balance",
    ] {
        assert!(invoice.get(key).is_some(), "entry is missing key {key}");
    }
    assert_eq!(invoice["description"], "Invoice 1");
    assert_eq!(invoice["reference"], "INV-1");
    assert!(invoice["transaction_id"].is_i64());

    // An explicitly requested account is included even without activity.
    let env = ledger_json(&db, &["--account", "2000"])?;
    let loan = account(&env, "2000")?;
    assert_eq!(loan["normal_balance"], "credit");
    assert_eq!(loan["closing_balance"], 0);
    Ok(())
}

#[test]
fn multiple_currencies_require_currency_flag() -> TestResult {
    let (_dir, db) = setup()?;
    post(
        &db,
        "2025-01-25",
        "Euro sale",
        "1000:20",
        "4000:20",
        &["--currency", "EUR"],
    )?;

    let out = bk(&db, &["--company", CO, "report", "ledger"])?;
    assert!(
        !out.status.success(),
        "ledger should fail with mixed currencies"
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "expected a validation error exit code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--currency"),
        "error should mention --currency: {stderr}"
    );

    let env = ledger_json(&db, &["--account", "1000", "--currency", "EUR"])?;
    assert_eq!(env["data"]["currency"], "EUR");
    let cash = account(&env, "1000")?;
    assert_eq!(entry_field(cash, "balance")?, [2_000]);

    let env = ledger_json(&db, &["--account", "1000", "--currency", "USD"])?;
    assert_eq!(account(&env, "1000")?["closing_balance"], 12_000);
    Ok(())
}

#[test]
fn csv_has_opening_entry_and_closing_rows() -> TestResult {
    let (_dir, db) = setup()?;
    let csv = bk_ok(
        &db,
        &[
            "--format",
            "csv",
            "--company",
            CO,
            "report",
            "ledger",
            "--account",
            "5000",
        ],
    )?;
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "header + opening + 1 entry + closing: {csv}"
    );
    assert!(lines[0].starts_with("account_code,account_name,account_type,currency,row_type"));
    assert!(lines[1].contains(",opening,"));
    assert!(lines[2].contains(",entry,2025-01-05,"));
    assert!(lines[2].ends_with(",3000,0,3000"));
    assert!(lines[3].contains(",closing,"));
    assert!(lines[3].ends_with(",3000,0,3000"));
    Ok(())
}

#[test]
fn table_output_renders_statement() -> TestResult {
    let (_dir, db) = setup()?;
    let out = bk_ok(
        &db,
        &[
            "--no-color",
            "--company",
            CO,
            "report",
            "ledger",
            "--account",
            "1000",
        ],
    )?;
    assert!(out.contains(&format!(
        "General Ledger (through {}) (USD)",
        chrono::Local::now().date_naive()
    )));
    assert!(out.contains("Opening balance"));
    assert!(out.contains("Closing balance"));
    assert!(out.contains("120.00"));
    Ok(())
}

#[test]
fn ledger_rejects_running_balance_overflow_before_rendering() -> TestResult {
    let (_dir, db) = setup()?;
    for (date, amount) in [("2026-01-12", "9223372036854775807"), ("2026-01-13", "1")] {
        post(
            &db,
            date,
            "Large ledger entry",
            &format!("1000:{amount}"),
            &format!("4000:{amount}"),
            &["--currency", "JPY"],
        )?;
    }
    let output = bk(
        &db,
        &[
            "--json",
            "--company",
            CO,
            "report",
            "ledger",
            "--account",
            "1000",
            "--currency",
            "JPY",
            "--to",
            "2026-01-15",
        ],
    )?;
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, [] as [u8; 0]);
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert!(error["error"]["message"].as_str().is_some_and(|message| {
        message.contains("ledger balance exceeds the supported minor-unit range")
    }));
    Ok(())
}
