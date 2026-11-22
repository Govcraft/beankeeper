//! Regression tests for issues #20 and #27.
//!
//! Reports must never add minor units across currencies. A company with
//! transactions in several currencies must pick one with `--currency`; a
//! single-currency company has its currency inferred (including by
//! `report budget-variance`, which used to default to USD).

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::tempdir;

/// Result of a `bk` invocation.
struct Run {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `bk` against the given database, with `BEANKEEPER_CURRENCY` cleared so
/// the caller's environment cannot influence currency resolution.
fn run_bk(db_path: &str, args: &[&str]) -> Run {
    let mut cmd =
        Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.env_remove("BEANKEEPER_CURRENCY");
    cmd.arg("--db").arg(db_path);
    cmd.args(args);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("bk should execute: {e}"));
    Run {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Run `bk` and assert it succeeds.
fn ok(db_path: &str, args: &[&str]) -> Run {
    let run = run_bk(db_path, args);
    assert!(
        run.success,
        "bk {args:?} should succeed\nstdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    run
}

/// Run a `--json` command for company `co` and return the envelope's `data`.
fn json_data(db_path: &str, args: &[&str]) -> Value {
    let mut full = vec!["--json", "--company", "co"];
    full.extend_from_slice(args);
    let run = ok(db_path, &full);
    let v: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout is JSON ({e}): {}", run.stdout));
    v.get("data")
        .cloned()
        .unwrap_or_else(|| panic!("envelope has data: {v}"))
}

/// Post a sale of `amount` (major units) in `currency`.
fn post_sale(db_path: &str, amount: &str, currency: &str) {
    let debit = format!("1000:{amount}");
    let credit = format!("4000:{amount}");
    ok(
        db_path,
        &[
            "--company",
            "co",
            "txn",
            "post",
            "-d",
            "sale",
            "--debit",
            &debit,
            "--credit",
            &credit,
            "--currency",
            currency,
            "--date",
            "2026-03-15",
        ],
    );
}

/// Create a fresh database with company `co` (cash 1000, revenue 4000, an
/// unused payable 2000) and the given sales posted as `(amount, currency)`.
fn setup(sales: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("create tempdir: {e}"));
    let db_path = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("path is utf-8"))
        .to_string();

    ok(&db_path, &["init"]);
    ok(&db_path, &["company", "create", "co", "Co"]);
    for (code, name, ty) in [
        ("1000", "Cash", "asset"),
        ("2000", "Payables", "liability"),
        ("4000", "Revenue", "revenue"),
    ] {
        ok(
            &db_path,
            &[
                "--company",
                "co",
                "account",
                "create",
                code,
                name,
                "--type",
                ty,
            ],
        );
    }
    for (amount, currency) in sales {
        post_sale(&db_path, amount, currency);
    }
    (dir, db_path)
}

fn as_i64(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("{key} is an integer in {v}"))
}

fn account<'a>(data: &'a Value, code: &str) -> &'a Value {
    data.get("accounts")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|row| row.get("code").and_then(Value::as_str) == Some(code))
        })
        .unwrap_or_else(|| panic!("account {code} listed in {data}"))
}

#[test]
fn mixed_currency_reports_require_currency_flag() {
    let (_dir, db) = setup(&[("100.00", "USD"), ("250.00", "MXN")]);

    for args in [
        vec!["report", "trial-balance"],
        vec!["report", "balance", "--account", "1000"],
        vec!["report", "income-statement"],
        vec!["report", "balance-sheet"],
        vec!["report", "tax-summary"],
        vec!["report", "budget-variance", "--year", "2026"],
    ] {
        let mut full = vec!["--company", "co"];
        full.extend_from_slice(&args);
        let run = run_bk(&db, &full);
        assert!(
            !run.success,
            "{args:?} must fail for a mixed-currency company"
        );
        assert_eq!(run.code, Some(3), "{args:?} is a validation error");
        let out = format!("{}{}", run.stdout, run.stderr);
        assert!(
            out.contains("--currency"),
            "{args:?} hints at --currency: {out}"
        );
        assert!(
            out.contains("MXN") && out.contains("USD"),
            "{args:?} lists the currencies found: {out}"
        );
    }
}

