//! End-to-end evidence from document pricing through posting, credits and reconciliation.

use std::{
    error::Error,
    path::Path,
    process::{Command, Output, Stdio},
};

use assert_cmd::prelude::*;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

fn bk(path: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command
        .args(["--json", "--db"])
        .arg(path)
        .env("BEANKEEPER_CURRENCY", "GBP");
    for key in [
        "BEANKEEPER_DB",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_PASSPHRASE_CMD",
        "BK_AGENT_TOKEN",
        "BK_ACTOR",
    ] {
        command.env_remove(key);
    }
    Ok(command)
}

fn run(path: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    decode_output(&bk(path)?.args(args).output()?, args)
}

fn decode_output(output: &Output, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    Ok(envelope["data"].clone())
}

fn company(path: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let args = [&["--company", "acme"], args].concat();
    run(path, &args)
}

fn seed(path: &Path) -> TestResult {
    run(path, &["init"])?;
    run(path, &["company", "create", "acme", "Acme"])?;
    for (code, name, account_type) in [
        ("1000", "Cash", "asset"),
        ("1100", "Receivables", "asset"),
        ("1300", "Recoverable VAT", "asset"),
        ("2000", "Payables", "liability"),
        ("2200", "Output VAT", "liability"),
        ("4000", "Revenue", "revenue"),
        ("5000", "Purchases", "expense"),
    ] {
        company(
            path,
            &["account", "create", code, name, "--type", account_type],
        )?;
    }
    company(
        path,
        &[
            "txn",
            "post",
            "--description",
            "Opening seed",
            "--date",
            "2026-01-01",
            "--debit",
            "1000:1",
            "--credit",
            "4000:1",
        ],
    )?;
    Ok(())
}

fn configure(path: &Path, code: &str, date: &str, percent: &str) -> Result<Value, Box<dyn Error>> {
    company(
        path,
        &[
            "tax",
            "configure",
            code,
            "--effective-from",
            date,
            "--treatment",
            "taxable",
            "--rate",
            percent,
            "--output-account",
            "2200",
            "--input-account",
            "1300",
        ],
    )
}

fn selector<'a>(value: &'a Value, key: &'static str) -> Result<&'a str, Box<dyn Error>> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("no string at {key}: {value}").into())
}

fn issue(path: &Path, draft: &Value) -> Result<Value, Box<dyn Error>> {
    let saved = company(path, &["document", "draft", "--terms", &draft.to_string()])?;
    let id = selector(&saved["record"], "id")?;
    let args = [
        "--company",
        "acme",
        "document",
        "issue",
        id,
        "--date",
        "2026-06-01",
    ];
    let mut command = bk(path)?;
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let pending = command.spawn()?;
    let concurrent = command.output()?;
    let first = decode_output(&pending.wait_with_output()?, &args)?;
    let second = decode_output(&concurrent, &args)?;
    let retry = run(path, &args)?;
    assert_eq!(first["record"], second["record"]);
    assert_eq!(first["record"], retry["record"]);
    Ok(first["record"]["issued"].clone())
}

fn exempt_rate_codes(path: &Path) -> Result<(Value, Value), Box<dyn Error>> {
    let zero = company(
        path,
        &[
            "tax",
            "configure",
            "ZERO",
            "--effective-from",
            "2026-01-01",
            "--treatment",
            "zero-rated",
            "--rate",
            "0",
        ],
    )?;
    let exposed = company(
        path,
        &[
            "tax",
            "configure",
            "EXEMPT",
            "--effective-from",
            "2026-01-01",
            "--treatment",
            "exempt",
            "--rate",
            "0",
        ],
    )?;

    Ok((zero, exposed))
}

