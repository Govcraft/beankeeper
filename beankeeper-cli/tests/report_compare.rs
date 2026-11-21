//! Period comparisons, ranked movers and reconciled explanations.

use std::error::Error;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
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

    fn compare(&self, command: &[&str], options: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut args = vec!["--json", "report"];
        args.extend(command);
        args.extend([
            "--from",
            "2024-02-01",
            "--to",
            "2024-02-29",
            "--compare",
            "previous",
        ]);
        args.extend(options);
        Ok(serde_json::from_str(&self.ok(&args)?)?)
    }
}

fn account<'a>(report: &'a Value, code: &str) -> &'a Value {
    report["data"]["accounts"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["code"] == code))
        .unwrap_or_else(|| panic!("missing account {code}: {report}"))
}

#[test]
fn spending_movers_zero_bases_refunds_and_explanations() -> TestResult {
    let ledger = Ledger::new()?;
    for (date, debit, credit) in [
        ("2024-01-01", "5000:100", "1000:100"),
        ("2024-02-01", "5000:150", "1000:150"),
        ("2024-02-29", "5100:70", "1000:70"),
        ("2024-02-20", "1000:20", "5000:20"),
        ("2024-03-01", "5000:999", "1000:999"),
    ] {
        ledger.post(date, debit, credit, "USD")?;
    }
    ledger.ok(&["txn", "meta", "set", "1", "payee=Landlord"])?;
    ledger.ok(&["txn", "meta", "set", "2", "payee=Landlord"])?;
    let report = ledger.compare(
        &["trend", "--metric", "spending"],
        &["--explain-change", "5000"],
    )?;
    assert_eq!(report["data"]["prior"]["from"], "2024-01-01");
    let rent = account(&report, "5000");
    assert_eq!(rent["current"], 13000);
    assert_eq!(rent["prior"], 10000);
    assert_eq!(rent["change"], 3000);
    assert_eq!(rent["change_percent"], 30.0);
    assert_eq!(account(&report, "5100")["change"], 7000);
    assert!(account(&report, "5100")["change_percent"].is_null());
    assert_eq!(account(&report, "5200")["change"], 0);
    assert_eq!(report["data"]["movers"][0]["code"], "5100");
    assert_eq!(report["data"]["movers"][0]["rank"], 1);
    let explanation = &report["data"]["explanation"];
    assert_eq!(explanation["change"], 3000);
    assert_eq!(explanation["transactions"][0]["change"], 15000);
    assert_eq!(explanation["transactions"][1]["change"], -10000);
    assert_eq!(explanation["transactions"][2]["change"], -2000);
    assert_eq!(explanation["payees"][0]["payee"], "Landlord");
    assert_eq!(explanation["payees"][0]["change"], 5000);
    assert!(
        report["data"]["summary"]
            .as_str()
            .is_some_and(|s| s.contains("Supplies increased"))
    );
    ledger.ok(&["verify"])?;
    Ok(())
}

