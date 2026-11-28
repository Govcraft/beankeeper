//! Project distributions, reporting and concurrent source claims through the executable.

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

    fn project(&self, code: &str) -> Result<String, Box<dyn Error>> {
        let terms = json!({"code":code,"name":code,"customer":self.party,"currency":"USD"});
        id(&self.data(&["project", "create", "--terms", &terms.to_string()])?["project"])
    }

    fn report(&self) -> Result<Value, Box<dyn Error>> {
        self.data(&[
            "project",
            "report",
            "--month",
            "2026-01",
            "--currency",
            "USD",
            "--detail",
        ])
    }

    fn draft(
        &self,
        project: &str,
        number: &str,
        sources: &[Value],
    ) -> Result<String, Box<dyn Error>> {
        let terms = json!({
            "project":project, "number":number, "receivable_account":"1100", "revenue_account":"4000",
            "from":"2026-01-01", "through":"2026-01-10", "sources":sources,
        });
        id(
            &self.data(&["project", "draft-invoice", "--invoice", &terms.to_string()])?["result"]["record"],
        )
    }

    fn unbilled(&self, project: &str, cutoff: &str) -> Result<Value, Box<dyn Error>> {
        self.data(&[
            "project",
            "unbilled",
            project,
            "--from",
            "2026-01-01",
            "--through",
            cutoff,
        ])
    }

    fn credit(&self, document: &str, number: &str, amount: u32, date: &str) -> TestResult {
        let input = json!({
            "document_id":document,"number":number,"line":1,"amount_minor":amount,"date":date,
        });
        self.data(&["document", "credit", "--credit", &input.to_string()])?;
        Ok(())
    }
}

fn id(record: &Value) -> Result<String, Box<dyn Error>> {
    Ok(record["id"].as_str().ok_or("missing ID")?.into())
}

fn row<'a>(report: &'a Value, project: &str) -> Result<&'a Value, Box<dyn Error>> {
    report["projects"]
        .as_array()
        .ok_or("missing project rows")?
        .iter()
        .find(|row| row["project"]["id"] == project)
        .ok_or_else(|| format!("missing project {project}").into())
}

fn expense(books: &Books) -> Result<(String, String), Box<dyn Error>> {
    let posted = books.data(&[
        "txn",
        "post",
        "--date",
        "2026-01-03",
        "-d",
        "Printing",
        "--debit",
        "5000:100",
        "--credit",
        "1000:100",
    ])?;
    let posting = posted["id"].to_string();
    let details = books.data(&["txn", "show", &posting])?;
    let entry = details["entries"]
        .as_array()
        .ok_or("missing entries")?
        .iter()
        .find(|entry| entry["account_code"] == "5000")
        .ok_or("missing expense")?["id"]
        .to_string();
    Ok((posting, entry))
}

#[test]
fn split_expense_margin_budget_and_reversal_tie_to_ledger() -> TestResult {
    let books = Books::new()?;
    let oak = books.project("oak")?;
    let pine = books.project("pine")?;
    let (posting, entry) = expense(&books)?;
    let split = json!([
        {"project":oak,"amount_minor":6000,"billable":"billable"},
        {"project":pine,"amount_minor":3000},
    ])
    .to_string();
    let over = json!([{"project":oak,"amount_minor":10001}]).to_string();
    books
        .command()?
        .args([
            "project",
            "split",
            &entry,
            "--date",
            "2026-01-03",
            "--splits",
            &over,
        ])
        .assert()
        .code(3);
    books.data(&[
        "project",
        "split",
        &entry,
        "--date",
        "2026-01-03",
        "--splits",
        &split,
    ])?;
    let budget = json!({"project":oak,"month":"2026-01","amount_minor":5000});
    books.data(&["project", "budget", "--budget", &budget.to_string()])?;
    let before = books.report()?;
    assert_eq!(row(&before, &oak)?["expense_minor"], 6000);
    assert_eq!(row(&before, &oak)?["unbilled_expense_minor"], 6000);
    assert_eq!(row(&before, &oak)?["profit_minor"], -6000);
    assert_eq!(row(&before, &oak)?["budget_remaining_minor"], -1000);
    assert_eq!(row(&before, &pine)?["expense_minor"], 3000);
    assert_eq!(before["unallocated"]["expense_minor"], 1000);
    books.data(&["txn", "reverse", &posting, "--date", "2026-01-07"])?;
    let after = books.report()?;
    assert_eq!(row(&after, &oak)?["expense_minor"], 0);
    assert_eq!(row(&after, &pine)?["expense_minor"], 0);
    assert_eq!(row(&after, &oak)?["unbilled_expense_minor"], 0);
    assert_eq!(after["unallocated"]["expense_minor"], 0);
    assert_eq!(books.unbilled(&oak, "2026-01-05")?[0]["amount_minor"], 6000);
    books.data(&["verify"])?;
    Ok(())
}

