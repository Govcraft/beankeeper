//! Execute the README's demo commands so dates and examples cannot drift.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

fn run(path: &Path, args: &[String]) -> Result<Value, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command
        .arg("--db")
        .arg(path)
        .args(["--json", "--actor", "demo-test"])
        .args(args);
    for key in [
        "BEANKEEPER_PASSPHRASE_CMD",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BK_AGENT_TOKEN",
        "BK_ACTOR",
    ] {
        command.env_remove(key);
    }
    let output = command.output()?;
    assert!(
        output.status.success(),
        "bk {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["ok"], true, "{args:?}: {response}");
    Ok(response)
}

fn walkthrough_commands() -> Result<Vec<Vec<String>>, Box<dyn Error>> {
    let readme = include_str!("../../README.md");
    let section = readme
        .split_once("## Demo Mode\n")
        .ok_or("missing demo section")?
        .1
        .split("\n### Intercompany Transactions")
        .next()
        .ok_or("missing demo walkthrough")?
        .replace("\\\n", " ");
    let mut in_shell = false;
    let mut commands = Vec::new();
    for line in section.lines().map(str::trim) {
        if line == "```sh" {
            in_shell = true;
        } else if line == "```" {
            in_shell = false;
        } else if in_shell && let Some(args) = line.strip_prefix("bk ") {
            // Walkthrough commands use whitespace-separated arguments, with
            // optional quotes around single words (e.g. "payroll").
            commands.push(
                args.split_whitespace()
                    .map(|arg| arg.trim_matches('"').to_owned())
                    .collect(),
            );
        }
    }
    assert!(!commands.is_empty(), "README must contain demo commands");
    Ok(commands)
}

#[test]
fn readme_demo_reports_show_seeded_features() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("demo.db");
    let mut results = BTreeMap::new();
    for args in walkthrough_commands()? {
        let response = run(&path, &args)?;
        let name = response["meta"]["command"]
            .as_str()
            .ok_or("response command name")?;
        results.insert(name.to_owned(), response["data"].clone());
    }

    let balance = results.get("report.balance-sheet").ok_or("balance sheet")?;
    assert!(balance["total_assets"].as_i64().is_some_and(|n| n > 0));
    assert_eq!(balance["balanced"], true);
    let tax = results.get("report.schedule-c").ok_or("Schedule C")?;
    let lines = tax["lines"].as_array().ok_or("mapped Schedule C lines")?;
    for (line, expected) in [("11", 150_000), ("16c", 6_000), ("22", 12_000)] {
        let row = lines
            .iter()
            .find(|row| row["line"] == line)
            .ok_or("tax line")?;
        assert_eq!(row["net"], expected, "Schedule C line {line}");
    }
    let vendors = results.get("report.1099").ok_or("1099 report")?;
    assert_eq!(vendors["vendors"][0]["total"], 150_000);
    assert_eq!(vendors["vendors"][0]["payment_count"], 2);

    let lots = results.get("investment.lots").ok_or("lots report")?;
    assert_eq!(lots.as_array().map(Vec::len), Some(2));
    assert_eq!(lots[0]["remaining_quantity"], "6");
    assert_eq!(lots[0]["remaining_cost_basis"], 60_000);
    assert_eq!(lots[1]["remaining_quantity"], "5");
    let holdings = results
        .get("investment.holdings")
        .ok_or("holdings report")?;
    assert_eq!(holdings[0]["quantity"], "11");
    assert_eq!(holdings[0]["cost_basis"], 120_000);
    assert_eq!(holdings[0]["market_value"], 143_000);

    let rules = results.get("rule.list").ok_or("rules")?;
    assert_eq!(rules[0]["contains"], "office supplies");
    assert_eq!(rules[0]["account_code"], "5200");
    let schedules = results.get("schedule.list").ok_or("schedules")?;
    assert_eq!(schedules[0]["status"], "pending");
    let preview = results.get("schedule.run").ok_or("schedule preview")?;
    assert_eq!(preview.as_array().map(Vec::len), Some(1));
    assert_eq!(preview[0]["date"], "2025-03-05");
    assert!(preview[0]["transaction_id"].is_null());

    let loan = results.get("loan.schedule").ok_or("loan schedule")?;
    assert_eq!(loan["payments"].as_array().map(Vec::len), Some(12));
    assert_eq!(loan["payments"][0]["interest"], 6_000);
    let ledger = results.get("report.ledger").ok_or("loan ledger")?;
    let loan_account = &ledger["accounts"][0];
    assert_eq!(loan_account["entries"].as_array().map(Vec::len), Some(2));
    assert_eq!(loan_account["total_credits"], 1_200_000);
    assert_eq!(
        loan_account["total_debits"],
        loan["payments"][0]["principal"]
    );
    let settlement = results.get("report.reconciliation").ok_or("settlement")?;
    assert_eq!(settlement["reconciled"], true);
    assert_eq!(settlement["difference"], 0);
    assert_eq!(settlement["uncleared_total"], 18_000);
    assert_eq!(settlement["charges"][0]["age_days"], 30);
    assert_eq!(settlement["charges"][0]["short_pay"], 18_000);

    // The README preview must not post or advance the pending rent schedule.
    let after = run(
        &path,
        &["--company", "acme-consulting", "schedule", "list"].map(str::to_owned),
    )?;
    assert_eq!(after["data"], *schedules);
    let budget = results.get("report.budget-variance").ok_or("rent budget")?;
    assert_eq!(budget["totals"]["budget"], 750_000);
    assert_eq!(budget["totals"]["actual"], 500_000);
    let verification = results.get("verify").ok_or("ledger verification")?;
    assert_eq!(
        verification["schema_version"],
        i64::from(beankeeper_cli::db::current_schema_version())
    );
    assert_eq!(verification["status"], "healthy");
    Ok(())
}

#[test]
fn demo_help_describes_multiple_companies() -> TestResult {
    let output = Command::cargo_bin("bk")?
        .args(["init", "--demo", "--help"])
        .output()?;
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("multi-company sample data"));
    assert!(!help.contains("a sample company"));
    Ok(())
}
