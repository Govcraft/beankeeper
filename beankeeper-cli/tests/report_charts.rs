//! Chart projections preserve source reports and exact signed money.

use std::error::Error;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger(TempDir);

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let ledger = Self(tempfile::tempdir()?);
        ledger.ok(&["init"])?;
        ledger.ok(&["company", "create", "co", "Company"])?;
        for (code, name, kind) in [
            ("1000", "Cash", "asset"),
            ("2000", "Debt", "liability"),
            ("3000", "Equity", "equity"),
            ("4000", "Sales", "revenue"),
        ] {
            ledger.ok(&["account", "create", code, name, "--type", kind])?;
        }
        for index in 0..10 {
            // Duplicate display names must never replace account identifiers.
            ledger.ok(&[
                "account",
                "create",
                &format!("50{index:02}"),
                "Same, label",
                "--type",
                "expense",
            ])?;
        }
        Ok(ledger)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .args(["--company", "co", "--quiet", "--db"])
            .arg(self.0.path().join("books.db"))
            .args(args);
        for name in [
            "BEANKEEPER_PASSPHRASE_CMD",
            "BK_AGENT_TOKEN",
            "BEANKEEPER_CURRENCY",
            "BK_ACTOR",
        ] {
            command.env_remove(name);
        }
        Ok(command.output()?)
    }

    fn ok(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = self.run(args)?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    fn report(&self, args: &[&str], chart: bool, all: bool) -> Result<Value, Box<dyn Error>> {
        let mut flags = vec!["--json"];
        if chart {
            flags.push("--chart");
        }
        if all {
            flags.push("--chart-all");
        }
        flags.extend(args);
        Ok(serde_json::from_str(&self.ok(&flags)?)?)
    }

    fn post(&self, date: &str, debit: &str, credit: &str, currency: &str) -> TestResult {
        self.ok(&[
            "txn",
            "post",
            "-d",
            "chart fixture",
            "--date",
            date,
            "--debit",
            debit,
            "--credit",
            credit,
            "--currency",
            currency,
        ])?;
        Ok(())
    }
}

fn validate(chart: &Value, currency: &str) {
    assert_eq!(chart["schema_version"], 1);
    assert!(matches!(chart["kind"].as_str(), Some("line" | "bar")));
    assert!(
        chart["title"]
            .as_str()
            .is_some_and(|title| !title.is_empty())
    );
    let x = chart["x"]
        .as_array()
        .unwrap_or_else(|| panic!("x: {chart}"));
    assert!(x.iter().all(Value::is_string));
    assert_eq!(chart["x_accounts"].as_array().map(Vec::len), Some(x.len()));
    for series in chart["series"]
        .as_array()
        .unwrap_or_else(|| panic!("series: {chart}"))
    {
        assert_eq!(series["currency"], currency);
        assert_eq!(series["unit"], "major");
        let values = series["values"]
            .as_array()
            .unwrap_or_else(|| panic!("values: {series}"));
        assert_eq!(values.len(), x.len());
        assert!(values.iter().all(|value| {
            value
                .as_str()
                .is_some_and(|text| text.parse::<Decimal>().is_ok())
        }));
    }
    assert_eq!(chart["desktop_view"]["state"]["company"], "co");
    assert_eq!(chart["desktop_view"]["state"]["currency"], currency);
}

