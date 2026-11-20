//! End-to-end CSV and JSON import guarantees (issue #38).

use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::prelude::*;
use beankeeper_cli::db::{accounts, companies, connection::Db};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
    path: PathBuf,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let db = Db::open(&path, None)?;
        for company in ["acme", "other"] {
            companies::create_company(db.conn(), company, company, None)?;
            accounts::create_account(db.conn(), company, "1000", "Bank", "asset", None)?;
            accounts::create_account(
                db.conn(),
                company,
                "9000",
                "Suspense",
                "expense",
                Some("default-tax"),
            )?;
            accounts::create_account(db.conn(), company, "4000", "Revenue", "revenue", None)?;
        }
        Ok(Self { dir, path })
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .args(["--json", "--company", "acme"])
            .env_remove("BEANKEEPER_CURRENCY")
            .env_remove("BEANKEEPER_PASSPHRASE_CMD");
        Ok(command)
    }

    fn import(&self, name: &str, content: &str, extra: &[&str]) -> Result<Output, Box<dyn Error>> {
        let file = self.dir.path().join(name);
        std::fs::write(&file, content)?;
        Ok(self
            .command()?
            .args(["txn", "import", "--file"])
            .arg(file)
            .args(extra)
            .output()?)
    }

    fn count(&self, table: &str) -> Result<i64, Box<dyn Error>> {
        let db = Db::open_existing(&self.path, None)?;
        Ok(db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }
}

const BANK_FLAGS: &[&str] = &["--account", "1000", "--suspense", "9000"];

fn success(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["ok"], true);
    assert_eq!(value["meta"]["command"], "txn.import");
    Ok(value["data"].clone())
}

fn validation_error(output: &Output, expected: &str) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "VALIDATION");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(expected)),
        "{value}"
    );
    assert_eq!(output.stdout, Vec::<u8>::new());
    Ok(())
}

fn journal(reference: Option<&str>) -> Value {
    json!({
        "date": "2026-03-15", "description": "Split receipt", "reference": reference,
        "metadata": {"vendor": "Coffee", "nested": {"paid": true}},
        "entries": [
            {"account_code": "1000", "direction": "debit", "amount": "12.34", "memo": "bank memo"},
            {"account_code": "4000", "direction": "credit", "amount": "10.00"},
            {"account_code": "9000", "direction": "credit", "amount": "2.34", "tax_category": "explicit-tax"}
        ]
    })
}

#[test]
fn csv_posts_inflows_outflows_quoted_descriptions_and_default_tax() -> TestResult {
    let ledger = Ledger::new()?;
    let csv = "date,description,amount,reference\n2026-03-15,\"Coffee, shop\",-12.34, bank-1 \n2026-03-16,Deposit,20,bank-2\n";
    assert_eq!(
        success(&ledger.import("bank.CSV", csv, BANK_FLAGS)?)?["imported"],
        2
    );
    assert_eq!(ledger.count("entries")?, 4);
    let db = Db::open_existing(&ledger.path, None)?;
    let description: String = db.conn().query_row(
        "SELECT description FROM transactions WHERE reference = 'bank-1'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(description, "Coffee, shop");
    let mut stmt = db
        .conn()
        .prepare("SELECT account_code, direction, amount, tax_category FROM entries ORDER BY id")?;
    let rows: Vec<(String, String, i64, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        rows,
        [
            (
                "9000".into(),
                "debit".into(),
                1234,
                Some("default-tax".into())
            ),
            ("1000".into(), "credit".into(), 1234, None),
            ("1000".into(), "debit".into(), 2000, None),
            (
                "9000".into(),
                "credit".into(),
                2000,
                Some("default-tax".into())
            )
        ]
    );
    let repeat = success(&ledger.import("renamed.csv", csv, BANK_FLAGS)?)?;
    assert_eq!(repeat["imported"], 0);
    assert_eq!(repeat["skipped"], 2);
    Ok(())
}

#[test]
fn csv_maps_columns_dates_currency_and_signs() -> TestResult {
    let ledger = Ledger::new()?;
    let csv = "Posted,Payee,Value,ID\n03/15/2026,Purchase,5.125,custom-1\n03/16/2026,Refund,-2.000,custom-2\n";
    let flags = [
        "--account",
        "1000",
        "--suspense",
        "9000",
        "--date-col",
        "Posted",
        "--description-col",
        "Payee",
        "--amount-col",
        "Value",
        "--reference-col",
        "ID",
        "--date-format",
        "%m/%d/%Y",
        "--positive-amount",
        "outflow",
        "--currency",
        "BHD",
    ];
    assert_eq!(
        success(&ledger.import("mapped.csv", csv, &flags)?)?["imported"],
        2
    );
    let db = Db::open_existing(&ledger.path, None)?;
    let mut stmt = db.conn().prepare("SELECT t.date, t.currency, e.amount, e.account_code FROM entries e JOIN transactions t ON t.id = e.transaction_id WHERE e.direction = 'debit' ORDER BY e.id")?;
    let rows: Vec<(String, String, i64, String)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        rows,
        [
            ("2026-03-15".into(), "BHD".into(), 5125, "9000".into()),
            ("2026-03-16".into(), "BHD".into(), 2000, "1000".into())
        ]
    );
    Ok(())
}