#[test]
fn vat_uses_frozen_rates_and_reconciles_credits_and_manual_entries() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("vat.db");
    seed(&path)?;
    let customer = company(
        &path,
        &[
            "counterparty",
            "create",
            "--name",
            "Buyer",
            "--role",
            "both",
        ],
    )?;
    let party = selector(&customer["party"], "id")?;
    let original = configure(&path, "MAIN", "2026-01-01", "20")?;
    let original_code = selector(&original, "code")?;
    assert_eq!(configure(&path, "MAIN", "2026-01-01", "20")?, original);
    let (zero, exposed) = exempt_rate_codes(&path)?;
    let invoice = issue(
        &path,
        &json!({
            "kind": "invoice", "number": "SALE-1", "counterparty_id": party, "control_account": "1100",
            "lines": [
                {"description":"exclusive", "quantity":"1.5", "unit_price_minor":1000, "account":"4000",
                 "tax":{"code":original_code}},
                {"description":"inclusive", "quantity":"1", "unit_price_minor":1200, "account":"4000",
                 "tax":{"code":original_code, "price_mode":"inclusive"}},
                {"description":"zero", "quantity":"1", "unit_price_minor":100, "account":"4000",
                 "tax":{"code":zero["code"]}},
                {"description":"exempt", "quantity":"1", "unit_price_minor":100, "account":"4000",
                 "tax":{"code":exposed["code"]}}
            ],
        }),
    )?;
    assert_eq!(invoice["total_minor"], 3200);
    assert_eq!(invoice["vat"][0]["net_minor"], 1500);
    assert_eq!(invoice["vat"][0]["vat_minor"], 300);
    assert_eq!(invoice["vat"][1]["net_minor"], 1000);
    assert_eq!(invoice["vat"][1]["gross_minor"], 1200);
    assert_eq!(
        invoice["vat"][2]["revision"]["terms"]["treatment"],
        "zero_rated"
    );
    assert_eq!(
        invoice["vat"][3]["revision"]["terms"]["treatment"],
        "exempt"
    );

    let bill = issue(
        &path,
        &json!({
            "kind":"bill", "number":"BILL-1", "counterparty_id":party, "control_account":"2000",
            "lines":[{"description":"goods", "quantity":"1", "unit_price_minor":1000, "account":"5000",
                      "tax":{"code":original_code}}]
        }),
    )?;
    assert_eq!(bill["total_minor"], 1200);
    let new_rate = configure(&path, "MAIN", "2026-07-01", "25")?;
    assert_eq!(new_rate["code"], original_code);
    assert_ne!(new_rate["id"], original["id"]);

    let id = selector(&invoice, "id")?;
    for (number, amount) in [("CREDIT-1", 500), ("CREDIT-2", 700)] {
        let payload = json!({"document_id":id,"number":number,"line":2,"amount_minor":amount,"date":"2026-08-01"});
        company(
            &path,
            &["document", "credit", "--credit", &payload.to_string()],
        )?;
    }
    company(
        &path,
        &[
            "txn",
            "post",
            "--description",
            "Manual tax correction",
            "--date",
            "2026-08-15",
            "--debit",
            "1000:0.35",
            "--credit",
            "2200:0.35",
        ],
    )?;

    let report_args = [
        "tax",
        "report",
        "--from",
        "2026-06-01",
        "--to",
        "2026-08-31",
    ];
    let report = assert_register(&path, &report_args, &original)?;
    assert_eq!(run(&path, &["verify"])?["status"], "healthy");
    assert_backup_restore(&path, &report_args, &report)
}

fn assert_backup_restore(path: &Path, report_args: &[&str], report: &Value) -> TestResult {
    let archive = path.with_file_name("backup.db");
    let archive = archive.to_str().ok_or("backup path")?;
    run(path, &["backup", "--output", archive])?;
    let rates_before = company(path, &["tax", "list"])?;
    configure(path, "MAIN", "2026-08-01", "15")?;
    run(path, &["restore", "--input", archive])?;
    assert_eq!(company(path, &["tax", "list"])?, rates_before);
    assert_eq!(&company(path, report_args)?, report);
    assert_eq!(run(path, &["verify"])?["status"], "healthy");
    Ok(())
}

fn assert_register(
    path: &Path,
    report_args: &[&str],
    original: &Value,
) -> Result<Value, Box<dyn Error>> {
    let report = company(path, report_args)?;
    assert_eq!(report["sales"]["vat_minor"], 300);
    assert_eq!(report["purchases"]["recovered_vat_minor"], 200);
    assert_eq!(report["net_control_vat_minor"], 100);
    assert_eq!(report["evidence"].as_array().map(Vec::len), Some(4));
    for (index, credit_tax) in [(2, 83), (3, 117)] {
        let row = &report["evidence"][index];
        assert_eq!(row["origin"], "credit");
        assert_eq!(row["lines"][0]["revision"]["id"], original["id"]);
        assert_eq!(row["lines"][0]["vat_minor"], credit_tax);
    }
    let payable = &report["controls"][1];
    assert_eq!(payable["account"], "2200");
    assert_eq!(payable["expected_minor"], 300);
    assert_eq!(payable["posted_minor"], 335);
    assert_eq!(payable["differences"][0]["posted_minor"], 35);
    Ok(report)
}
