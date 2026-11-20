//! Monetary presentation across the CLI's report and transaction contracts.

use std::{error::Error, path::Path, process::Command};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::tempdir;

fn run(db: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("bk")?
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn every_report_and_transaction_uses_its_currency_for_decimal_strings() -> Result<(), Box<dyn Error>>
{
    for (currency, minor, major, zero) in [
        ("USD", 180_000, "1800.00", "0.00"),
        ("JPY", 1800, "1800", "0"),
        ("BHD", 1_800_000, "1800.000", "0.000"),
    ] {
        let dir = tempdir()?;
        let db = dir.path().join("books.db");
        seed(&db, currency)?;
        for args in [vec!["txn", "list"], vec!["txn", "show", "1"]] {
            let mut full = vec!["--company", "co"];
            full.extend(args);
            let result = run(&db, &full)?;
            let entry = &result["data"][0]["entries"][0];
            assert_eq!(entry["amount"], minor);
            assert_eq!(entry["amount_display"], major);
        }
        run(
            &db,
            &[
                "--company",
                "co",
                "budget",
                "set",
                "1000",
                "--year",
                "2026",
                "--month",
                "3",
                "--amount",
                "1800",
                "--currency",
                currency,
            ],
        )?;
        let budget = run(
            &db,
            &["--company", "co", "budget", "list", "--year", "2026"],
        )?;
        assert_eq!(budget["data"][0]["amount_display"], major);

        verify_reports(&db, currency, minor, major)?;
        verify_columns(&db, major, zero)?;
    }
    Ok(())
}

fn seed(db: &Path, currency: &str) -> Result<(), Box<dyn Error>> {
    run(db, &["init"])?;
    run(db, &["company", "create", "co", "Company"])?;
    for (code, name, kind) in [("1000", "Cash", "asset"), ("4000", "Sales", "revenue")] {
        run(
            db,
            &[
                "--company",
                "co",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ],
        )?;
    }
    run(
        db,
        &[
            "--company",
            "co",
            "txn",
            "post",
            "-d",
            "Sale",
            "--date",
            "2026-03-01",
            "--debit",
            "1000:1800",
            "--credit",
            "4000:1800",
            "--currency",
            currency,
            "--reference",
            "sale",
            "--tax",
            "4000=sales",
        ],
    )?;
    Ok(())
}

fn verify_reports(
    db: &Path,
    currency: &str,
    minor: i64,
    major: &str,
) -> Result<(), Box<dyn Error>> {
    let cases: &[(&[&str], &str)] = &[
        (&["trial-balance"], "/total_debits"),
        (&["balance", "--account", "1000"], "/debit_total"),
        (&["income-statement"], "/total_revenue"),
        (&["balance-sheet"], "/total_assets"),
        (&["tax-summary"], "/0/credit_total"),
        (
            &[
                "budget-variance",
                "--year",
                "2026",
                "--type",
                "asset",
                "--currency",
                currency,
            ],
            "/totals/actual",
        ),
        (
            &["ledger", "--account", "1000"],
            "/accounts/0/closing_balance",
        ),
        (
            &[
                "aging",
                "--account",
                "1000",
                "--as-of",
                "2026-03-31",
                "--detail",
            ],
            "/totals/total_open",
        ),
        (
            &["reconciliation", "--account", "1000", "--to", "2026-03-31"],
            "/book_balance",
        ),
        (&["cash-flow"], "/net_change"),
    ];
    for (args, pointer) in cases {
        let mut full = vec!["--company", "co", "report"];
        full.extend_from_slice(args);
        if !matches!(args[0], "aging" | "reconciliation" | "budget-variance") {
            full.extend(["--to", "2026-03-31"]);
        }
        let result = run(db, &full)?;
        let data = &result["data"];
        assert_eq!(
            data.pointer(pointer),
            Some(&Value::from(minor)),
            "{args:?}: {data}"
        );
        assert_eq!(
            data.pointer(&format!("{pointer}_display")),
            Some(&Value::from(major)),
            "{args:?}: {data}"
        );
    }
    Ok(())
}

fn verify_columns(db: &Path, major: &str, zero: &str) -> Result<(), Box<dyn Error>> {
    let periods = run(
        db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2026-03-01",
            "--to",
            "2026-04-30",
            "--period",
            "month",
        ],
    )?;
    assert_eq!(
        periods["data"]["accounts"][0]["amounts_display"],
        serde_json::json!([major, zero])
    );
    let comparison = run(
        db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
            "--compare",
            "prior-period",
        ],
    )?;
    assert_eq!(comparison["data"]["accounts"][0]["current_display"], major);
    assert_eq!(comparison["data"]["accounts"][0]["prior_display"], zero);
    Ok(())
}