#[test]
fn requested_charts_are_additive_for_every_supported_report() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2023-12-31", "1000:1000", "3000:1000", "USD")?;
    ledger.post("2024-01-15", "5000:12.34", "1000:12.34", "USD")?;
    ledger.post("2024-03-10", "1000:20", "5000:20", "USD")?;
    for args in [
        vec!["summary"],
        vec!["report", "trend", "--metric", "spending"],
        vec!["report", "trend", "--metric", "net-worth"],
        vec!["report", "cash-flow"],
        vec!["report", "income-statement"],
        vec!["report", "income-statement", "--period", "month"],
        vec!["report", "income-statement", "--compare", "prior-period"],
        vec!["report", "trial-balance", "--period", "month"],
        vec!["report", "trial-balance", "--compare", "prior-year"],
    ] {
        let mut args = args;
        args.extend(["--from", "2024-01-01", "--to", "2024-03-31"]);
        let original = ledger.report(&args, false, false)?;
        assert!(original["data"].get("chart").is_none());
        let mut projected = ledger.report(&args, true, false)?;
        validate(&projected["data"]["chart"], "USD");
        let state = &projected["data"]["chart"]["desktop_view"]["state"];
        assert_eq!(state["from"], "2024-01-01");
        assert_eq!(state["to"], "2024-03-31");
        projected["data"]
            .as_object_mut()
            .ok_or("data")?
            .remove("chart");
        assert_eq!(projected["data"], original["data"], "{args:?}");
        assert_eq!(projected["ok"], original["ok"]);
        assert_eq!(projected["meta"]["command"], original["meta"]["command"]);
    }
    Ok(())
}

fn totals(chart: &Value) -> Result<Vec<Decimal>, Box<dyn Error>> {
    let width = chart["x"].as_array().ok_or("x")?.len();
    let mut totals = vec![Decimal::ZERO; width];
    for row in chart["series"].as_array().ok_or("series")? {
        for (sum, value) in totals
            .iter_mut()
            .zip(row["values"].as_array().ok_or("values")?)
        {
            *sum += value.as_str().ok_or("decimal string")?.parse::<Decimal>()?;
        }
    }
    Ok(totals)
}

#[test]
fn top_eight_plus_other_preserves_signed_totals_and_account_identifiers() -> TestResult {
    let ledger = Ledger::new()?;
    for index in 0..10 {
        ledger.post(
            "2024-01-15",
            &format!("50{index:02}:{}", index + 1),
            &format!("1000:{}", index + 1),
            "USD",
        )?;
    }
    ledger.post("2024-03-10", "1000:20", "5000:20", "USD")?;
    let args = [
        "report",
        "trend",
        "--metric",
        "spending",
        "--from",
        "2024-01-01",
        "--to",
        "2024-03-31",
    ];
    let compact = ledger.report(&args, true, false)?;
    let all = ledger.report(&args, true, true)?;
    let compact = &compact["data"]["chart"];
    let all = &all["data"]["chart"];
    assert_eq!(compact["series"].as_array().map(Vec::len), Some(9));
    assert_eq!(compact["truncated"], true);
    assert_eq!(compact["omitted_count"], 2);
    assert_eq!(compact["series"][8]["name"], "Other");
    assert!(compact["series"][8].get("account").is_none());
    assert_eq!(all["series"].as_array().map(Vec::len), Some(10));
    assert_eq!(all["truncated"], false);
    assert_eq!(compact["series"][0]["account"], "5000"); // largest absolute movement
    assert_eq!(
        all["series"][0]["values"],
        json!(["1.00", "0.00", "-20.00"])
    );
    assert_eq!(totals(compact)?, totals(all)?);
    assert_eq!(
        totals(compact)?,
        vec![Decimal::from(55), Decimal::ZERO, Decimal::from(-20)]
    );
    let state = &all["desktop_view"]["state"];
    assert_eq!(state["period"], "month");
    assert_eq!(state["metric"], "spending");
    assert!(state.get("category").is_none());
    for args in [
        vec!["report", "income-statement"],
        vec!["report", "cash-flow"],
    ] {
        let mut args = args;
        args.extend(["--from", "2024-01-01", "--to", "2024-03-31"]);
        let compact = ledger.report(&args, true, false)?;
        let all = ledger.report(&args, true, true)?;
        let compact = &compact["data"]["chart"];
        let all = &all["data"]["chart"];
        assert_eq!(compact["x"].as_array().map(Vec::len), Some(9));
        assert_eq!(all["x"].as_array().map(Vec::len), Some(10));
        assert_eq!(compact["x"][8], "Other");
        assert!(compact["x_accounts"][8].is_null());
        assert_eq!(
            totals(compact)?.iter().sum::<Decimal>(),
            totals(all)?.iter().sum::<Decimal>()
        );
    }
    Ok(())
}