#[test]
fn json_posts_split_entries_metadata_memos_and_tax_categories() -> TestResult {
    let ledger = Ledger::new()?;
    let result = success(&ledger.import(
        "batch.json",
        &json!([journal(Some(" json-1 "))]).to_string(),
        &[],
    )?)?;
    assert_eq!(result["imported"], 1);
    assert_eq!(result["transactions"][0]["reference"], "json-1");
    let db = Db::open_existing(&ledger.path, None)?;
    let metadata: String = db
        .conn()
        .query_row("SELECT metadata FROM transactions", [], |row| row.get(0))?;
    assert_eq!(
        serde_json::from_str::<Value>(&metadata)?,
        journal(None)["metadata"]
    );
    let memo: String = db.conn().query_row(
        "SELECT memo FROM entries WHERE account_code = '1000'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(memo, "bank memo");
    let tax: String = db.conn().query_row(
        "SELECT tax_category FROM entries WHERE account_code = '9000'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(tax, "explicit-tax");
    assert_eq!(ledger.count("entries")?, 3);
    Ok(())
}

#[test]
fn identical_unreferenced_rows_are_all_imported_for_both_formats() -> TestResult {
    for (name, input, flags) in [
        (
            "rows.csv",
            "date,description,amount\n2026-01-01,Receipt,10\n2026-01-01,Receipt,10\n".into(),
            BANK_FLAGS,
        ),
        (
            "rows.json",
            json!([journal(None), journal(None)]).to_string(),
            &[][..],
        ),
    ] {
        let ledger = Ledger::new()?;
        let mut dry_run_flags = flags.to_vec();
        dry_run_flags.push("--dry-run");
        let preview = success(&ledger.import(name, &input, &dry_run_flags)?)?;
        assert_eq!(preview["imported"], 2);
        assert_eq!(preview["skipped"], 0);
        assert_eq!(ledger.count("transactions")?, 0);
        let first = success(&ledger.import(name, &input, flags)?)?;
        assert_eq!(first["imported"], 2);
        assert_eq!(first["skipped"], 0);
        for index in 0..2 {
            assert_eq!(
                preview["transactions"][index]["reference"],
                first["transactions"][index]["reference"]
            );
        }
        assert_ne!(
            first["transactions"][0]["reference"],
            first["transactions"][1]["reference"]
        );
        assert!(
            first["transactions"][0]["reference"]
                .as_str()
                .is_some_and(|value| value.contains(":sha256:"))
        );
        assert_eq!(ledger.count("transactions")?, 2);
    }
    Ok(())
}

#[test]
fn reimporting_identical_unreferenced_rows_skips_all_for_both_formats() -> TestResult {
    for (name, input, flags) in [
        (
            "rows.csv",
            "date,description,amount\n2026-01-01,Receipt,10\n2026-01-01,Receipt,10.00\n".into(),
            BANK_FLAGS,
        ),
        (
            "rows.json",
            json!([journal(None), journal(None)]).to_string(),
            &[][..],
        ),
    ] {
        let ledger = Ledger::new()?;
        let first = success(&ledger.import(name, &input, flags)?)?;
        assert_eq!(first["imported"], 2);
        let again = success(&ledger.import(name, &input, flags)?)?;
        assert_eq!(again["imported"], 0);
        assert_eq!(again["skipped"], 2);
        for index in 0..2 {
            assert_eq!(
                first["transactions"][index]["reference"],
                again["transactions"][index]["reference"]
            );
        }
        assert_eq!(ledger.count("transactions")?, 2);
    }
    Ok(())
}

#[test]
fn overlapping_batch_imports_only_additional_identical_row_for_both_formats() -> TestResult {
    for (name, later_name, initial, later, flags) in [
        (
            "rows.csv",
            "later.csv",
            "date,description,amount\n2026-01-01,Receipt,10\n2026-01-01,Receipt,10.00\n".into(),
            "date,description,amount\n2026-01-01,Receipt,10.00\n2026-01-01,Receipt,10\n2026-01-01,Receipt,10.0\n".into(),
            BANK_FLAGS,
        ),
        (
            "rows.json",
            "later.json",
            json!([journal(None), journal(None)]).to_string(),
            json!([journal(None), journal(None), journal(None)]).to_string(),
            &[][..],
        ),
    ] {
        let ledger = Ledger::new()?;
        let first = success(&ledger.import(name, &initial, flags)?)?;
        assert_eq!(first["imported"], 2);
        let overlap = success(&ledger.import(later_name, &later, flags)?)?;
        assert_eq!(overlap["imported"], 1);
        assert_eq!(overlap["skipped"], 2);
        assert_eq!(overlap["transactions"][2]["status"], "imported");
        assert_eq!(ledger.count("transactions")?, 3);
    }
    Ok(())
}

#[test]
fn dry_runs_validate_batch_duplicates_without_persisting_any_rows() -> TestResult {
    for (name, input, mut flags) in [
        (
            "rows.csv",
            "date,description,amount,reference\n2026-01-01,Receipt,10,duplicate\n2026-01-01,Receipt,10,duplicate\n".into(),
            BANK_FLAGS.to_vec(),
        ),
        (
            "rows.json",
            json!([journal(Some("duplicate")), journal(Some("duplicate"))]).to_string(),
            vec![],
        ),
    ] {
        let ledger = Ledger::new()?;
        flags.push("--dry-run");
        let result = success(&ledger.import(name, &input, &flags)?)?;
        assert_eq!(result["dry_run"], true);
        assert_eq!(result["imported"], 1);
        assert_eq!(result["skipped"], 1);
        assert!(result["transactions"][0].get("id").is_none());
        assert_eq!(ledger.count("transactions")?, 0);
        assert_eq!(ledger.count("entries")?, 0);
        assert_eq!(ledger.count("audit_log")?, 0);
        flags.extend(["--on-conflict", "error"]);
        validation_error(&ledger.import(name, &input, &flags)?, "already exists")?;
        assert_eq!(ledger.count("transactions")?, 0);
    }
    Ok(())
}

#[test]
fn conflict_error_rolls_back_earlier_rows_for_both_formats() -> TestResult {
    for (name, initial, batch, mut flags) in [
        (
            "rows.csv",
            "date,description,amount,reference\n2026-01-01,Old,10,old\n".into(),
            "date,description,amount,reference\n2026-01-02,New,20,new\n2026-01-01,Old,10,old\n"
                .into(),
            BANK_FLAGS.to_vec(),
        ),
        (
            "rows.json",
            json!([journal(Some("old"))]).to_string(),
            json!([journal(Some("new")), journal(Some("old"))]).to_string(),
            vec![],
        ),
    ] {
        let ledger = Ledger::new()?;
        success(&ledger.import(name, &initial, &flags)?)?;
        flags.extend(["--on-conflict", "error"]);
        validation_error(&ledger.import(name, &batch, &flags)?, "already exists")?;
        assert_eq!(ledger.count("transactions")?, 1);
    }
    Ok(())
}

#[test]
fn invalid_json_postings_roll_back_valid_preceding_rows() -> TestResult {
    let ledger = Ledger::new()?;
    let mut cases = Vec::new();
    for (amount, message) in [
        ("12.35", "unbalanced"),
        ("0", "zero"),
        ("-12.34", "must be positive"),
        ("12.345", "decimal places"),
        ("92233720368547758.08", "too large"),
    ] {
        let mut input = journal(None);
        input["entries"][0]["amount"] = json!(amount);
        cases.push((input, message));
    }
    let mut missing = journal(None);
    missing["entries"][0]["account_code"] = json!("9999");
    cases.push((missing, "not found"));
    let mut empty = journal(None);
    empty["entries"] = json!([]);
    cases.push((empty, "at least two entries"));
    let mut currency = journal(None);
    currency["currency"] = json!("XYZ");
    cases.push((currency, "currency"));
    let mut date = journal(None);
    date["date"] = json!("2026-02-30");
    cases.push((date, "date"));
    cases.push((journal(Some(" ")), "reference must not be empty"));
    for (bad, message) in cases {
        validation_error(
            &ledger.import(
                "bad.json",
                &json!([journal(Some("first")), bad]).to_string(),
                &[],
            )?,
            message,
        )?;
        assert_eq!(ledger.count("transactions")?, 0, "case: {message}");
        assert_eq!(ledger.count("entries")?, 0);
    }
    Ok(())
}

#[test]
fn json_rejects_unknown_fields_directions_and_inexact_numbers() -> TestResult {
    let ledger = Ledger::new()?;
    let mut unknown = journal(None);
    unknown["unexpected"] = json!(true);
    let mut mixed_currency = journal(None);
    mixed_currency["entries"][0]["currency"] = json!("EUR");
    let mut direction = journal(None);
    direction["entries"][0]["direction"] = json!("incoming");
    let mut fraction = journal(None);
    fraction["entries"][0]["amount"] = json!(12.34);
    for input in [unknown, mixed_currency, direction, fraction] {
        validation_error(
            &ledger.import("bad.json", &json!([input]).to_string(), &[])?,
            "JSON import",
        )?;
        assert_eq!(ledger.count("transactions")?, 0);
    }
    Ok(())
}

#[test]
fn csv_rejects_invalid_headers_records_amounts_and_accounts() -> TestResult {
    let ledger = Ledger::new()?;
    for (input, message) in [
        ("date,description\n2026-01-01,Missing\n", "header 'amount'"),
        (
            "date,description,amount,amount\n2026-01-01,Duplicate,1,1\n",
            "duplicate CSV header",
        ),
        ("date,description,amount\n2026-01-01,Missing\n", "CSV row 2"),
        ("date,description,amount\n2026-01-01,Zero,0\n", "zero"),
        (
            "date,description,amount\n2026-01-01,Precision,0.001\n",
            "decimal places",
        ),
        ("date,description,amount\n2026-02-30,Date,1\n", "date"),
        (
            "date,description,amount\n2026-01-01,Too big,999999999999999999999\n",
            "too large",
        ),
    ] {
        validation_error(&ledger.import("bad.csv", input, BANK_FLAGS)?, message)?;
        assert_eq!(ledger.count("transactions")?, 0);
    }
    let input = "date,description,amount\n2026-01-01,Missing account,1\n";
    validation_error(
        &ledger.import(
            "bad.csv",
            input,
            &["--account", "9999", "--suspense", "9000"],
        )?,
        "not found",
    )?;
    validation_error(
        &ledger.import(
            "bad.csv",
            input,
            &["--account", "1000", "--suspense", "1000"],
        )?,
        "must be different",
    )?;
    for flags in [
        &[][..],
        &["--account", "1000"][..],
        &[
            "--account",
            "1000",
            "--suspense",
            "9000",
            "--on-conflict",
            "upsert",
        ][..],
    ] {
        assert_eq!(
            ledger.import("bad.csv", input, flags)?.status.code(),
            Some(2)
        );
    }
    Ok(())
}

#[test]
fn stdin_explicit_formats_and_legacy_alias_work() -> TestResult {
    let ledger = Ledger::new()?;
    let mut command = assert_cmd::Command::from_std(ledger.command()?);
    command
        .args(["txn", "import", "--file", "-", "--format", "json"])
        .write_stdin(json!([journal(Some("stdin"))]).to_string())
        .assert()
        .success();
    assert_eq!(ledger.count("transactions")?, 1);
    let mut command = assert_cmd::Command::from_std(ledger.command()?);
    command
        .args(["txn", "import", "--input-format", "csv"])
        .args(BANK_FLAGS)
        .write_stdin("date,description,amount\n2026-01-01,Stdin,5\n")
        .assert()
        .success();
    assert_eq!(ledger.count("transactions")?, 2);
    ledger
        .command()?
        .args(["txn", "import", "--file", "-"])
        .assert()
        .code(2);
    Ok(())
}

#[test]
fn references_are_scoped_to_company_and_distinguish_identical_rows() -> TestResult {
    let ledger = Ledger::new()?;
    let input = json!([journal(Some("first")), journal(Some("second"))]).to_string();
    assert_eq!(
        success(&ledger.import("same.json", &input, &[])?)?["imported"],
        2
    );
    let mut command = Command::cargo_bin("bk")?;
    let output = command
        .arg("--db")
        .arg(&ledger.path)
        .args(["--json", "--company", "other", "txn", "import", "--file"])
        .arg(ledger.dir.path().join("same.json"))
        .output()?;
    assert_eq!(success(&output)?["imported"], 2);
    assert_eq!(ledger.count("transactions")?, 4);
    Ok(())
}

#[test]
fn json_currency_defaults_and_integer_amounts_use_major_units() -> TestResult {
    let ledger = Ledger::new()?;
    let input = json!([{
        "date": "2026-01-01", "description": "JPY", "currency": "JPY",
        "entries": [
            {"account_code": "1000", "direction": "debit", "amount": 100},
            {"account_code": "9000", "direction": "credit", "amount": "100"}
        ]
    }, {
        "date": "2026-01-01", "description": "BHD default",
        "entries": [
            {"account_code": "1000", "direction": "debit", "amount": 1},
            {"account_code": "9000", "direction": "credit", "amount": "1.000"}
        ]
    }]);
    success(&ledger.import("currency.json", &input.to_string(), &["--currency", "BHD"])?)?;
    let db = Db::open_existing(&ledger.path, None)?;
    let mut stmt = db.conn().prepare("SELECT t.currency, e.amount FROM transactions t JOIN entries e ON t.id = e.transaction_id WHERE e.direction = 'debit' ORDER BY t.id")?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(rows, [("JPY".into(), 100), ("BHD".into(), 1000)]);
    Ok(())
}
