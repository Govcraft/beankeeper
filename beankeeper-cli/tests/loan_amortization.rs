//! Stateless loan schedules and payments through the existing audited ledger.

use std::{
    error::Error,
    path::PathBuf,
    process::{Command, Output},
};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{Db, accounts, companies};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

fn command() -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    for variable in [
        "BEANKEEPER_DB",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BEANKEEPER_PASSPHRASE_CMD",
        "BK_AGENT_TOKEN",
    ] {
        command.env_remove(variable);
    }
    Ok(command)
}

fn envelope(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("loan.db");
        let db = Db::open(&path, None)?;
        companies::create_company(db.conn(), "acme", "Acme", None)?;
        companies::create_company(db.conn(), "other", "Other", None)?;
        for (code, kind, tax) in [
            ("1000", "asset", None),
            ("2000", "liability", None),
            ("5000", "expense", Some("interest")),
        ] {
            accounts::create_account(db.conn(), "acme", code, code, kind, tax)?;
        }
        Ok(Self { _dir: dir, path })
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut cmd = command()?;
        cmd.arg("--db")
            .arg(&self.path)
            .args(["--json", "--actor", "loan-test"]);
        Ok(cmd)
    }

    fn pay(&self, terms: &[&str], flags: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(self
            .command()?
            .args(["loan", "pay", "--company", "acme"])
            .args(terms)
            .args([
                "--liability",
                "2000",
                "--interest-account",
                "5000",
                "--paying-account",
                "1000",
                "--date",
                "2026-11-20",
                "--reference",
                "CAR-1",
                "--payment-number",
                "1",
            ])
            .args(flags)
            .output()?)
    }

    fn transaction_count(&self) -> Result<i64, Box<dyn Error>> {
        let db = Db::open_existing(&self.path, None)?;
        Ok(db
            .conn()
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?)
    }
}

const TERMS: &[&str] = &["--principal", "200000", "--rate", "6", "--months", "360"];

#[test]
fn schedule_needs_no_company_or_database_and_has_display_companions() -> TestResult {
    let dir = tempdir()?;
    let missing = dir.path().join("missing.db");
    let output = command()?
        .arg("--db")
        .arg(&missing)
        .args(["loan", "schedule", "--json"])
        .args(TERMS)
        .output()?;
    let result = envelope(&output)?;
    assert_eq!(result["meta"]["command"], "loan.schedule");
    assert!(result["meta"].get("company").is_none());
    let data = &result["data"];
    assert_eq!(data["regular_payment"], 119_910);
    assert_eq!(data["regular_payment_display"], "1199.10");
    assert_eq!(data["principal_display"], "200000.00");
    assert_eq!(data["total_interest_display"], "231677.04");
    assert_eq!(data["payments"][0]["interest_display"], "1000.00");
    assert_eq!(data["payments"][359]["balance"], 0);
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn zero_and_three_decimal_currencies_use_their_own_minor_units() -> TestResult {
    for (currency, principal, raw, interest, display) in [
        ("JPY", "100", 100, 1, "101"),
        ("BHD", "12.345", 12_345, 62, "12.407"),
    ] {
        let output = command()?
            .args([
                "loan",
                "schedule",
                "--json",
                "--principal",
                principal,
                "--rate",
                "6",
                "--months",
                "1",
                "--currency",
                currency,
            ])
            .output()?;
        let result = envelope(&output)?;
        assert_eq!(result["data"]["principal"], raw);
        assert_eq!(result["data"]["payments"][0]["interest"], interest);
        assert_eq!(result["data"]["payments"][0]["payment_display"], display);
    }
    Ok(())
}

#[test]
fn schedule_table_csv_and_quiet_output_follow_cli_conventions() -> TestResult {
    let terms = ["--principal", "10", "--rate", "0", "--months", "3"];
    let table = command()?.args(["loan", "schedule"]).args(terms).output()?;
    assert!(table.status.success());
    let text = String::from_utf8(table.stdout)?;
    assert!(text.contains("Monthly payment: 3.33 USD"));
    assert!(text.contains("3.34"));
    let csv = command()?
        .args(["loan", "schedule", "--format", "csv"])
        .args(terms)
        .output()?;
    assert!(csv.status.success());
    assert_eq!(
        String::from_utf8(csv.stdout)?,
        "number,payment,principal,interest,balance\n1,333,333,0,667\n2,333,333,0,334\n3,334,334,0,0\n"
    );
    let quiet = command()?
        .args(["loan", "schedule", "--quiet"])
        .args(terms)
        .output()?;
    assert!(quiet.status.success());
    assert_eq!(quiet.stdout, [] as [u8; 0]);
    Ok(())
}

#[test]
fn payment_posts_the_split_with_audit_actor_and_account_tax_default() -> TestResult {
    let ledger = Ledger::new()?;
    let created = envelope(&ledger.pay(TERMS, &[])?)?;
    assert_eq!(created["meta"]["command"], "loan.pay");
    assert_eq!(created["meta"]["company"], "acme");
    assert_eq!(created["data"]["created"], true);
    let id = created["data"]["id"].to_string();
    let shown = envelope(
        &ledger
            .command()?
            .args(["--company", "acme", "txn", "show", &id])
            .output()?,
    )?;
    let txn = &shown["data"][0];
    assert_eq!(txn["posted_by"], "loan-test");
    assert_eq!(txn["reference"], "CAR-1");
    assert_eq!(txn["date"], "2026-11-20");
    let entries = txn["entries"].as_array().ok_or("missing entries")?;
    for (code, direction, amount) in [
        ("2000", "debit", 19_910),
        ("5000", "debit", 100_000),
        ("1000", "credit", 119_910),
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry["account_code"] == code)
            .ok_or("missing entry")?;
        assert_eq!(entry["direction"], direction);
        assert_eq!(entry["amount"], amount);
        if code == "5000" {
            assert_eq!(entry["tax_category"], "interest");
        }
    }
    let db = Db::open_existing(&ledger.path, None)?;
    let snapshot: String = db.conn().query_row(
        "SELECT after FROM audit_log WHERE entity = 'transaction' AND action = 'create'",
        [],
        |row| row.get(0),
    )?;
    let snapshot: Value = serde_json::from_str(&snapshot)?;
    assert_eq!(snapshot["entries"].as_array().map(Vec::len), Some(3));
    envelope(&ledger.command()?.arg("verify").output()?)?;
    Ok(())
}