#[test]
fn currency_scales_empty_books_and_period_end_balances_remain_exact() -> TestResult {
    let ledger = Ledger::new()?;
    for (currency, amount) in [("JPY", "123"), ("KWD", "1.234")] {
        ledger.post(
            "2023-12-31",
            &format!("1000:{amount}"),
            &format!("3000:{amount}"),
            currency,
        )?;
        let report = ledger.report(
            &[
                "report",
                "trend",
                "--metric",
                "net-worth",
                "--from",
                "2024-01-15",
                "--to",
                "2024-02-20",
                "--currency",
                currency,
            ],
            true,
            false,
        )?;
        let chart = &report["data"]["chart"];
        validate(chart, currency);
        assert_eq!(chart["series"][2]["values"], json!([amount, amount]));
        assert_eq!(chart["x"], json!(["2024-01", "2024-02"]));
        assert_eq!(chart["desktop_view"]["state"]["to"], "2024-02-20");
    }
    let output = ledger.run(&[
        "--json", "--chart", "report", "trend", "--metric", "spending",
    ])?;
    assert_eq!(output.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    let empty = Ledger::new()?;
    for args in [
        vec!["summary"],
        vec!["report", "trend", "--metric", "net-worth"],
        vec!["report", "cash-flow"],
        vec!["report", "income-statement"],
    ] {
        let report = empty.report(&args, true, false)?;
        validate(&report["data"]["chart"], "USD");
    }
    Ok(())
}

#[test]
fn category_axes_and_comparison_targets_keep_resolved_identifiers() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-15", "5000:12.34", "1000:12.34", "USD")?;
    let report = ledger.report(
        &["report", "income-statement", "--to", "2024-02-29"],
        true,
        true,
    )?;
    let chart = &report["data"]["chart"];
    assert_eq!(chart["x_accounts"][0], "5000");
    assert_eq!(chart["x"][0], "Same, label");
    assert_eq!(chart["series"][0]["values"][0], "12.34");
    let cash = ledger.report(&["report", "cash-flow", "--to", "2024-02-29"], true, false)?;
    assert_eq!(
        cash["data"]["chart"]["series"][0]["values"],
        json!(["-12.34"])
    );
    assert_eq!(
        cash["data"]["chart"]["desktop_view"]["state"]["cash_account"],
        json!(["1000"])
    );
    let report = ledger.report(
        &[
            "report",
            "income-statement",
            "--from",
            "2024-03-01",
            "--to",
            "2024-03-31",
            "--compare",
            "prior-period",
        ],
        true,
        true,
    )?;
    assert_eq!(
        report["data"]["chart"]["x"],
        json!(["2024-03-01..2024-03-31", "2024-02-01..2024-02-29"])
    );
    assert_eq!(report["data"]["mode"], "compare");
    assert!(report["data"]["summary"].as_str().is_some());
    let state = &report["data"]["chart"]["desktop_view"]["state"];
    assert_eq!(state["compare_from"], "2024-02-01");
    assert_eq!(state["compare_to"], "2024-02-29");
    let summary = ledger.report(&["summary", "--to", "2024-03-31"], true, false)?;
    assert_eq!(summary["data"]["chart"]["x"][0], "2024-02-01..2024-02-29");
    Ok(())
}

#[test]
fn invalid_chart_requests_fail_with_named_errors() -> TestResult {
    let ledger = Ledger::new()?;
    for args in [
        vec!["--chart", "summary"],
        vec!["--chart", "--format", "csv", "report", "cash-flow"],
        vec!["--json", "--chart", "report", "balance-sheet"],
        vec!["--json", "--chart", "txn", "list"],
        vec!["--json", "--chart-all", "summary"],
    ] {
        let output = ledger.run(&args)?;
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    Ok(())
}
