//! Invoice and bill lifecycle through the executable and its real ledger.

#[path = "documents/collections.rs"]
mod collections;

use std::{error::Error, path::PathBuf, process::Command};

use assert_cmd::prelude::*;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    _directory: tempfile::TempDir,
    path: PathBuf,
    party: String,
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("books.db");
        let mut books = Self {
            _directory: directory,
            path,
            party: String::new(),
        };
        books.data(&["init"])?;
        books.data(&["company", "create", "acme", "Acme"])?;
        for (code, title, kind) in [
            ("1000", "Cash", "asset"),
            ("1100", "Accounts Receivable", "asset"),
            ("2100", "Accounts Payable", "liability"),
            ("4000", "Sales", "revenue"),
            ("5000", "Supplies", "expense"),
        ] {
            books.data(&["account", "create", code, title, "--type", kind])?;
        }
        let created = books.data(&[
            "counterparty",
            "create",
            "--name",
            "Clearwater",
            "--role",
            "both",
            "--payment-terms",
            "30",
        ])?;
        created["party"]["id"]
            .as_str()
            .ok_or("party without ID")?
            .clone_into(&mut books.party);
        Ok(books)
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .args(["--company", "acme", "--json"]);
        for name in [
            "BEANKEEPER_DB",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
        ] {
            command.env_remove(name);
        }
        Ok(command)
    }

    fn data(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let output = self.command()?.args(args).output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: Value = serde_json::from_slice(&output.stdout)?;
        Ok(payload["data"].clone())
    }

    fn draft(&self, kind: &str, number: &str, price_minor: u32) -> Result<String, Box<dyn Error>> {
        let (control, offset) = if kind == "invoice" {
            ("1100", "4000")
        } else {
            ("2100", "5000")
        };
        let terms = json!({
            "kind":kind, "number":number, "counterparty_id":self.party, "control_account":control,
            "lines":[{"description":"Monthly service", "quantity":"1", "unit_price_minor":price_minor, "account":offset}],
        }).to_string();
        let drafted = self.data(&["document", "draft", "--terms", &terms])?;
        Ok(drafted["record"]["id"]
            .as_str()
            .ok_or("missing document ID")?
            .to_owned())
    }

    fn issue(&self, id: &str, date: &str) -> Result<Value, Box<dyn Error>> {
        self.data(&["document", "issue", id, "--date", date])
    }

    fn post(&self, date: &str, debit: &str, credit: &str) -> Result<String, Box<dyn Error>> {
        let txn = self.data(&[
            "txn", "post", "-d", "Payment", "--date", date, "--debit", debit, "--credit", credit,
        ])?;
        Ok(txn["id"].to_string())
    }

    fn aging(&self, as_of: &str) -> Result<Value, Box<dyn Error>> {
        self.data(&["document", "aging", "--as-of", as_of])
    }
}

fn row<'a>(aging: &'a Value, number: &str) -> Result<&'a Value, Box<dyn Error>> {
    aging["items"]
        .as_array()
        .ok_or("missing items")?
        .iter()
        .find(|item| item["document"]["number"] == number)
        .ok_or_else(|| format!("missing {number}").into())
}

#[test]
fn issue_resolves_due_date_and_prints_frozen_contact_details() -> TestResult {
    let books = Books::new()?;
    let id = books.draft("invoice", "INV-1", 10_000)?;
    let first = books.issue(&id, "2026-01-01")?;
    let again = books.issue(&id, "2026-01-01")?;
    assert!(again["receipt_id"].is_null());
    assert_eq!(
        first["record"]["issued"]["posting_id"],
        again["record"]["issued"]["posting_id"]
    );
    assert_eq!(first["record"]["issued"]["due_date"], "2026-01-31");
    let as_of = books.aging("2026-02-05")?;
    assert_eq!(row(&as_of, "INV-1")?["days_overdue"], 5);
    assert_eq!(row(&as_of, "INV-1")?["status"], "open");
    books.data(&[
        "counterparty",
        "amend",
        &books.party,
        "--name",
        "Renamed",
        "--payment-terms",
        "90",
    ])?;
    let printed = books.data(&["document", "print", &id])?;
    let content = printed["document"]
        .as_str()
        .ok_or("missing printable document")?;
    assert!(content.contains("Clearwater"));
    assert!(content.contains("2026-01-31"));
    assert!(!content.contains("Renamed"));
    let posting = first["record"]["issued"]["posting_id"].to_string();
    books
        .command()?
        .args(["txn", "reverse", &posting, "--date", "2026-02-06"])
        .assert()
        .code(3);
    books.data(&["verify"])?;
    Ok(())
}

