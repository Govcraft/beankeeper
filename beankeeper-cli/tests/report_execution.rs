//! The in-process report API preserves CLI payloads without a stdout transport.

use std::error::Error;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::cli::{Cli, Command as BkCommand};
use beankeeper_cli::commands::report;
use beankeeper_cli::db::connection::Db;
use clap::Parser;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const CASES: &[&[&str]] = &[
    &[
        "trend",
        "--metric",
        "spending",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
    ],
    &[
        "trend",
        "--metric",
        "net-worth",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
    ],
    &[
        "trial-balance",
        "--from",
        "2000-01-01",
        "--to",
        "2099-12-31",
    ],
    &[
        "balance",
        "--account",
        "1000",
        "--from",
        "2000-01-01",
        "--to",
        "2099-12-31",
    ],
    &[
        "income-statement",
        "--from",
        "2000-01-01",
        "--to",
        "2099-12-31",
    ],
    &["balance-sheet", "--to", "2099-12-31"],
    &["tax-summary", "--from", "2000-01-01", "--to", "2099-12-31"],
    &[
        "ledger",
        "--account",
        "1000",
        "--from",
        "2000-01-01",
        "--to",
        "2099-12-31",
    ],
    &["budget-variance", "--year", "2026", "--include-unbudgeted"],
    &["cash-flow", "--from", "2000-01-01", "--to", "2099-12-31"],
    &[
        "aging",
        "--account",
        "1100",
        "--as-of",
        "2099-12-31",
        "--detail",
    ],
    &["reconciliation", "--account", "1000", "--to", "2099-12-31"],
    &[
        "trial-balance",
        "--from",
        "2026-01-01",
        "--to",
        "2026-03-31",
        "--period",
        "month",
    ],
    &[
        "income-statement",
        "--from",
        "2026-01-01",
        "--to",
        "2026-03-31",
        "--compare",
        "prior-year",
    ],
];

#[test]
fn in_process_reports_match_every_cli_report_payload() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("ledger.db");
    let output = Command::cargo_bin("bk")?
        .args(["--db"])
        .arg(&path)
        .args(["init", "--demo", "--quiet"])
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = Db::open_existing(&path, None)?;
    for args in CASES {
        let mut argv = vec!["bk", "--company", "acme-consulting", "--quiet", "report"];
        argv.extend_from_slice(args);
        // Deliberately omit --json: this API always selects its own JSON sink.
        let cli = Cli::try_parse_from(&argv)?;
        let BkCommand::Report(report_args) = &cli.command else {
            return Err("expected parsed report command".into());
        };
        let actual = report::execute_json(&cli, &db, "acme-consulting", &report_args.command)?;
        let output = Command::cargo_bin("bk")?
            .args(["--db"])
            .arg(&path)
            .args([
                "--company",
                "acme-consulting",
                "--quiet",
                "--json",
                "report",
            ])
            .args(*args)
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(actual, expected["data"], "{args:?}");
    }
    Ok(())
}

#[test]
fn in_process_reports_preserve_validation_and_missing_company_errors() -> TestResult {
    let db = Db::open_in_memory()?;
    beankeeper_cli::db::companies::create_company(db.conn(), "test", "Test", None)?;
    let cli = Cli::try_parse_from(["bk", "report", "trial-balance", "--from", "invalid"])?;
    let BkCommand::Report(args) = &cli.command else {
        return Err("expected report command".into());
    };
    let result = report::execute_json(&cli, &db, "test", &args.command);
    assert!(matches!(
        result,
        Err(beankeeper_cli::error::CliError::Validation(_))
    ));
    let result = report::execute_json(&cli, &db, "missing", &args.command);
    assert!(matches!(
        result,
        Err(beankeeper_cli::error::CliError::NotFound(_))
    ));
    Ok(())
}
