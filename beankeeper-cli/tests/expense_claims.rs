//! Receipt evidence and expense accounting exercised through the public CLI.

use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestError = Box<dyn std::error::Error>;

struct Ledger {
    dir: TempDir,
    party: String,
    receipt: String,
}

impl Ledger {
    fn run(&self, actor: &str, args: &[&str]) -> Result<Output, TestError> {
        Ok(Command::cargo_bin("bk")?
            .arg("--db")
            .arg(self.dir.path().join("books.db"))
            .args(["--company", "acme", "--actor", actor, "--json"])
            .args(args)
            .output()?)
    }

    fn data(&self, actor: &str, args: &[&str]) -> Result<Value, TestError> {
        let out = self.run(actor, args)?;
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let env: Value = serde_json::from_slice(&out.stdout)?;
        Ok(env["data"].clone())
    }

    fn error(&self, actor: &str, args: &[&str], code: &str) -> Result<(), TestError> {
        let out = self.run(actor, args)?;
        assert!(!out.status.success(), "{args:?}");
        let envelope: Value = serde_json::from_slice(&out.stderr)?;
        assert_eq!(envelope["error"]["code"], code, "{envelope}");
        Ok(())
    }

    fn new() -> Result<Self, TestError> {
        let mut book = Self {
            dir: tempfile::tempdir()?,
            party: String::new(),
            receipt: String::new(),
        };
        book.data("owner", &["init"])?;
        book.data("owner", &["company", "create", "acme", "Acme"])?;
        for (code, name, kind) in [
            ("1000", "Cash", "asset"),
            ("2100", "Employee reimbursement payable", "liability"),
            ("2200", "Company card", "liability"),
            ("5100", "Meals", "expense"),
            ("5200", "Supplies", "expense"),
        ] {
            book.data("owner", &["account", "create", code, name, "--type", kind])?;
        }
        let party = book.data(
            "owner",
            &[
                "counterparty",
                "create",
                "--name",
                "Synthetic claimant",
                "--role",
                "vendor",
            ],
        )?;
        party["party"]["id"]
            .as_str()
            .ok_or("party ID")?
            .clone_into(&mut book.party);

        let file = book.dir.path().join("receipt.pdf");
        let findings = book.dir.path().join("findings.json");
        std::fs::write(&file, b"%PDF-1.7\n%dummy fixture only\n%%EOF\n")?;
        std::fs::write(
            &findings,
            serde_json::to_vec(&json!({"total_minor":{
                "candidate":9999,"confidence":45,"evidence":"unconfirmed OCR amount"
            }}))?,
        )?;
        let result = book.data(
            "claimant",
            &[
                "expense-claim",
                "receipt-intake",
                "--file",
                file.to_str().ok_or("file")?,
                "--findings",
                findings.to_str().ok_or("findings")?,
            ],
        )?;
        result["id"]
            .as_str()
            .ok_or("receipt ID")?
            .clone_into(&mut book.receipt);
        assert_eq!(result["findings"]["total_minor"]["candidate"], 9999);
        let repeated = book.data(
            "claimant",
            &[
                "expense-claim",
                "receipt-intake",
                "--file",
                file.to_str().ok_or("file")?,
            ],
        )?;
        assert_eq!(repeated["id"], book.receipt);
        assert_eq!(repeated["hash"], result["hash"]);
        Ok(book)
    }

    fn draft(&self, funding: &Value) -> Result<Value, TestError> {
        let terms = self.dir.path().join("terms.json");
        std::fs::write(
            &terms,
            serde_json::to_vec(&json!({
                "claimant":self.party,"receipt":self.receipt,"merchant":"Synthetic cafe",
                "incurred_on":"2026-02-01","currency":"USD","amount_minor":1000,
                "splits":[{"account":"5100","amount_minor":600},{"account":"5200","amount_minor":400}],
                "funding":funding
            }))?,
        )?;
        self.data(
            "claimant",
            &[
                "expense-claim",
                "create",
                "--terms",
                terms.to_str().ok_or("terms path")?,
                "--reference",
                "draft-1",
            ],
        )
    }

    fn submit(&self, id: &str) -> Result<Value, TestError> {
        self.data(
            "claimant",
            &[
                "expense-claim",
                "submit",
                id,
                "--revision",
                "1",
                "--reference",
                "submit-1",
            ],
        )
    }

    fn approve(&self, id: &str) -> Result<Value, TestError> {
        self.data(
            "reviewer",
            &[
                "expense-claim",
                "approve",
                id,
                "--revision",
                "2",
                "--reference",
                "approve-1",
            ],
        )
    }

