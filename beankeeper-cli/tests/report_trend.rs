//! Calendar activity and cumulative balance behavior for historical trends.

use std::error::Error;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let ledger = Self {
            dir: tempfile::tempdir()?,
        };
        ledger.ok(&["init"])?;
        ledger.ok(&["company", "create", "co", "Company"])?;
        for (code, name, kind) in [
            ("1000", "Cash", "asset"),
            ("2000", "Loan", "liability"),
            ("3000", "Equity", "equity"),
            ("4000", "Sales", "revenue"),
            ("5000", "Rent, office", "expense"),
            ("5100", "Supplies", "expense"),
            ("5200", "Unused", "expense"),
        ] {
            ledger.ok(&["account", "create", code, name, "--type", kind])?;
        }
        Ok(ledger)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(self.dir.path().join("books.db"))
            .arg("--quiet");
        if !args.contains(&"--company") {
            command.args(["--company", "co"]);
        }
        Ok(command
            .args(args)
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BK_AGENT_TOKEN")
            .output()?)
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

    fn post(&self, date: &str, debit: &str, credit: &str, currency: &str) -> TestResult {
        self.ok(&[
            "txn",
            "post",
            "-d",
            "trend fixture",
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

    fn report(&self, metric: &str, options: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut args = vec!["--json", "report", "trend", "--metric", metric];
        args.extend(options);
        Ok(serde_json::from_str(&self.ok(&args)?)?)
    }
}

fn series<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["data"]["series"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .unwrap_or_else(|| panic!("missing series {name}: {report}"))
}

#[test]
fn monthly_spending_includes_refunds_zero_categories_and_clipped_boundaries() -> TestResult {
    let ledger = Ledger::new()?;
    for (date, debit, credit) in [
        ("2024-01-14", "5000:99", "1000:99"), // excluded
        ("2024-01-15", "5000:100", "1000:100"),
        ("2024-02-29", "5100:30", "1000:30"),
        ("2024-03-10", "1000:120", "5000:120"), // refund
        ("2024-03-11", "5000:88", "1000:88"),   // excluded
    ] {
        ledger.post(date, debit, credit, "USD")?;
    }
    let report = ledger.report("spending", &["--from", "2024-01-15", "--to", "2024-03-10"])?;
    assert_eq!(report["meta"]["command"], "report.trend");
    assert_eq!(report["meta"]["currency"], "USD");
    assert_eq!(report["data"]["period"], "month");
    assert_eq!(
        report["data"]["columns"],
        json!([
            {"label":"2024-01", "from":"2024-01-15", "to":"2024-01-31"},
            {"label":"2024-02", "from":"2024-02-01", "to":"2024-02-29"},
            {"label":"2024-03", "from":"2024-03-01", "to":"2024-03-10"}
        ])
    );
    assert_eq!(series(&report, "Rent, office")["code"], "5000");
    assert_eq!(
        series(&report, "Rent, office")["amounts"],
        json!([10000, 0, -12000])
    );
    assert_eq!(
        series(&report, "Total Spending")["amounts"],
        json!([10000, 3000, -12000])
    );
    assert_eq!(
        series(&report, "Total Spending")["amounts_display"],
        json!(["100.00", "30.00", "-120.00"])
    );
    assert_eq!(series(&report, "Unused")["amounts"], json!([0, 0, 0]));
    assert_eq!(report["data"]["series"].as_array().map(Vec::len), Some(4));
    Ok(())
}

#[test]
fn net_worth_carries_opening_balances_through_empty_months_and_ignores_future_entries() -> TestResult
{
    let ledger = Ledger::new()?;
    for (date, debit, credit) in [
        ("2023-12-31", "1000:1000", "3000:1000"),
        ("2023-12-31", "1000:500", "2000:500"),
        ("2024-01-01", "5000:100", "1000:100"),
        ("2024-03-10", "2000:200", "1000:200"), // debt repayment leaves worth unchanged
        ("2024-03-11", "5000:99", "1000:99"),
    ] {
        ledger.post(date, debit, credit, "USD")?;
    }
    let report = ledger.report("net-worth", &["--from", "2024-01-01", "--to", "2024-03-10"])?;
    assert_eq!(
        series(&report, "Total Assets")["amounts"],
        json!([140_000, 140_000, 120_000])
    );
    assert_eq!(
        series(&report, "Total Liabilities")["amounts"],
        json!([50000, 50000, 30000])
    );
    assert_eq!(
        series(&report, "Net Worth")["amounts"],
        json!([90000, 90000, 90000])
    );
    assert!(report["data"].get("total").is_none());
    Ok(())
}

#[test]
fn net_worth_can_be_negative_and_spending_can_be_zero() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-01", "3000:50", "2000:50", "USD")?;
    let report = ledger.report("net-worth", &["--to", "2024-02-29"])?;
    assert_eq!(
        series(&report, "Net Worth")["amounts"],
        json!([-5000, -5000])
    );
    let report = ledger.report("spending", &["--to", "2024-02-29"])?;
    assert_eq!(series(&report, "Total Spending")["amounts"], json!([0, 0]));
    Ok(())
}

#[test]
fn currency_is_required_when_ambiguous_and_controls_minor_unit_display() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-02", "5000:100", "1000:100", "JPY")?;
    ledger.post("2024-01-02", "5000:12", "1000:12", "USD")?;
    let output = ledger.run(&["--json", "report", "trend", "--metric", "spending"])?;
    assert_eq!(output.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["meta"]["command"], "report.trend");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("--currency"))
    );
    for metric in ["spending", "net-worth"] {
        let report = ledger.report(metric, &["--to", "2024-01-31", "--currency", "jpy"])?;
        assert_eq!(report["data"]["currency"], "JPY");
        let (name, raw, display) = if metric == "spending" {
            ("Total Spending", 100, "100")
        } else {
            ("Net Worth", -100, "-100")
        };
        assert_eq!(series(&report, name)["amounts"], json!([raw]));
        assert_eq!(series(&report, name)["amounts_display"], json!([display]));
    }
    Ok(())
}