#[test]
fn payment_retries_reject_or_skip_the_same_reference() -> TestResult {
    let ledger = Ledger::new()?;
    let first = envelope(&ledger.pay(TERMS, &[])?)?;
    let duplicate = ledger.pay(TERMS, &[])?;
    assert_eq!(duplicate.status.code(), Some(3));
    let skipped = envelope(&ledger.pay(TERMS, &["--on-conflict", "skip"])?)?;
    assert_eq!(skipped["data"]["existing_id"], first["data"]["id"]);
    assert_eq!(skipped["data"]["skipped"], true);
    assert_eq!(ledger.transaction_count()?, 1);
    let db = Db::open_existing(&ledger.path, None)?;
    let count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM audit_log WHERE entity = 'transaction' AND action = 'create'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn final_zero_rate_and_currency_specific_payments_skip_zero_lines() -> TestResult {
    for (currency, principal, months, number, expected) in [
        ("USD", "10", "3", "3", 334),
        ("JPY", "10", "3", "3", 4),
        ("BHD", "1.001", "3", "3", 333),
    ] {
        let ledger = Ledger::new()?;
        // Construct explicitly because clap rejects repeated scalar flags.
        let output = ledger
            .command()?
            .args([
                "--company",
                "acme",
                "loan",
                "pay",
                "--principal",
                principal,
                "--rate",
                "0",
                "--months",
                months,
                "--currency",
                currency,
                "--payment-number",
                number,
                "--liability",
                "2000",
                "--interest-account",
                "5000",
                "--paying-account",
                "1000",
                "--date",
                "2026-11-20",
                "--reference",
                "FINAL",
            ])
            .output()?;
        let id = envelope(&output)?["data"]["id"].to_string();
        let shown = envelope(
            &ledger
                .command()?
                .args(["--company", "acme", "txn", "show", &id])
                .output()?,
        )?;
        let entries = shown["data"][0]["entries"]
            .as_array()
            .ok_or("missing entries")?;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|row| row["amount"] == expected));
        assert_eq!(shown["data"][0]["currency"], currency);
    }
    Ok(())
}