    fn post_payment(&self, amount: &str, reference: &str) -> Result<String, TestError> {
        let result = self.data(
            "owner",
            &[
                "txn",
                "post",
                "-d",
                "Reimburse",
                "--date",
                "2026-02-04",
                "--currency",
                "USD",
                "--debit",
                &format!("2100:{amount}"),
                "--credit",
                &format!("1000:{amount}"),
                "--reference",
                reference,
            ],
        )?;
        Ok(result["id"].to_string())
    }
}

#[test]
fn reimbursement_freezes_confirmed_total_and_matches_partial_payment() -> Result<(), TestError> {
    let book = Ledger::new()?;
    let draft = book.draft(&json!({"kind":"reimbursable","payable":"2100"}))?;
    let id = draft["id"].as_str().ok_or("claim ID")?;
    assert_eq!(draft["terms"]["amount_minor"], 1000);
    let submitted = book.submit(id)?;
    assert_eq!(submitted["status"], "submitted");
    book.error(
        "claimant",
        &[
            "expense-claim",
            "approve",
            id,
            "--revision",
            "2",
            "--reference",
            "approve-1",
        ],
        "FORBIDDEN",
    )?;
    let approval = book.approve(id)?;
    assert_eq!(approval["status"], "approved");
    assert_eq!(
        book.approve(id)?["approval_journal"],
        approval["approval_journal"]
    );
    assert!(approval["approval_journal"].is_number());
    let snapshot = book.dir.path().join("claims.bk-backup");
    let snapshot = snapshot.to_str().ok_or("backup path")?;
    book.data("owner", &["backup", "--output", snapshot])?;

    let first = book.post_payment("3.00", "pay-1")?;
    let result = book.data(
        "reconciler",
        &[
            "expense-claim",
            "record-payment",
            id,
            "--revision",
            "3",
            "--reference",
            "reconcile-1",
            "--transaction",
            &first,
        ],
    )?;
    assert_eq!(result["status"], "partially_reimbursed");
    assert_eq!(result["reimbursed_minor"], 300);
    let final_id = book.post_payment("7.00", "pay-2")?;
    let result = book.data(
        "reconciler",
        &[
            "expense-claim",
            "record-payment",
            id,
            "--revision",
            "4",
            "--reference",
            "reconcile-2",
            "--transaction",
            &final_id,
        ],
    )?;
    assert_eq!(result["status"], "reimbursed");
    assert_eq!(result["reimbursed_minor"], 1000);
    assert_eq!(book.data("owner", &["verify"])?["status"], "healthy");
    book.data("owner", &["restore", "--input", snapshot])?;
    let restored = book.data("owner", &["expense-claim", "show", id])?;
    assert_eq!(restored["status"], "approved");
    assert_eq!(restored["reimbursed_minor"], 0);
    let receipt = book.data("owner", &["expense-claim", "receipt-show", &book.receipt])?;
    assert_eq!(receipt["id"], book.receipt);
    assert_eq!(book.data("owner", &["verify"])?["status"], "healthy");
    Ok(())
}

#[test]
fn company_card_approval_attaches_receipt_without_a_second_posting() -> Result<(), TestError> {
    let book = Ledger::new()?;
    let posted = book.data(
        "owner",
        &[
            "txn",
            "post",
            "-d",
            "Synthetic card charge",
            "--date",
            "2026-02-03",
            "--currency",
            "USD",
            "--debit",
            "5100:6",
            "--debit",
            "5200:4",
            "--credit",
            "2200:10",
        ],
    )?;
    let draft = book.draft(&json!({"kind":"company_card","transaction":posted["id"]}))?;
    let id = draft["id"].as_str().ok_or("claim ID")?;
    book.submit(id)?;
    let result = book.approve(id)?;
    assert_eq!(result["status"], "card_matched");
    assert_eq!(result["approval_journal"], posted["id"]);
    let journal = book.data("owner", &["txn", "list"])?;
    assert_eq!(journal.as_array().map(Vec::len), Some(1));
    assert_eq!(book.data("owner", &["verify"])?["status"], "healthy");
    Ok(())
}

#[test]
fn closed_period_approval_leaves_submitted_claim_and_general_ledger_intact() -> Result<(), TestError>
{
    let book = Ledger::new()?;
    let draft = book.draft(&json!({"kind":"reimbursable","payable":"2100"}))?;
    let id = draft["id"].as_str().ok_or("claim ID")?;
    book.submit(id)?;
    book.data("owner", &["period", "close", "--through", "2026-02-28"])?;
    book.error(
        "reviewer",
        &[
            "expense-claim",
            "approve",
            id,
            "--revision",
            "2",
            "--reference",
            "approve-1",
        ],
        "VALIDATION",
    )?;
    assert_eq!(
        book.data("owner", &["expense-claim", "show", id])?["status"],
        "submitted"
    );
    assert_eq!(book.data("owner", &["txn", "list"])?, json!([]));
    Ok(())
}