#[test]
fn split_payments_credit_and_revocation_follow_the_effective_cutoff() -> TestResult {
    let books = Books::new()?;
    let first = books.draft("invoice", "INV-1", 10_000)?;
    let second = books.draft("invoice", "INV-2", 7_000)?;
    books.issue(&first, "2026-01-01")?;
    books.issue(&second, "2026-01-02")?;
    let payment = books.post("2026-02-06", "1000:120", "1100:120")?;
    let allocations = json!([
        {"document_id":first, "amount_minor":8000},
        {"document_id":second, "amount_minor":3000},
    ])
    .to_string();
    let applied = books.data(&[
        "document",
        "allocate",
        &payment,
        "--date",
        "2026-02-06",
        "--allocations",
        &allocations,
    ])?;
    assert_eq!(applied["unapplied_minor"], 1_000);
    let before = books.aging("2026-02-05")?;
    assert_eq!(row(&before, "INV-1")?["open_minor"], 10_000);
    let credit = json!({
        "document_id":second, "number":"CN-1", "line":1, "amount_minor":1000, "date":"2026-02-07"
    })
    .to_string();
    books.data(&["document", "credit", "--credit", &credit])?;
    let after = books.aging("2026-02-07")?;
    assert_eq!(row(&after, "INV-1")?["open_minor"], 2_000);
    assert_eq!(row(&after, "INV-2")?["open_minor"], 3_000);
    assert_eq!(row(&after, "INV-2")?["status"], "partial");
    let control = books.data(&[
        "report",
        "balance",
        "--account",
        "1100",
        "--to",
        "2026-02-07",
    ])?;
    let debit = control["debit_total"].as_i64().ok_or("control debits")?;
    let credit = control["credit_total"].as_i64().ok_or("control credits")?;
    assert_eq!(debit - credit, 4_000); // 5,000 open less 1,000 unapplied.
    let impossible = json!([{"document_id":second,"amount_minor":2000}]).to_string();
    books
        .command()?
        .args([
            "document",
            "allocate",
            &payment,
            "--date",
            "2026-02-07",
            "--allocations",
            &impossible,
        ])
        .assert()
        .code(3);
    let revoked = applied["items"][0]["id"]
        .as_str()
        .ok_or("missing allocation")?;
    books.data(&["document", "revoke", revoked, "--date", "2026-02-08"])?;
    assert_eq!(
        row(&books.aging("2026-02-07")?, "INV-1")?["open_minor"],
        2_000
    );
    assert_eq!(
        row(&books.aging("2026-02-08")?, "INV-1")?["open_minor"],
        10_000
    );
    books.data(&["verify"])?;
    Ok(())
}

#[test]
fn bills_apply_debit_settlements_and_closed_periods_block_issue() -> TestResult {
    let books = Books::new()?;
    let bill = books.draft("bill", "B-1", 5_000)?;
    books.data(&["period", "close", "--through", "2026-01-31"])?;
    books
        .command()?
        .args(["document", "issue", &bill, "--date", "2026-01-04"])
        .assert()
        .code(3);
    assert!(books.data(&["document", "show", &bill])?["issued"].is_null());
    books.issue(&bill, "2026-02-01")?;
    let payment = books.post("2026-02-02", "2100:50", "1000:50")?;
    let allocated = json!([{"document_id":bill,"amount_minor":5000}]).to_string();
    books.data(&[
        "document",
        "allocate",
        &payment,
        "--date",
        "2026-02-02",
        "--allocations",
        &allocated,
    ])?;
    let aging = books.aging("2026-04-01")?;
    assert_eq!(row(&aging, "B-1")?["status"], "paid");
    assert_eq!(row(&aging, "B-1")?["days_overdue"], 0);
    books.data(&["verify"])?;
    Ok(())
}
