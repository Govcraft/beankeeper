//! Exercise company isolation, retained history and explicit grouping with the CLI.

use std::{error::Error, path::PathBuf, process::Command};

use assert_cmd::prelude::*;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("test.db");
        let books = Self {
            _directory: directory,
            path,
        };
        books.data(&["init"])?;
        for company in ["acme", "other"] {
            books.data(&["company", "create", company, company])?;
            for (code, name, kind) in [
                ("1000", "Cash", "asset"),
                ("1100", "Accounts Receivable", "asset"),
                ("4000", "Sales", "revenue"),
            ] {
                books.data(&[
                    "--company",
                    company,
                    "account",
                    "create",
                    code,
                    name,
                    "--type",
                    kind,
                ])?;
            }
        }
        Ok(books)
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command.arg("--db").arg(&self.path).arg("--json");
        for var in [
            "BEANKEEPER_DB",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
        ] {
            command.env_remove(var);
        }
        Ok(command)
    }

    fn data(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let result = self.command()?.args(args).output()?;
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let payload: Value = serde_json::from_slice(&result.stdout)?;
        Ok(payload["data"].clone())
    }

    fn acme(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut command = vec!["--company", "acme"];
        command.extend_from_slice(args);
        self.data(&command)
    }

    fn customer(&self) -> Result<String, Box<dyn Error>> {
        let data = self.acme(&[
            "counterparty",
            "create",
            "--name",
            "Clearwater",
            "--role",
            "customer",
            "--payment-terms",
            "30",
        ])?;
        Ok(data["party"]["id"]
            .as_str()
            .ok_or("missing counterparty ID")?
            .to_owned())
    }

    fn post(&self, company: &str, debit: &str, credit: &str) -> Result<String, Box<dyn Error>> {
        Ok(self.data(&[
            "--company",
            company,
            "txn",
            "post",
            "-d",
            "Activity",
            "--date",
            "2026-02-01",
            "--debit",
            debit,
            "--credit",
            credit,
            "--meta",
            "customer=Clearwater",
        ])?["id"]
            .to_string())
    }

    fn aging(&self, grouping: bool) -> Result<Value, Box<dyn Error>> {
        let mut args = vec![
            "report",
            "aging",
            "--account",
            "1100",
            "--as-of",
            "2026-04-01",
        ];
        if grouping {
            args.push("--group-by-counterparty");
        }
        self.acme(&args)
    }
}

#[test]
fn identical_names_stay_separate_through_rename_archive_and_aging() -> TestResult {
    let books = Books::new()?;
    let a = books.customer()?;
    let b = books.customer()?;
    assert_ne!(a, b);
    let charge = books.post("acme", "1100:100", "4000:100")?;
    let payment = books.post("acme", "1000:100", "1100:100")?;
    let preview = books.acme(&[
        "counterparty",
        "suggest",
        "--label",
        "customer",
        "--limit",
        "1",
    ])?;
    assert_eq!(preview["items"][0]["transaction_id"].to_string(), charge);
    assert_eq!(
        preview["items"][0]["candidates"]
            .as_array()
            .ok_or("missing candidates")?
            .len(),
        2
    );
    assert_eq!(preview["next_offset"], 1);
    let before = books.acme(&["txn", "show", &charge])?;
    let link = books.acme(&["counterparty", "associate", &a, &charge])?;
    assert!(link["receipt_id"].is_number());
    assert!(books.acme(&["counterparty", "associate", &a, &charge])?["receipt_id"].is_null());
    books.acme(&["counterparty", "associate", &b, &payment])?;
    let after = books.acme(&["txn", "show", &charge])?;
    assert_eq!(before["entries"], after["entries"]);
    let grouped = books.aging(true)?;
    let groups = grouped["groups"].as_array().ok_or("missing aging groups")?;
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .any(|group| group["counterparty"] == format!("Clearwater [{a}]"))
    );
    assert!(
        groups
            .iter()
            .any(|group| group["counterparty"] == format!("Clearwater [{b}]"))
    );
    assert_eq!(
        books.aging(false)?["groups"]
            .as_array()
            .ok_or("missing ungrouped aging")?
            .len(),
        1
    );

    let amended = books.acme(&[
        "counterparty",
        "amend",
        &a,
        "--name",
        "Clearwater North",
        "--clear-payment-terms",
    ])?;
    assert!(amended["party"]["payment_terms"].is_null());
    assert_eq!(amended["party"]["id"], a);
    books.acme(&["counterparty", "archive", &a])?;
    assert_eq!(books.acme(&["counterparty", "show", &a])?["archived"], true);
    assert_eq!(
        books.acme(&["counterparty", "list"])?["items"]
            .as_array()
            .ok_or("missing list")?
            .len(),
        1
    );
    let renamed = books.aging(true)?;
    assert!(
        renamed["groups"]
            .as_array()
            .ok_or("missing aging groups")?
            .iter()
            .any(|group| group["counterparty"] == format!("Clearwater North [{a}]"))
    );
    books
        .command()?
        .args([
            "--company",
            "acme",
            "counterparty",
            "associate",
            &a,
            &payment,
        ])
        .assert()
        .failure()
        .code(3);
    books.data(&["verify"])?;
    Ok(())
}

