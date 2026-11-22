//! Pagination remains additive to transaction arrays and counts matching rows.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{Actor, accounts, companies, connection::Db};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("books.db");
        let db = Db::open(&path, None)?;
        companies::create_company(
            db.conn(),
            &Actor::new("fixture"),
            &"acme".parse()?,
            "Acme",
            None,
        )?;
        for (code, kind) in [
            ("1000", beankeeper::types::AccountType::Asset),
            ("4000", beankeeper::types::AccountType::Revenue),
        ] {
            accounts::create_account(
                db.conn(),
                &Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code,
                    name: code,
                    account_type: kind,
                    default_tax_category: None,
                },
            )?;
        }
        let journals: Vec<_> = (0..51)
            .map(|index| {
                json!({
                    "date": "2026-01-01",
                    "description": if index < 3 { "Selected" } else { "Other" },
                    "metadata": {"vendor": if index < 3 { "Acme" } else { "Other" }},
                    "entries": [
                        {"account_code": "1000", "direction": "debit", "amount": "1.00"},
                        {"account_code": "4000", "direction": "credit", "amount": "1.00"}
                    ]
                })
            })
            .collect();
        let file = dir.path().join("journals.json");
        std::fs::write(&file, serde_json::to_vec(&journals)?)?;
        let ledger = Self { _dir: dir, path };
        let output = ledger
            .command()?
            .args(["txn", "import", "--input-format", "json", "--file"])
            .arg(&file)
            .output()?;
        assert_success(&output);
        Ok(ledger)
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .args(["--company", "acme"])
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_CURRENCY")
            .env_remove("BEANKEEPER_COMPANY");
        Ok(command)
    }

    fn list(&self, flags: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(self.command()?.args(["txn", "list"]).args(flags).output()?)
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn default_page_and_offset_keep_the_array_and_report_total() -> TestResult {
    let ledger = Ledger::new()?;
    for (flags, returned, offset, has_more) in [
        (vec!["--json"], 50, 0, true),
        (vec!["--json", "--offset", "50"], 1, 50, false),
        (vec!["--json", "--offset", "100"], 0, 100, false),
    ] {
        let output = ledger.list(&flags)?;
        assert_success(&output);
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["data"].as_array().map(Vec::len), Some(returned));
        assert_eq!(
            value["meta"]["pagination"],
            json!({"limit": 50, "offset": offset, "returned": returned, "total": 51, "has_more": has_more})
        );
    }
    let output = ledger.list(&[])?;
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("showing 50 of 51 transactions (use --limit/--offset)")
    );
    let quiet = ledger.list(&["--quiet"])?;
    assert_success(&quiet);
    assert_eq!(quiet.stderr, Vec::<u8>::new());
    assert_eq!(quiet.stdout, output.stdout);
    Ok(())
}

#[test]
fn filtered_pages_below_at_and_above_the_limit_signal_only_real_truncation() -> TestResult {
    let ledger = Ledger::new()?;
    for (limit, returned, has_more) in [("2", 2, true), ("3", 3, false), ("4", 3, false)] {
        let filters = [
            "--limit",
            limit,
            "--account",
            "1000",
            "--description",
            "Selected",
            "--filter-metadata",
            "vendor=Acme",
            "--amount-eq",
            "1",
        ];
        let output = ledger.list(&filters)?;
        assert_success(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.contains("showing"), has_more, "{stderr}");
        let mut json_flags = vec!["--json"];
        json_flags.extend(filters);
        let value: Value = serde_json::from_slice(&ledger.list(&json_flags)?.stdout)?;
        assert_eq!(value["data"].as_array().map(Vec::len), Some(returned));
        assert_eq!(value["meta"]["pagination"]["total"], 3);
        assert_eq!(value["meta"]["pagination"]["returned"], returned);
        assert_eq!(value["meta"]["pagination"]["has_more"], has_more);
    }
    let output = ledger.list(&["--json", "--count", "--limit", "1"])?;
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["data"]["count"], 51);
    assert!(value["meta"].get("pagination").is_none());
    Ok(())
}

#[test]
fn zero_and_unlimited_pages_follow_sqlite_limit_semantics() -> TestResult {
    let ledger = Ledger::new()?;
    for (limit, returned, has_more) in [("0", 0, true), ("-1", 51, false)] {
        let output = ledger.list(&["--json", &format!("--limit={limit}")])?;
        assert_success(&output);
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["data"].as_array().map(Vec::len), Some(returned));
        assert_eq!(value["meta"]["pagination"]["has_more"], has_more);
    }
    Ok(())
}

#[test]
fn date_filters_match_the_same_calendar_day_for_list_and_count() -> TestResult {
    let ledger = Ledger::new()?;
    let bounds = ["--json", "--from", "2026-1-1", "--to", "2026-1-1"];
    let rows = ledger.list(&bounds)?;
    assert_success(&rows);
    let result: Value = serde_json::from_slice(&rows.stdout)?;
    assert_eq!(result["meta"]["pagination"]["total"], 51);

    let mut count_flags = bounds.to_vec();
    count_flags.push("--count");
    let count = ledger.list(&count_flags)?;
    assert_success(&count);
    let result: Value = serde_json::from_slice(&count.stdout)?;
    assert_eq!(result["data"]["count"], 51);

    let invalid = ledger.list(&["--from", "2026-02-30"])?;
    assert_eq!(invalid.status.code(), Some(2));
    Ok(())
}