#[test]
fn cash_flow_preserves_attribution_and_excludes_non_cash_transactions() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-31", "5000:20", "1000:20", "USD")?;
    ledger.post("2024-02-01", "5000:60", "1000:60", "USD")?;
    ledger.post("2024-02-10", "5000:500", "2000:500", "USD")?;
    let report = ledger.compare(&["cash-flow"], &["--explain-change", "5000"])?;
    assert_eq!(account(&report, "5000")["current"], -6000);
    assert_eq!(account(&report, "5000")["change"], -4000);
    assert_eq!(report["data"]["totals"]["net_change"]["change"], -4000);
    assert_eq!(report["data"]["explanation"]["change"], -4000);
    assert_eq!(
        report["data"]["explanation"]["transactions"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    Ok(())
}

#[test]
fn balance_snapshots_cancel_opening_history_and_activity_can_be_requested() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2023-12-31", "1000:1000", "3000:1000", "USD")?;
    ledger.post("2024-01-10", "5000:100", "1000:100", "USD")?;
    ledger.post("2024-02-20", "5000:40", "1000:40", "USD")?;
    let report: Value = serde_json::from_str(&ledger.ok(&[
        "--json",
        "report",
        "balance",
        "--account",
        "1000",
        "--to",
        "2024-02-29",
        "--compare",
        "previous",
        "--explain-change",
        "1000",
    ])?)?;
    assert_eq!(report["data"]["basis"], "cumulative");
    assert_eq!(account(&report, "1000")["current"], 86000);
    assert_eq!(account(&report, "1000")["prior"], 90000);
    assert_eq!(report["data"]["explanation"]["change"], -4000);
    assert_eq!(
        report["data"]["explanation"]["transactions"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let activity = ledger.compare(&["balance", "--account", "1000"], &[])?;
    assert_eq!(account(&activity, "1000")["current"], -4000);
    assert_eq!(account(&activity, "1000")["prior"], -10000);
    Ok(())
}

#[test]
fn currencies_are_never_combined_and_explicit_windows_and_errors_work() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-01", "5000:20", "1000:20", "USD")?;
    ledger.post("2024-02-01", "5000:60", "1000:60", "MXN")?;
    let failure = ledger.run(&[
        "--json",
        "report",
        "income-statement",
        "--compare",
        "previous",
    ])?;
    assert!(!failure.status.success());
    let error: Value = serde_json::from_slice(&failure.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    let report = ledger.compare(
        &["income-statement"],
        &["--currency", "MXN", "--explain-change", "5000"],
    )?;
    assert_eq!(account(&report, "5000")["current"], 6000);
    assert_eq!(account(&report, "5000")["prior"], 0);
    assert!(account(&report, "5000")["change_percent"].is_null());
    let report: Value = serde_json::from_str(&ledger.ok(&[
        "--json",
        "report",
        "income-statement",
        "--from",
        "2024-02-01",
        "--to",
        "2024-02-29",
        "--compare",
        "2024-01-01..2024-01-31",
        "--currency",
        "USD",
    ])?)?;
    assert_eq!(account(&report, "5000")["change"], -2000);
    assert_eq!(report["data"]["movers"][0]["direction"], "decrease");
    for args in [
        vec![
            "report",
            "income-statement",
            "--compare",
            "2024-02-30..2024-03-01",
        ],
        vec![
            "report",
            "income-statement",
            "--compare",
            "2024-02-01..2024-01-01",
        ],
        vec!["report", "income-statement", "--explain-change", "5000"],
        vec![
            "report",
            "income-statement",
            "--compare",
            "previous",
            "--currency",
            "USD",
            "--explain-change",
            "no-such-account",
        ],
    ] {
        assert!(!ledger.run(&args)?.status.success());
    }
    Ok(())
}

#[test]
fn balance_sheet_earnings_and_all_output_formats_preserve_comparison_data() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-01-01", "1000:1000", "3000:1000", "USD")?;
    ledger.post("2024-01-15", "1000:500", "4000:500", "USD")?;
    ledger.post("2024-02-15", "5000:100", "1000:100", "USD")?;
    let report: Value = serde_json::from_str(&ledger.ok(&[
        "--json",
        "report",
        "balance-sheet",
        "--to",
        "2024-02-29",
        "--compare",
        "previous",
    ])?)?;
    assert_eq!(report["data"]["totals"]["total_assets"]["current"], 140_000);
    assert_eq!(
        report["data"]["totals"]["computed_earnings"]["current"],
        40000
    );
    assert_eq!(
        report["data"]["totals"]["total_liabilities_and_equity"]["current"],
        140_000
    );
    assert!(
        !report["data"]["accounts"]
            .as_array()
            .ok_or("missing accounts")?
            .iter()
            .any(|row| row["type"] == "revenue" || row["type"] == "expense")
    );
    let args = [
        "report",
        "trend",
        "--metric",
        "spending",
        "--from",
        "2024-02-01",
        "--to",
        "2024-02-29",
        "--compare",
        "previous",
        "--explain-change",
        "5000",
    ];
    let table = ledger.ok(&args)?;
    assert!(table.contains("Rent, office increased by 100.00 USD"));
    assert!(table.contains("Contribution to change"));
    assert!(table.contains("/transactions?"));
    let mut csv_args = vec!["--format", "csv"];
    csv_args.extend(args);
    let csv = ledger.ok(&csv_args)?;
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let rows = reader.records().collect::<Result<Vec<_>, _>>()?;
    assert!(rows.iter().any(|row| row.get(0) == Some("account")
        && row.get(1) == Some("5000")
        && row.get(5) == Some("10000")));
    assert!(rows.iter().any(|row| row.get(0) == Some("desktop_current")));
    assert!(rows.iter().any(|row| row.get(0) == Some("transaction")));
    Ok(())
}

#[test]
fn comparison_preserves_existing_alphanumeric_account_codes_and_missing_account_errors()
-> TestResult {
    let ledger = Ledger::new()?;
    ledger.ok(&[
        "account",
        "create",
        "rent-office",
        "Office rent",
        "--type",
        "expense",
    ])?;
    for command in [
        &["income-statement"][..],
        &["trial-balance"][..],
        &["trend", "--metric", "spending"][..],
        &["balance", "--account", "rent-office"][..],
    ] {
        let report = ledger.compare(command, &["--explain-change", "rent-office"])?;
        let line = account(&report, "rent-office");
        assert_eq!(line["current"], 0);
        assert_eq!(line["prior"], 0);
        assert_eq!(line["change"], 0);
        assert_eq!(report["data"]["explanation"]["change"], 0);
    }
    let output = ledger.run(&[
        "--json",
        "report",
        "balance",
        "--account",
        "unknown",
        "--compare",
        "previous",
    ])?;
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "NOT_FOUND");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("account") && !message.contains("minor-unit"))
    );
    Ok(())
}
