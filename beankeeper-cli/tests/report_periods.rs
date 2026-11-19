//! Integration tests for multi-column reports (issue #22):
//! `report income-statement|trial-balance --period ...` and `--compare ...`.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

/// Output of one `bk` invocation.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn bk(db_path: &str, args: &[&str]) -> Run {
    let mut cmd =
        Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.arg("--db").arg(db_path).arg("--quiet").args(args);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("bk should execute: {e}"));
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn bk_ok(db_path: &str, args: &[&str]) -> String {
    let run = bk(db_path, args);
    assert_eq!(
        run.code, 0,
        "bk {args:?} failed: stdout={} stderr={}",
        run.stdout, run.stderr
    );
    run.stdout
}

fn bk_json(db_path: &str, args: &[&str]) -> Value {
    let mut full = vec!["--json", "--company", "co"];
    full.extend_from_slice(args);
    let stdout = bk_ok(db_path, &full);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
}

/// A company with revenue/expense activity spread over 2025 and 2026.
///
/// Amounts are in dollars; JSON reports minor units (cents).
fn setup() -> (TempDir, String) {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("utf-8 path"))
        .to_string();

    bk_ok(&db, &["init"]);
    bk_ok(&db, &["company", "create", "co", "Co"]);
    for (code, name, ty) in [
        ("1000", "Cash", "asset"),
        ("3000", "Equity", "equity"),
        ("4000", "Sales", "revenue"),
        ("5000", "Rent", "expense"),
    ] {
        bk_ok(
            &db,
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

    for (debit, credit, date) in [
        ("1000:1000", "4000:1000", "2025-01-10"),
        ("5000:300", "1000:300", "2025-02-05"),
        ("1000:2000", "4000:2000", "2026-01-15"),
        ("1000:500", "4000:500", "2026-02-20"),
        ("5000:400", "1000:400", "2026-03-03"),
        ("1000:800", "4000:800", "2026-05-02"),
    ] {
        bk_ok(
            &db,
            &[
                "--company",
                "co",
                "txn",
                "post",
                "-d",
                "test",
                "--debit",
                debit,
                "--credit",
                credit,
                "--date",
                date,
            ],
        );
    }
    (dir, db)
}

fn i64s(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap_or_else(|| panic!("expected array, got {v}"))
        .iter()
        .map(|x| {
            x.as_i64()
                .unwrap_or_else(|| panic!("expected int, got {x}"))
        })
        .collect()
}

fn account<'a>(data: &'a Value, code: &str) -> &'a Value {
    data["accounts"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["code"] == code))
        .unwrap_or_else(|| panic!("account {code} missing in {data}"))
}

#[test]
fn income_statement_monthly_columns() {
    let (_dir, db) = setup();
    let v = bk_json(
        &db,
        &[
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
            "--period",
            "month",
        ],
    );
    assert_eq!(v["meta"]["command"], "report.income-statement.periods");
    let data = &v["data"];
    assert_eq!(data["mode"], "periods");
    assert_eq!(data["period"], "month");

    let labels: Vec<&str> = data["columns"]
        .as_array()
        .unwrap_or_else(|| panic!("columns"))
        .iter()
        .filter_map(|c| c["label"].as_str())
        .collect();
    assert_eq!(labels, ["2026-01", "2026-02", "2026-03"]);
    assert_eq!(data["columns"][1]["to"], "2026-02-28");

    let sales = account(data, "4000");
    assert_eq!(i64s(&sales["amounts"]), [200_000, 50_000, 0]);
    assert_eq!(sales["total"], 250_000);
    let rent = account(data, "5000");
    assert_eq!(i64s(&rent["amounts"]), [0, 0, 40_000]);

    // Only revenue and expense accounts appear, revenue first.
    let codes: Vec<&str> = data["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("accounts"))
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert_eq!(codes, ["4000", "5000"]);

    let net = &data["totals"]["net_income"];
    assert_eq!(i64s(&net["amounts"]), [200_000, 50_000, -40_000]);
    assert_eq!(net["total"], 210_000);

    // Table output has one column per month plus Total and a Net Income row.
    let table = bk_ok(
        &db,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
            "--period",
            "month",
        ],
    );
    for needle in [
        "Income Statement by Month",
        "2026-01",
        "2026-02",
        "2026-03",
        "Total",
        "Net Income",
        "2,100.00",
        "-400.00",
    ] {
        assert!(table.contains(needle), "missing {needle:?} in:\n{table}");
    }
}

