//! Agent-facing audit pagination and currency companions.

use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt as _;
use beankeeper_cli::db::{self, Actor, Db, PostEntryParams, PostTransactionParams};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
    db: Db,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("audit.db");
        let db = Db::open(&path, None)?;
        for company in ["acme", "other"] {
            db::create_company(db.conn(), company, company, None)?;
            db::create_account(db.conn(), company, "1000", "Cash", "asset", None)?;
            db::create_account(db.conn(), company, "4000", "Revenue", "revenue", None)?;
        }
        let ledger = Self {
            _dir: dir,
            path,
            db,
        };
        for (company, currency) in [
            ("acme", "USD"),
            ("acme", "JPY"),
            ("acme", "BHD"),
            ("other", "USD"),
        ] {
            ledger.post(company, currency)?;
        }
        Ok(ledger)
    }

    fn post(&self, company: &str, currency: &str) -> TestResult {
        let entries = [
            PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount: 1234,
                memo: None,
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount: 1234,
                memo: None,
                tax_category: None,
            },
        ];
        db::post_transaction(
            self.db.conn(),
            &PostTransactionParams {
                company_slug: company,
                description: "Sale",
                metadata: None,
                currency,
                date: "2026-01-01",
                entries: &entries,
                correlate: None,
                reference: None,
                on_conflict: db::ConflictStrategy::Error,
                actor: &Actor::new("test"),
            },
        )?;
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let output = Command::cargo_bin("bk")?
            .arg("--db")
            .arg(&self.path)
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_COMPANY")
            .env_remove("BEANKEEPER_DB")
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    }
}

#[test]
fn audit_pagination_counts_filtered_rows_at_below_and_above_limit() -> TestResult {
    let ledger = Ledger::new()?;
    for (limit, returned, has_more) in [(2, 2, true), (3, 3, false), (4, 3, false)] {
        let limit = limit.to_string();
        let args = [
            "audit",
            "log",
            "--company",
            "acme",
            "--entity",
            "transaction",
            "--action",
            "create",
            "--limit",
            &limit,
        ];
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(&args);
        let output = ledger.run(&json_args)?;
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["data"].as_array().map(Vec::len), Some(returned));
        assert_eq!(
            value["meta"]["pagination"],
            json!({"limit":limit.parse::<i64>()?,"offset":0,"returned":returned,"total":3,"has_more":has_more})
        );
        let table = ledger.run(&args)?;
        let stderr = String::from_utf8_lossy(&table.stderr);
        assert_eq!(
            stderr.contains("use --limit/--offset"),
            has_more,
            "{stderr}"
        );
        if has_more {
            assert!(stderr.contains("showing 2 of 3 audit entries"), "{stderr}");
        }
        let mut quiet = vec!["--quiet"];
        quiet.extend_from_slice(&args);
        assert_eq!(ledger.run(&quiet)?.stderr, Vec::<u8>::new());
    }
    Ok(())
}

#[test]
fn audit_offsets_and_empty_pages_preserve_array_shape() -> TestResult {
    let ledger = Ledger::new()?;
    for (limit, offset, returned, has_more) in [
        (1, 1, 1, true),
        (1, 2, 1, false),
        (2, 3, 0, false),
        (0, 0, 0, true),
        (2, 99, 0, false),
    ] {
        let output = ledger.run(&[
            "--json",
            "audit",
            "log",
            "--company",
            "acme",
            "--limit",
            &limit.to_string(),
            "--offset",
            &offset.to_string(),
        ])?;
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["data"].as_array().map(Vec::len), Some(returned));
        if returned > 0 {
            assert_eq!(value["data"][0]["id"], 3 - offset);
        }
        assert_eq!(
            value["meta"]["pagination"],
            json!({"limit":limit,"offset":offset,"returned":returned,"total":3,"has_more":has_more})
        );
    }
    let output = ledger.run(&["--json", "audit", "log", "--action", "status-change"])?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["data"], json!([]));
    assert_eq!(value["meta"]["pagination"]["total"], 0);
    Ok(())
}

#[test]
fn audit_snapshot_companions_preserve_original_strings_and_hashes() -> TestResult {
    let ledger = Ledger::new()?;
    let output = ledger.run(&["--json", "audit", "log", "--company", "acme"])?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let rows = value["data"].as_array().ok_or("audit array")?;
    for (row, expected) in rows.iter().zip(["1.234", "1234", "12.34"]) {
        let raw: String = ledger.db.conn().query_row(
            "SELECT after FROM audit_log WHERE id = ?1",
            [row["id"].as_i64().ok_or("audit id")?],
            |row| row.get(0),
        )?;
        assert_eq!(row["after"], raw);
        assert_eq!(row["after_display"]["entries"][0]["amount"], 1234);
        assert_eq!(
            row["after_display"]["entries"][0]["amount_display"],
            expected
        );
        assert_eq!(row["before_display"], Value::Null);
    }
    ledger
        .db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE length(row_hash) = 64",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| assert_eq!(count, 4))?;
    Ok(())
}

#[test]
fn clearing_audit_resolves_immutable_entry_currency() -> TestResult {
    let ledger = Ledger::new()?;
    for (transaction_id, entry_id) in [(1, 1), (2, 3), (3, 5)] {
        db::clear_entry(
            ledger.db.conn(),
            &Actor::new("test"),
            &db::ClearRequest {
                company_slug: "acme",
                transaction_id,
                entry_id,
                amount: Some(100),
                date: "2026-01-31",
                statement: Some("S1"),
                status: db::ClearingStatus::Cleared,
            },
        )?;
    }
    let output = ledger.run(&[
        "--json",
        "audit",
        "log",
        "--company",
        "acme",
        "--action",
        "status-change",
    ])?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let rows = value["data"].as_array().ok_or("audit array")?;
    for (row, expected) in rows.iter().zip(["0.100", "100", "1.00"]) {
        assert_eq!(row["after_display"]["cleared_amount"], 100);
        assert_eq!(row["after_display"]["cleared_amount_display"], expected);
        assert_eq!(
            row["after_display"]["clearings"][0]["amount_display"],
            expected
        );
    }
    ledger.run(&["verify"])?;
    Ok(())
}

#[test]
fn new_budget_audit_snapshots_keep_currency_after_deletion() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.run(&[
        "--company",
        "acme",
        "budget",
        "set",
        "4000",
        "--year",
        "2026",
        "--month",
        "1",
        "--amount",
        "1.234",
        "--currency",
        "BHD",
    ])?;
    ledger.run(&[
        "--company",
        "acme",
        "budget",
        "delete",
        "4000",
        "--year",
        "2026",
        "--month",
        "1",
        "--currency",
        "BHD",
        "--force",
    ])?;
    let output = ledger.run(&[
        "--json",
        "audit",
        "log",
        "--company",
        "acme",
        "--entity",
        "budget",
    ])?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        value["data"][0]["before_display"]["amount_display"],
        "1.234"
    );
    assert_eq!(value["data"][1]["after_display"]["amount_display"], "1.234");
    ledger.run(&["verify"])?;
    Ok(())
}