#[test]
fn currency_flag_limits_totals_to_that_currency() {
    let (_dir, db) = setup(&[("100.00", "USD"), ("250.00", "MXN"), ("5.00", "MXN")]);

    let tb = json_data(&db, &["report", "trial-balance", "--currency", "MXN"]);
    assert_eq!(as_i64(&tb, "total_debits"), 25_500);
    assert_eq!(as_i64(&tb, "total_credits"), 25_500);
    assert_eq!(as_i64(account(&tb, "1000"), "debit_total"), 25_500);
    // Zero-balance accounts are still listed under a currency filter.
    assert_eq!(as_i64(account(&tb, "2000"), "debit_total"), 0);
    assert_eq!(
        tb.get("accounts").and_then(Value::as_array).map(Vec::len),
        Some(3)
    );

    let tb_usd = json_data(&db, &["report", "trial-balance", "--currency", "USD"]);
    assert_eq!(as_i64(&tb_usd, "total_debits"), 10_000);

    let bal = json_data(
        &db,
        &[
            "report",
            "balance",
            "--account",
            "1000",
            "--currency",
            "MXN",
        ],
    );
    assert_eq!(as_i64(&bal, "debit_total"), 25_500);
    assert_eq!(bal.get("currency").and_then(Value::as_str), Some("MXN"));

    let is = json_data(&db, &["report", "income-statement", "--currency", "MXN"]);
    assert_eq!(as_i64(&is, "total_revenue"), 25_500);
    assert_eq!(as_i64(&is, "net_income"), 25_500);

    let bs = json_data(&db, &["report", "balance-sheet", "--currency", "USD"]);
    assert_eq!(as_i64(&bs, "total_assets"), 10_000);
    assert_eq!(bs.get("balanced").and_then(Value::as_bool), Some(true));

    // An invalid code is rejected rather than silently replaced by USD.
    let run = run_bk(
        &db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--currency",
            "XYZ",
        ],
    );
    assert_eq!(run.code, Some(3));
}

#[test]
fn single_currency_company_infers_its_currency() {
    let (_dir, db) = setup(&[("100.00", "MXN"), ("20.00", "MXN")]);

    let bal = json_data(&db, &["report", "balance", "--account", "1000"]);
    assert_eq!(bal.get("currency").and_then(Value::as_str), Some("MXN"));
    assert_eq!(as_i64(&bal, "debit_total"), 12_000);

    let tb = json_data(&db, &["report", "trial-balance"]);
    assert_eq!(as_i64(&tb, "total_debits"), 12_000);

    let run = ok(&db, &["--company", "co", "report", "trial-balance"]);
    assert!(
        run.stdout.contains("(MXN)"),
        "table title shows MXN: {}",
        run.stdout
    );
}

#[test]
fn budget_variance_infers_company_currency() {
    let (_dir, db) = setup(&[("100.00", "MXN")]);

    let bv = json_data(
        &db,
        &[
            "report",
            "budget-variance",
            "--year",
            "2026",
            "--type",
            "revenue",
            "--include-unbudgeted",
        ],
    );
    assert_eq!(bv.get("currency").and_then(Value::as_str), Some("MXN"));
    let totals = bv
        .get("totals")
        .unwrap_or_else(|| panic!("totals present: {bv}"));
    assert_eq!(as_i64(totals, "actual"), 10_000);
}

#[test]
fn budget_variance_keeps_month_validation_in_the_command() {
    let (_dir, db) = setup(&[]);

    for (args, message) in [
        (["--month", "13"], "month must be 1-12, got 13"),
        (["--from", "0"], "from/to months must be 1-12"),
    ] {
        let mut command = vec![
            "--company",
            "co",
            "report",
            "budget-variance",
            "--year",
            "2026",
        ];
        command.extend(args);
        let result = run_bk(&db, &command);
        assert_eq!(result.code, Some(3), "{result:?}", result = result.stderr);
        assert!(result.stderr.contains(message), "{}", result.stderr);
    }
}

#[test]
fn company_without_transactions_defaults_to_usd() {
    let (_dir, db) = setup(&[]);
    let bal = json_data(&db, &["report", "balance", "--account", "1000"]);
    assert_eq!(bal.get("currency").and_then(Value::as_str), Some("USD"));
}
