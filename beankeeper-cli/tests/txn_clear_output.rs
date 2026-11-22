//! Currency-aware clearing messages and additive partial-clear status.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::tempdir;

fn run(db: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
    command
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(db)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap_or_else(|e| panic!("spawn bk: {e}"));
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap_or_else(|| panic!("piped stdin"))
            .write_all(input.as_bytes())
            .unwrap_or_else(|e| panic!("write input: {e}"));
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait bk: {e}"));
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup(db: &Path) {
    run(db, &["init"], None);
    run(db, &["company", "create", "co", "Company"], None);
    for (code, kind) in [("1200", "asset"), ("4000", "revenue")] {
        run(
            db,
            &[
                "--company",
                "co",
                "account",
                "create",
                code,
                code,
                "--type",
                kind,
            ],
            None,
        );
    }
}

fn post(db: &Path, currency: &str) {
    run(
        db,
        &[
            "--company",
            "co",
            "txn",
            "post",
            "--description",
            "Invoice",
            "--date",
            "2026-01-01",
            "--debit",
            "1200:1800",
            "--credit",
            "4000:1800",
            "--currency",
            currency,
            "--reference",
            currency,
        ],
        None,
    );
}

fn clear(db: &Path, currency: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "--company",
        "co",
        "--json",
        "txn",
        "clear",
        "--reference",
        currency,
        "--account",
        "1200",
        "--date",
        "2026-01-31",
    ];
    args.extend_from_slice(extra);
    run(db, &args, None)
}

fn data(output: &Output) -> Value {
    let envelope: Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("JSON: {e}"));
    envelope["data"].clone()
}

#[test]
fn clearing_messages_use_currency_precision_and_describe_partial_states() {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir.path().join("books.db");
    setup(&db);
    for (currency, partial_text, total_text, multiplier) in [
        ("USD", "1,620.00", "1,800.00", 100),
        ("JPY", "1,620", "1,800", 1),
        ("BHD", "1,620.000", "1,800.000", 1000),
    ] {
        post(&db, currency);
        let partial = clear(&db, currency, &["--amount", "1620", "--statement", "S1"]);
        let message = String::from_utf8_lossy(&partial.stderr);
        assert!(
            message.contains(&format!(
                "cleared {partial_text} of {total_text} {currency}, entry now partially cleared"
            )),
            "{message}"
        );
        assert!(!message.contains("minor units"), "{message}");
        let partial_data = data(&partial);
        assert_eq!(partial_data["currency"], currency);
        assert_eq!(partial_data["cleared_amount"], 1620 * multiplier);
        assert_eq!(partial_data["remaining_amount"], 180 * multiplier);
        assert_eq!(
            partial_data["amount_display"],
            partial_text.replace(',', "")
        );
        assert_eq!(partial_data["entry_status"], "uncleared");
        assert_eq!(partial_data["partially_cleared"], true);

        let skipped = clear(&db, currency, &["--amount", "1620", "--statement", "S1"]);
        assert!(String::from_utf8_lossy(&skipped.stderr).contains("[skip]"));
        assert_eq!(data(&skipped)["partially_cleared"], true);

        let full = clear(&db, currency, &["--statement", "S2"]);
        let message = String::from_utf8_lossy(&full.stderr);
        assert!(
            message.contains(&format!(
                "cleared {total_text} of {total_text} {currency}, entry now cleared"
            )),
            "{message}"
        );
        assert_eq!(data(&full)["partially_cleared"], false);
        assert_eq!(data(&full)["entry_status"], "cleared");

        let uncleared = clear(&db, currency, &["--status", "uncleared"]);
        assert!(String::from_utf8_lossy(&uncleared.stderr).contains("entry now uncleared"));
        assert_eq!(data(&uncleared)["cleared_amount"], 0);
        assert_eq!(data(&uncleared)["partially_cleared"], false);
    }
}

#[test]
fn batch_items_keep_each_currency_and_mixed_total_is_null() {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir.path().join("books.db");
    setup(&db);
    post(&db, "USD");
    post(&db, "BHD");
    let output = run(
        &db,
        &[
            "--company",
            "co",
            "--json",
            "txn",
            "clear",
            "--from-file",
            "-",
            "--account",
            "1200",
            "--date",
            "2026-01-31",
            "--statement",
            "S1",
        ],
        Some("USD,1620\nBHD,1620\n"),
    );
    let batch = data(&output);
    assert!(batch["currency"].is_null());
    assert_eq!(batch.get("cleared_total"), Some(&Value::Null));
    assert_eq!(batch["cleared"][0]["currency"], "USD");
    assert_eq!(batch["cleared"][1]["currency"], "BHD");
    assert_eq!(batch["cleared"][0]["partially_cleared"], true);
    assert_eq!(batch["cleared"][1]["partially_cleared"], true);
    assert_eq!(batch["cleared"][0]["amount_display"], "1620.00");
    assert_eq!(batch["cleared"][1]["amount_display"], "1620.000");
    assert!(batch["cleared_total_display"].is_null());
}