#[test]
fn quarters_years_defaults_and_company_isolation() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-01", "5000:20", "1000:20", "USD")?;
    ledger.post("2025-01-01", "5100:30", "1000:30", "USD")?;
    let report = ledger.report(
        "spending",
        &[
            "--from",
            "2024-01-01",
            "--to",
            "2025-12-31",
            "--period",
            "year",
        ],
    )?;
    assert_eq!(
        series(&report, "Total Spending")["amounts"],
        json!([2000, 3000])
    );
    let report = ledger.report("spending", &["--to", "2025-06-30", "--period", "quarter"])?;
    assert_eq!(report["data"]["from"], "2025-01-01");
    assert_eq!(
        series(&report, "Total Spending")["amounts"],
        json!([3000, 0])
    );
    ledger.ok(&["company", "create", "empty", "Empty"])?;
    let raw = ledger.ok(&[
        "--company",
        "empty",
        "--json",
        "report",
        "trend",
        "--metric",
        "spending",
        "--to",
        "2025-02-28",
    ])?;
    let report: Value = serde_json::from_str(&raw)?;
    assert_eq!(series(&report, "Total Spending")["amounts"], json!([0, 0]));
    assert_eq!(report["data"]["series"].as_array().map(Vec::len), Some(1));
    let raw = ledger.ok(&[
        "--company",
        "empty",
        "--json",
        "report",
        "trend",
        "--metric",
        "net-worth",
        "--to",
        "2025-02-28",
    ])?;
    assert_eq!(
        series(&serde_json::from_str(&raw)?, "Net Worth")["amounts"],
        json!([0, 0])
    );
    Ok(())
}

#[test]
fn table_and_csv_preserve_labels_and_values() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-01", "5000:12.34", "1000:12.34", "USD")?;
    for metric in ["spending", "net-worth"] {
        let args = ["report", "trend", "--metric", metric, "--to", "2024-02-29"];
        let table = ledger.ok(&args)?;
        for label in ["2024-01", "2024-02", "USD", "12.34"] {
            assert!(table.contains(label), "missing {label}: {table}");
        }
        let mut csv_args = vec!["--format", "csv"];
        csv_args.extend(args);
        let csv = ledger.ok(&csv_args)?;
        let records = csv::Reader::from_reader(csv.as_bytes())
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        assert!(csv.starts_with("code,name,2024-01,2024-02\n"));
        let name = if metric == "spending" {
            "Rent, office"
        } else {
            "Net Worth"
        };
        let row = records
            .iter()
            .find(|row| row.get(1) == Some(name))
            .ok_or("missing CSV series")?;
        assert_eq!(
            row.get(2),
            Some(if metric == "spending" {
                "1234"
            } else {
                "-1234"
            })
        );
    }
    Ok(())
}

#[test]
fn invalid_dates_currency_metric_and_excessive_periods_fail() -> TestResult {
    let ledger = Ledger::new()?;
    for options in [
        vec!["--from", "2024-03-01", "--to", "2024-02-01"],
        vec!["--to", "2024-02-30"],
        vec!["--from", "bad"],
        vec!["--currency", "BAD"],
        vec!["--from", "2000-01-01", "--to", "2025-12-31"],
    ] {
        let mut args = vec!["--json", "report", "trend", "--metric", "spending"];
        args.extend(options);
        let output = ledger.run(&args)?;
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        let value: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(value["error"]["code"], "VALIDATION");
    }
    for args in [
        vec!["report", "trend"],
        vec!["report", "trend", "--metric", "unknown"],
    ] {
        assert_eq!(ledger.run(&args)?.status.code(), Some(2));
    }
    Ok(())
}