#[test]
fn invalid_terms_return_structured_errors_without_posting() -> TestResult {
    let ledger = Ledger::new()?;
    for (principal, rate, months, currency) in [
        ("0", "6", "12", "USD"),
        ("-10", "6", "12", "USD"),
        ("10", "-1", "12", "USD"),
        ("10", "NaN", "12", "USD"),
        ("10", "101", "12", "USD"),
        ("10", "6.00001", "12", "USD"),
        ("10", "6", "0", "USD"),
        ("10", "6", "1201", "USD"),
        ("10.1", "6", "12", "JPY"),
        ("10", "6", "12", "XXX"),
        ("92233720368547758.07", "100", "120", "USD"),
    ] {
        let output = ledger.pay(
            &[
                "--principal",
                principal,
                "--rate",
                rate,
                "--months",
                months,
                "--currency",
                currency,
            ],
            &[],
        )?;
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let error: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["error"]["code"], "VALIDATION");
        assert_eq!(error["meta"]["command"], "loan.pay");
    }
    assert_eq!(ledger.transaction_count()?, 0);
    Ok(())
}

#[test]
fn invalid_accounts_dates_references_and_payment_numbers_leave_ledger_unchanged() -> TestResult {
    let ledger = Ledger::new()?;
    let base = [
        "loan",
        "pay",
        "--principal",
        "100",
        "--rate",
        "6",
        "--months",
        "12",
    ];
    for (company, liability, expense, asset, number, date, reference, exit) in [
        ("other", "2000", "5000", "1000", "1", "2026-11-20", "x", 5),
        ("missing", "2000", "5000", "1000", "1", "2026-11-20", "x", 5),
        ("acme", "1000", "5000", "1000", "1", "2026-11-20", "x", 3),
        ("acme", "2000", "1000", "1000", "1", "2026-11-20", "x", 3),
        ("acme", "2000", "5000", "2000", "1", "2026-11-20", "x", 3),
        ("acme", "2000", "5000", "1000", "0", "2026-11-20", "x", 3),
        ("acme", "2000", "5000", "1000", "13", "2026-11-20", "x", 3),
        ("acme", "2000", "5000", "1000", "1", "2026-02-30", "x", 3),
        ("acme", "2000", "5000", "1000", "1", "2026-11-20", " ", 3),
    ] {
        let output = ledger
            .command()?
            .args(base)
            .args([
                "--company",
                company,
                "--liability",
                liability,
                "--interest-account",
                expense,
                "--paying-account",
                asset,
                "--payment-number",
                number,
                "--date",
                date,
                "--reference",
                reference,
            ])
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let error: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["ok"], false);
    }
    assert_eq!(ledger.transaction_count()?, 0);
    let missing_company = ledger
        .command()?
        .args(base)
        .args([
            "--liability",
            "2000",
            "--interest-account",
            "5000",
            "--paying-account",
            "1000",
            "--payment-number",
            "1",
            "--date",
            "2026-11-20",
            "--reference",
            "x",
        ])
        .output()?;
    assert_eq!(missing_company.status.code(), Some(2));
    Ok(())
}

#[test]
fn calculator_claps_help_and_early_payoff_payment_number_validation() -> TestResult {
    use clap::CommandFactory;
    beankeeper_cli::cli::Cli::command().debug_assert();
    let help = command()?.args(["loan", "pay", "--help"]).output()?;
    assert!(help.status.success());
    let ledger = Ledger::new()?;
    let output = ledger
        .command()?
        .args([
            "--company",
            "acme",
            "loan",
            "pay",
            "--principal",
            "0.01",
            "--rate",
            "0",
            "--months",
            "1200",
            "--payment-number",
            "2",
            "--liability",
            "2000",
            "--interest-account",
            "5000",
            "--paying-account",
            "1000",
            "--date",
            "2026-11-20",
            "--reference",
            "small",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("1 to 1"))
    );
    assert_eq!(ledger.transaction_count()?, 0);
    Ok(())
}

#[test]
fn closed_period_payment_is_rejected_without_a_posting() -> TestResult {
    let ledger = Ledger::new()?;
    let output = ledger
        .command()?
        .args([
            "--company",
            "acme",
            "period",
            "close",
            "--through",
            "2026-11-20",
        ])
        .output()?;
    envelope(&output)?;
    let output = ledger.pay(TERMS, &[])?;
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("closed period"));
    assert_eq!(ledger.transaction_count()?, 0);
    Ok(())
}