#[test]
fn trial_balance_quarterly_columns_are_clipped() {
    let (_dir, db) = setup();
    let v = bk_json(
        &db,
        &[
            "report",
            "trial-balance",
            "--from",
            "2025-11-15",
            "--to",
            "2026-05-10",
            "--period",
            "quarter",
        ],
    );
    assert_eq!(v["meta"]["command"], "report.trial-balance.periods");
    let data = &v["data"];
    let cols = data["columns"].as_array().unwrap_or_else(|| panic!("cols"));
    let labels: Vec<&str> = cols.iter().filter_map(|c| c["label"].as_str()).collect();
    assert_eq!(labels, ["2025-Q4", "2026-Q1", "2026-Q2"]);
    assert_eq!(cols[0]["from"], "2025-11-15");
    assert_eq!(cols[0]["to"], "2025-12-31");
    assert_eq!(cols[1]["from"], "2026-01-01");
    assert_eq!(cols[1]["to"], "2026-03-31");
    assert_eq!(cols[2]["from"], "2026-04-01");
    assert_eq!(cols[2]["to"], "2026-05-10");

    // Cash (debit-normal): +2000 +500 -400 in Q1, +800 in Q2.
    let cash = account(data, "1000");
    assert_eq!(cash["normal_balance"], "debit");
    assert_eq!(i64s(&cash["amounts"]), [0, 210_000, 80_000]);
    assert_eq!(cash["total"], 290_000);
    // Sales (credit-normal) shows a positive amount.
    assert_eq!(
        i64s(&account(data, "4000")["amounts"]),
        [0, 250_000, 80_000]
    );

    let totals = &data["totals"];
    assert_eq!(
        i64s(&totals["total_debits"]["amounts"]),
        i64s(&totals["total_credits"]["amounts"])
    );

    // CSV: one header column per quarter plus total; amounts in minor units.
    let csv = bk_ok(
        &db,
        &[
            "--format",
            "csv",
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2025-11-15",
            "--to",
            "2026-05-10",
            "--period",
            "quarter",
        ],
    );
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some("code,name,type,2025-Q4,2026-Q1,2026-Q2,total")
    );
    assert!(
        csv.contains("1000,Cash,asset,0,210000,80000,290000"),
        "{csv}"
    );
}

#[test]
fn income_statement_prior_year_compare() {
    let (_dir, db) = setup();
    let v = bk_json(
        &db,
        &[
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
            "--compare",
            "prior-year",
        ],
    );
    assert_eq!(v["meta"]["command"], "report.income-statement.compare");
    let data = &v["data"];
    assert_eq!(data["compare"], "prior-year");
    assert_eq!(data["current"]["label"], "2026-Q1");
    assert_eq!(data["prior"]["label"], "2025-Q1");
    assert_eq!(data["prior"]["from"], "2025-01-01");
    assert_eq!(data["prior"]["to"], "2025-03-31");

    let sales = account(data, "4000");
    assert_eq!(sales["current"], 250_000);
    assert_eq!(sales["prior"], 100_000);
    assert_eq!(sales["change"], 150_000);
    assert_eq!(sales["change_percent"], 150.0);

    let net = &data["totals"]["net_income"];
    assert_eq!(net["current"], 210_000);
    assert_eq!(net["prior"], 70_000);
    assert_eq!(net["change"], 140_000);
    assert_eq!(net["change_percent"], 200.0);

    // Q2 2026 vs Q2 2025: no prior revenue, so the percent is null / n/a.
    let v = bk_json(
        &db,
        &[
            "report",
            "income-statement",
            "--from",
            "2026-04-01",
            "--to",
            "2026-06-30",
            "--compare",
            "prior-year",
        ],
    );
    let sales = account(&v["data"], "4000");
    assert_eq!(sales["current"], 80_000);
    assert_eq!(sales["prior"], 0);
    assert!(sales["change_percent"].is_null());

    let table = bk_ok(
        &db,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--from",
            "2026-04-01",
            "--to",
            "2026-06-30",
            "--compare",
            "prior-year",
        ],
    );
    for needle in [
        "2026-Q2",
        "2025-Q2",
        "Change",
        "Change %",
        "n/a",
        "Net Income",
    ] {
        assert!(table.contains(needle), "missing {needle:?} in:\n{table}");
    }
}