#[test]
fn company_scope_is_enforced_and_verify_detects_snapshot_drift() -> TestResult {
    let books = Books::new()?;
    let party = books.customer()?;
    let other = books.post("other", "1100:80", "4000:80")?;
    books
        .command()?
        .args([
            "--company",
            "other",
            "counterparty",
            "associate",
            &party,
            &other,
        ])
        .assert()
        .failure()
        .code(5);
    books
        .command()?
        .args([
            "--company",
            "acme",
            "counterparty",
            "associate",
            &party,
            &other,
        ])
        .assert()
        .failure()
        .code(5);
    books.data(&["verify"])?;

    let connection = rusqlite::Connection::open(&books.path)?;
    connection.execute(
        "UPDATE counterparties SET name = 'Drift' WHERE id = ?1",
        [&party],
    )?;
    books.command()?.args(["verify"]).assert().failure().code(4);
    Ok(())
}

#[test]
fn snapshot_restore_and_undo_follow_counterparty_bookkeeping_history() -> TestResult {
    let books = Books::new()?;
    let party = books.customer()?;
    let charge = books.post("acme", "1100:35", "4000:35")?;
    let snapshot_path = books.path.with_extension("bk-backup");
    let snapshot = snapshot_path.to_str().ok_or("non-UTF-8 snapshot path")?;
    books.acme(&["counterparty", "associate", &party, &charge])?;
    books.data(&["backup", "--output", snapshot])?;

    books.acme(&["counterparty", "amend", &party, "--name", "Revised"])?;
    let restored = books.data(&["restore", "--input", snapshot])?;
    assert_eq!(
        books.acme(&["counterparty", "show", &party])?["name"],
        "Clearwater"
    );
    let grouped = books.aging(true)?;
    assert_eq!(
        grouped["groups"][0]["counterparty"],
        format!("Clearwater [{party}]")
    );
    books.data(&["verify"])?;

    let undo = restored["undo_id"].to_string();
    books.data(&["restore-undo", &undo])?;
    assert_eq!(
        books.acme(&["counterparty", "show", &party])?["name"],
        "Revised"
    );
    let grouped = books.aging(true)?;
    assert_eq!(
        grouped["groups"][0]["counterparty"],
        format!("Revised [{party}]")
    );
    books.data(&["verify"])?;
    Ok(())
}

#[test]
fn summary_can_match_open_items_within_each_party() -> TestResult {
    let books = Books::new()?;
    let first = books.customer()?;
    let second = books.customer()?;
    let charge = books.post("acme", "1100:100", "4000:100")?;
    let payment = books.post("acme", "1000:100", "1100:100")?;
    books.acme(&["counterparty", "associate", &first, &charge])?;
    books.acme(&["counterparty", "associate", &second, &payment])?;

    let standard = books.acme(&["summary", "--to", "2026-04-01"])?;
    let grouped = books.acme(&["summary", "--to", "2026-04-01", "--group-by-counterparty"])?;
    assert_eq!(
        standard["needs_attention"]["items"]["aged_receivables"]["count"],
        0
    );
    assert_eq!(
        grouped["needs_attention"]["items"]["aged_receivables"]["count"],
        1
    );
    assert_eq!(
        grouped["needs_attention"]["items"]["aged_receivables"]["total"],
        10_000
    );
    assert_eq!(standard["standing"], grouped["standing"]);
    Ok(())
}
