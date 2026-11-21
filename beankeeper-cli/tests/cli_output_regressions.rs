//! Regression coverage for CLI presentation behavior.

use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use tempfile::{TempDir, tempdir};

fn command(db: &str, args: &[&str]) -> Command {
    let mut cmd =
        Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
        .arg("--db")
        .arg(db)
        .args(args);
    cmd
}

fn run(db: &str, args: &[&str]) -> Output {
    command(db, args)
        .output()
        .unwrap_or_else(|e| panic!("execute bk: {e}"))
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup() -> (TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir.path().join("books.db").to_string_lossy().into_owned();
    success(&run(&db, &["init"]));
    success(&run(&db, &["company", "create", "acme", "Acme"]));
    for (code, name, kind) in [("1000", "Cash", "asset"), ("5000", "Rent", "expense")] {
        success(&run(
            &db,
            &[
                "--company",
                "acme",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ],
        ));
    }
    (dir, db)
}

#[test]
fn attachment_hash_retains_full_json_and_abbreviated_table_forms() {
    let (dir, db) = setup();
    let posted = run(
        &db,
        &[
            "--company",
            "acme",
            "--json",
            "txn",
            "post",
            "-d",
            "Rent",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
        ],
    );
    success(&posted);
    let posted: serde_json::Value = serde_json::from_slice(&posted.stdout)
        .unwrap_or_else(|error| panic!("read posted transaction: {error}"));
    let transaction = posted["data"]["id"].to_string();
    let file = dir.path().join("receipt.txt");
    std::fs::write(&file, b"hello world").unwrap_or_else(|error| panic!("write receipt: {error}"));
    success(&run(
        &db,
        &[
            "--company",
            "acme",
            "txn",
            "attach",
            &transaction,
            &file.to_string_lossy(),
            "--type",
            "RECEIPT",
        ],
    ));

    let shown = run(
        &db,
        &["--company", "acme", "--json", "txn", "show", &transaction],
    );
    success(&shown);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout)
        .unwrap_or_else(|error| panic!("read transaction with attachment: {error}"));
    let attachment = &shown["data"][0]["attachments"][0];
    let hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    assert_eq!(attachment["hash"], hash);
    assert_eq!(attachment["document_type"], "receipt");
    assert_eq!(attachment["uri"], format!("attachments/{hash}"));

    let shown = run(&db, &["--company", "acme", "txn", "show", &transaction]);
    success(&shown);
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains("[receipt] receipt.txt  (b94d27b9934d)")
    );
}

#[test]
fn budget_tables_honor_color_policy_for_all_render_paths() {
    let (_dir, db) = setup();
    let commands: &[&[&str]] = &[
        &["set", "5000", "--year", "2026", "--annual", "14400"],
        &[
            "set", "5000", "--year", "2026", "--month", "1", "--amount", "1200",
        ],
        &["list", "--year", "2026"],
    ];
    for mode in ["no-color-flag", "NO_COLOR", "TERM", "pipe"] {
        for args in commands {
            let mut full = vec!["--company", "acme"];
            if mode == "no-color-flag" {
                full.push("--no-color");
            }
            full.push("budget");
            full.extend_from_slice(args);
            let mut cmd = command(&db, &full);
            cmd.env_remove("NO_COLOR").env("TERM", "xterm-256color");
            if mode == "NO_COLOR" {
                cmd.env("NO_COLOR", "1");
            }
            if mode == "TERM" {
                cmd.env("TERM", "dumb");
            }
            let output = cmd.output().unwrap_or_else(|e| panic!("execute bk: {e}"));
            success(&output);
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("5000"), "{text}");
            assert!(
                !text.contains('\x1b'),
                "mode {mode} should disable ANSI: {text:?}"
            );
        }
    }
}

#[test]
fn list_counts_appear_only_on_stderr_and_quiet_suppresses_them() {
    let (_dir, db) = setup();
    success(&run(
        &db,
        &[
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "Rent",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
        ],
    ));
    success(&run(
        &db,
        &[
            "--company",
            "acme",
            "budget",
            "set",
            "5000",
            "--year",
            "2026",
            "--month",
            "1",
            "--amount",
            "100",
        ],
    ));
    let cases: &[(&[&str], &str)] = &[
        (&["company", "list"], "1 company"),
        (&["--company", "acme", "account", "list"], "2 accounts"),
        (
            &["--company", "acme", "account", "list", "--with-balances"],
            "2 accounts",
        ),
        (&["--company", "acme", "txn", "list"], "1 transaction"),
        (
            &["--company", "acme", "budget", "list", "--year", "2026"],
            "1 budget entr",
        ),
    ];
    for (args, count) in cases {
        let output = run(&db, args);
        success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stdout.contains(count),
            "stdout should only contain table data: {stdout}"
        );
        assert_eq!(stderr.matches(count).count(), 1, "{stderr}");
        let mut quiet_args = vec!["-q"];
        quiet_args.extend_from_slice(args);
        let quiet = run(&db, &quiet_args);
        success(&quiet);
        assert_eq!(quiet.stdout, output.stdout);
        assert_eq!(
            quiet.stderr,
            Vec::<u8>::new(),
            "quiet stderr: {:?}",
            quiet.stderr
        );
    }
}

#[test]
fn posting_validation_formats_amounts_in_the_input_currency() {
    let (_dir, db) = setup();
    for (currency, hundred, ninety_nine, difference, negative, zero) in [
        ("USD", "100.00", "99.00", "1.00", "-100.00", "0.00"),
        ("JPY", "100", "99", "1", "-100", "0"),
        ("BHD", "100.000", "99.000", "1.000", "-100.000", "0.000"),
    ] {
        for (debit, credit, message) in [
            (
                "5000:100",
                "1000:99",
                format!(
                    "transaction is unbalanced: debits={hundred} {currency}, credits={ninety_nine} {currency} (difference {difference} {currency})"
                ),
            ),
            (
                "5000:99",
                "1000:100",
                format!(
                    "transaction is unbalanced: debits={ninety_nine} {currency}, credits={hundred} {currency} (difference {difference} {currency})"
                ),
            ),
            (
                "5000:-100",
                "1000:100",
                format!("entry amount must be positive, got {negative} {currency}"),
            ),
            (
                "5000:100",
                "1000:-100:credit memo",
                format!("entry amount must be positive, got {negative} {currency}"),
            ),
            (
                "5000:0",
                "1000:0",
                format!("entry amount must be positive, got {zero} {currency}"),
            ),
        ] {
            for json in [false, true] {
                let mut args = vec!["--company", "acme"];
                if json {
                    args.push("--json");
                }
                args.extend([
                    "txn",
                    "post",
                    "-d",
                    "Invalid",
                    "--currency",
                    currency,
                    "--debit",
                    debit,
                    "--credit",
                    credit,
                ]);
                let output = run(&db, &args);
                assert_eq!(output.status.code(), Some(3));
                assert_eq!(output.stdout, Vec::<u8>::new());
                let stderr = String::from_utf8_lossy(&output.stderr);
                if json {
                    let error: serde_json::Value = serde_json::from_str(&stderr)
                        .unwrap_or_else(|e| panic!("JSON error: {e}: {stderr}"));
                    assert_eq!(error["error"]["code"], "VALIDATION");
                    assert_eq!(error["error"]["message"], message);
                } else {
                    assert_eq!(stderr.trim(), format!("error: {message}"));
                }
            }
        }
    }
    let output = run(&db, &["--company", "acme", "--json", "txn", "list"]);
    success(&output);
    let list: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("JSON list: {e}"));
    assert!(list["data"].as_array().is_some_and(Vec::is_empty), "{list}");
}