fn approved_work(books: &Books, project: &str) -> Result<Value, Box<dyn Error>> {
    let args = json!({
        "project":project,"description":"Inspection","date":"2026-01-05","minutes":90,"rate_minor":6000,
    });
    let work = id(&books.data(&["project", "record", "--work", &args.to_string()])?["work"])?;
    assert_eq!(books.unbilled(project, "2026-01-05")?, json!([]));
    books.data(&["project", "approve", &work, "--date", "2026-01-06"])?;
    let source = &books.unbilled(project, "2026-01-06")?[0];
    assert_eq!(source["amount_minor"], 9000);
    Ok(source["source"].clone())
}

fn issue_race(books: &Books, first: &str, second: &str) -> Result<String, Box<dyn Error>> {
    use std::process::Stdio;
    let start = |id: &str| -> Result<_, Box<dyn Error>> {
        Ok(books
            .command()?
            .args(["document", "issue", id, "--date", "2026-01-11"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let first_child = start(first)?;
    let second_child = start(second)?;
    let first_result = first_child.wait_with_output()?;
    let second_result = second_child.wait_with_output()?;
    assert_ne!(
        first_result.status.success(),
        second_result.status.success(),
        "issue race: {:?}, {:?}",
        first_result,
        second_result
    );
    let (winner, failed) = if first_result.status.success() {
        (first, second_result)
    } else {
        (second, first_result)
    };
    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    Ok(winner.into())
}

#[test]
fn invoice_source_claim_is_atomic_and_credit_controls_rebilling() -> TestResult {
    let books = Books::new()?;
    let project = books.project("oak")?;
    let source = approved_work(&books, &project)?;
    let sources = &[source];
    let cancelled = books.draft(&project, "CANCELLED", sources)?;
    books.data(&["project", "cancel-invoice", &cancelled])?;
    books
        .command()?
        .args(["document", "issue", &cancelled, "--date", "2026-01-11"])
        .assert()
        .code(3);
    let first = books.draft(&project, "RACE-1", sources)?;
    let second = books.draft(&project, "RACE-2", sources)?;
    let billed = issue_race(&books, &first, &second)?;
    assert_eq!(books.unbilled(&project, "2026-01-11")?, json!([]));
    assert_eq!(row(&books.report()?, &project)?["revenue_minor"], 9000);
    assert_eq!(row(&books.report()?, &project)?["unbilled_time_minor"], 0);
    books.credit(&billed, "CREDIT-1", 3000, "2026-01-12")?;
    assert_eq!(books.unbilled(&project, "2026-01-12")?, json!([]));
    books.credit(&billed, "CREDIT-2", 6000, "2026-01-13")?;
    assert_eq!(
        books.unbilled(&project, "2026-01-13")?[0]["source"],
        sources[0]
    );
    assert_eq!(row(&books.report()?, &project)?["revenue_minor"], 0);
    let new_terms = json!({"project":project,"number":"RE-BILL","receivable_account":"1100",
        "revenue_account":"4000","from":"2026-01-01","through":"2026-01-13","sources":sources});
    let drafted = books.data(&[
        "project",
        "draft-invoice",
        "--invoice",
        &new_terms.to_string(),
    ])?;
    let id = id(&drafted["result"]["record"])?;
    books.data(&["document", "issue", &id, "--date", "2026-01-14"])?;
    books.data(&["project", "archive", &project])?;
    let archived = row(&books.report()?, &project)?.clone();
    assert_eq!(archived["project"]["archived"], true);
    assert_eq!(archived["profit_minor"], 9000);
    let late =
        json!({"project":project,"description":"More work","date":"2026-01-20","minutes":60});
    books
        .command()?
        .args(["project", "record", "--work", &late.to_string()])
        .assert()
        .code(3);
    books.data(&["verify"])?;
    Ok(())
}