#[test]
fn prior_period_compare_uses_preceding_quarter() {
    let (_dir, db) = setup();
    let v = bk_json(
        &db,
        &[
            "report",
            "income-statement",
            "--from",
            "2026-04-01",
            "--to",
            "2026-06-30",
            "--compare",
            "prior-period",
        ],
    );
    let data = &v["data"];
    assert_eq!(data["prior"]["from"], "2026-01-01");
    assert_eq!(data["prior"]["to"], "2026-03-31");
    let sales = account(data, "4000");
    assert_eq!(sales["current"], 80_000);
    assert_eq!(sales["prior"], 250_000);
    assert_eq!(sales["change"], -170_000);
    assert_eq!(sales["change_percent"], -68.0);
}

#[test]
fn default_income_statement_is_single_column() {
    let (_dir, db) = setup();
    let v = bk_json(
        &db,
        &[
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
        ],
    );
    assert_eq!(v["meta"]["command"], "report.income-statement");
    assert!(v["data"]["columns"].is_null());
    assert!(v["data"]["total_debits"].is_i64());
}

#[test]
fn period_requires_from_and_to() {
    let (_dir, db) = setup();
    for args in [
        vec!["report", "income-statement", "--period", "month"],
        vec![
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--period",
            "quarter",
        ],
        vec![
            "report",
            "trial-balance",
            "--to",
            "2026-01-31",
            "--period",
            "year",
        ],
    ] {
        let mut full = vec!["--company", "co"];
        full.extend(args.iter().copied());
        let run = bk(&db, &full);
        assert_eq!(run.code, 3, "{args:?}: {}", run.stderr);
        assert!(
            run.stderr
                .contains("--period requires both --from and --to"),
            "{args:?}: {}",
            run.stderr
        );
    }

    let run = bk(
        &db,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--to",
            "2026-03-31",
            "--compare",
            "prior-year",
        ],
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("--compare requires both --from and --to"),
        "{}",
        run.stderr
    );

    // JSON mode reports a VALIDATION error with the mode's command id.
    let run = bk(
        &db,
        &[
            "--json",
            "--company",
            "co",
            "report",
            "income-statement",
            "--period",
            "month",
        ],
    );
    assert_eq!(run.code, 3);
    assert!(run.stderr.contains("\"VALIDATION\""), "{}", run.stderr);
    assert!(
        run.stderr.contains("report.income-statement.periods"),
        "{}",
        run.stderr
    );
}

#[test]
fn invalid_dates_are_rejected() {
    let (_dir, db) = setup();
    let run = bk(
        &db,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--from",
            "2026-03-31",
            "--to",
            "2026-01-01",
            "--period",
            "month",
        ],
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(
        run.stderr.contains("must be on or before"),
        "{}",
        run.stderr
    );

    let run = bk(
        &db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2026-13-01",
            "--to",
            "2026-12-31",
            "--period",
            "month",
        ],
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stderr.contains("invalid --from date"), "{}", run.stderr);
}

#[test]
fn period_and_compare_conflict() {
    let (_dir, db) = setup();
    let run = bk(
        &db,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
            "--period",
            "month",
            "--compare",
            "prior-year",
        ],
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("cannot be used with"), "{}", run.stderr);
}
