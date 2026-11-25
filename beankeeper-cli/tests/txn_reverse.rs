//! Linked transaction reversals preserve the ledger and its audit history.

use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    _dir: TempDir,
    path: PathBuf,
    original: String,
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let mut books = Self {
            _dir: dir,
            path,
            original: String::new(),
        };
        books.json(&["init"])?;
        books.json(&["company", "create", "acme-consulting", "Acme Consulting"])?;
        books.json(&["company", "create", "acme-products", "Acme Products"])?;
        books.json(&["account", "create", "1000", "Cash", "--type", "asset"])?;
        books.json(&["account", "create", "5000", "Expense", "--type", "expense"])?;
        let posted = books.json(&[
            "txn",
            "post",
            "-d",
            "Split expense",
            "--date",
            "2026-01-01",
            "--debit",
            "5000:12.34:First item",
            "--debit",
            "5000:7.66:Second item",
            "--credit",
            "1000:20:Payment",
            "--currency",
            "MXN",
            "--tax",
            "5000=sched-c:18",
            "--reference",
            "ORIGINAL",
            "-m",
            r#"{"vendor":"AWS","applies_to":"invoice"}"#,
        ])?;
        books.original = posted["data"]["id"].to_string();
        Ok(books)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(Command::cargo_bin("bk")?
            .arg("--db")
            .arg(&self.path)
            .args([
                "--company",
                "acme-consulting",
                "--actor",
                "reversal-test",
                "--no-color",
            ])
            .args(args)
            .output()?)
    }

    fn json(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(args);
        let output = self.run(&json_args)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn show(&self, id: &str) -> Result<Value, Box<dyn Error>> {
        Ok(self.json(&["txn", "show", id])?["data"][0].clone())
    }

    fn reverse(&self) -> Result<String, Box<dyn Error>> {
        Ok(
            self.json(&["txn", "reverse", &self.original, "--date", "2026-02-01"])?["data"]["id"]
                .to_string(),
        )
    }

    fn count(&self) -> Result<Value, Box<dyn Error>> {
        Ok(self.json(&["txn", "list", "--count"])?["data"]["count"].clone())
    }

    fn assert_failure(&self, args: &[&str], code: i32, message: &str) -> TestResult {
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(args);
        let output = self.run(&json_args)?;
        assert_eq!(output.status.code(), Some(code));
        let error: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["ok"], false);
        assert_eq!(error["meta"]["command"], "txn.reverse");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(message)),
            "{error}"
        );
        Ok(())
    }
}

fn metadata(transaction: &Value) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(
        transaction["metadata"].as_str().ok_or("missing metadata")?,
    )?)
}

#[test]
fn split_reversal_preserves_details_links_both_ways_and_cancels_balances() -> TestResult {
    let books = Books::new()?;
    let before = books.show(&books.original)?;
    let id = books.reverse()?;
    let original = books.show(&books.original)?;
    let reversal = books.show(&id)?;
    assert_eq!(original["entries"], before["entries"]);
    for key in ["description", "reference", "date", "currency"] {
        assert_eq!(original[key], before[key]);
    }
    assert_eq!(metadata(&original)?["vendor"], "AWS");
    assert_eq!(metadata(&original)?["applies_to"], "invoice");
    assert_eq!(metadata(&original)?["reversed_by"].to_string(), id);
    assert_eq!(
        metadata(&reversal)?,
        json!({"reverses": books.original.parse::<i64>()?})
    );
    assert_eq!(reversal["date"], "2026-02-01");
    assert_eq!(reversal["currency"], "MXN");
    assert_eq!(
        reversal["reference"],
        format!("reversal-of-{}", books.original)
    );
    assert_eq!(
        reversal["description"],
        format!("Reversal of #{}: Split expense", books.original)
    );
    let reversed_entries = reversal["entries"].as_array().ok_or("missing entries")?;
    let entries = before["entries"].as_array().ok_or("missing entries")?;
    assert_eq!(reversed_entries.len(), entries.len());
    for (old, new) in entries.iter().zip(reversed_entries) {
        for key in ["account_code", "amount", "memo", "tax_category"] {
            assert_eq!(new[key], old[key]);
        }
        assert_ne!(new["direction"], old["direction"]);
        assert_eq!(new["status"], "uncleared");
        assert_eq!(new["cleared_amount"], 0);
        assert_eq!(new["clearings"], json!([]));
    }
    let conn = Connection::open(&books.path)?;
    let nonzero: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (SELECT account_code FROM entries WHERE company_slug = 'acme-consulting' \
         GROUP BY account_code HAVING SUM(CASE direction WHEN 'debit' THEN amount ELSE -amount END) != 0)",
        [], |row| row.get(0))?;
    assert_eq!(nonzero, 0);
    let list = books.run(&["txn", "list", "--include-reversed"])?;
    let table = String::from_utf8(list.stdout)?;
    assert!(table.contains(&format!("Reversed by #{id}")));
    assert!(table.contains(&format!("Reverses #{}", books.original)));
    let detail = books.run(&["txn", "show", &id])?;
    assert!(String::from_utf8(detail.stdout)?.contains(&format!("Reverses #{}", books.original)));
    let listed = books.json(&["txn", "list", "--include-reversed"])?;
    let numeric_id: i64 = id.parse()?;
    assert!(
        listed["data"]
            .as_array()
            .ok_or("missing list")?
            .iter()
            .any(|t| metadata(t).is_ok_and(|m| m["reversed_by"].as_i64() == Some(numeric_id)))
    );
    let audit = books.json(&["audit", "log", "--action", "reverse"])?;
    let rows = audit["data"].as_array().ok_or("missing audit rows")?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["actor"], "reversal-test");
    assert_eq!(rows[0]["entity_id"], books.original);
    assert_eq!(rows[0]["action"], "reverse");
    assert_eq!(
        serde_json::from_str::<Value>(rows[0]["before"].as_str().ok_or("missing before")?)?["metadata"],
        before["metadata"]
    );
    Ok(())
}

#[test]
fn list_hides_reversal_pairs_from_rows_counts_and_pagination() -> TestResult {
    let books = Books::new()?;
    books.reverse()?;
    let active = books.json(&[
        "txn",
        "post",
        "-d",
        "Active expense",
        "--date",
        "2026-03-01",
        "--debit",
        "5000:1",
        "--credit",
        "1000:1",
    ])?;
    let page = books.json(&["txn", "list", "--limit", "1"])?;
    assert_eq!(page["data"][0]["id"], active["data"]["id"]);
    assert_eq!(page["meta"]["pagination"]["total"], 1);
    assert_eq!(page["meta"]["pagination"]["has_more"], false);
    assert_eq!(books.count()?, 1);

    let history = books.json(&["txn", "list", "--include-reversed", "--limit", "1"])?;
    assert_eq!(history["data"][0]["id"].to_string(), books.original);
    assert_eq!(history["meta"]["pagination"]["total"], 3);
    assert_eq!(history["meta"]["pagination"]["has_more"], true);
    assert_eq!(
        books.json(&["txn", "list", "--count", "--include-reversed"])?["data"]["count"],
        3
    );
    assert_eq!(
        books.json(&["txn", "list", "--count", "--reference", "ORIGINAL"])?["data"]["count"],
        0
    );
    assert_eq!(
        books.json(&[
            "txn",
            "list",
            "--count",
            "--reference",
            "ORIGINAL",
            "--include-reversed",
        ])?["data"]["count"],
        1
    );
    Ok(())
}

#[test]
fn accounting_exports_keep_both_sides_of_a_reversal() -> TestResult {
    let books = Books::new()?;
    let reversal_id = books.reverse()?;
    let export = books.json(&["export"])?;
    let transactions = &export["beankeeper_export"]["companies"][0]["transactions"];
    assert_eq!(transactions.as_array().map(Vec::len), Some(2));
    assert_eq!(transactions[0]["id"].to_string(), books.original);
    assert_eq!(transactions[1]["id"].to_string(), reversal_id);

    let csv = books.run(&["export", "--output-format", "csv"])?;
    assert!(csv.status.success());
    let csv = String::from_utf8(csv.stdout)?;
    assert!(csv.contains(",2026-01-01,Split expense,"));
    assert!(csv.contains(&format!(
        ",2026-02-01,Reversal of #{}: Split expense,",
        books.original
    )));
    Ok(())
}

#[test]
fn explicit_reference_retries_skip_and_other_repeats_fail() -> TestResult {
    let books = Books::new()?;
    let args = [
        "txn",
        "reverse",
        &books.original,
        "--reference",
        "VOID-1",
        "--description",
        "Correcting expense",
    ];
    let created = books.json(&args)?;
    assert_eq!(created["meta"]["command"], "txn.reverse");
    let retry = books.json(&args)?;
    assert_eq!(retry["data"]["existing_id"], created["data"]["id"]);
    assert_eq!(retry["data"]["skipped"], true);
    assert_eq!(
        books.show(&created["data"]["id"].to_string())?["description"],
        "Correcting expense"
    );
    books.assert_failure(&["txn", "reverse", &books.original], 3, "already reversed")?;
    books.assert_failure(
        &["txn", "reverse", &books.original, "--reference", "OTHER"],
        3,
        "already reversed",
    )?;
    assert_eq!(books.count()?, 0);
    assert_eq!(
        books.json(&["audit", "log", "--action", "reverse"])?["data"]
            .as_array()
            .ok_or("audit rows")?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn void_alias_uses_today_and_default_reference_can_be_retried_explicitly() -> TestResult {
    let books = Books::new()?;
    let before = chrono::Local::now().format("%Y-%m-%d").to_string();
    let result = books.json(&["txn", "void", &books.original])?;
    assert_eq!(result["meta"]["command"], "txn.reverse");
    let reversal = books.show(&result["data"]["id"].to_string())?;
    let after = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(reversal["date"] == before || reversal["date"] == after);
    books.assert_failure(&["txn", "reverse", &books.original], 3, "already reversed")?;
    let reference = format!("reversal-of-{}", books.original);
    assert_eq!(
        books.json(&["txn", "reverse", &books.original, "--reference", &reference])?["data"]["skipped"],
        true
    );
    Ok(())
}

#[test]
fn missing_foreign_and_invalid_inputs_leave_no_writes() -> TestResult {
    let books = Books::new()?;
    books.assert_failure(&["txn", "reverse", "999999"], 5, "not found")?;
    books.assert_failure(
        &[
            "txn",
            "reverse",
            &books.original,
            "--company",
            "acme-products",
        ],
        5,
        "not found",
    )?;
    for date in ["nonsense", "2026-02-30", "2025-12-31"] {
        books.assert_failure(
            &["txn", "reverse", &books.original, "--date", date],
            3,
            "date",
        )?;
    }
    books.assert_failure(
        &["txn", "reverse", &books.original, "--reference", "  "],
        3,
        "must not be empty",
    )?;
    books.assert_failure(
        &["txn", "reverse", &books.original, "--reference", "ORIGINAL"],
        3,
        "already exists",
    )?;
    assert_eq!(books.count()?, 1);
    assert!(
        metadata(&books.show(&books.original)?)?
            .get("reversed_by")
            .is_none()
    );
    Ok(())
}

#[test]
fn cleared_original_keeps_its_clearings_and_reversal_is_uncleared() -> TestResult {
    let books = Books::new()?;
    books.json(&[
        "txn",
        "clear",
        "--reference",
        "ORIGINAL",
        "--account",
        "1000",
        "--amount",
        "5",
        "--date",
        "2026-01-02",
        "--statement",
        "JAN",
    ])?;
    let before = books.show(&books.original)?;
    let id = books.reverse()?;
    assert_eq!(books.show(&books.original)?["entries"], before["entries"]);
    let reversed = books.show(&id)?;
    assert!(
        reversed["entries"]
            .as_array()
            .ok_or("entries")?
            .iter()
            .all(|e| e["clearings"] == json!([]) && e["status"] == "uncleared")
    );
    Ok(())
}

#[test]
fn even_partial_reconciled_clearings_block_reversal() -> TestResult {
    let books = Books::new()?;
    books.json(&[
        "txn",
        "clear",
        "--reference",
        "ORIGINAL",
        "--account",
        "1000",
        "--amount",
        "5",
        "--date",
        "2026-01-02",
        "--status",
        "reconciled",
    ])?;
    books.assert_failure(
        &["txn", "reverse", &books.original],
        3,
        "reconciled clearings",
    )?;
    assert_eq!(books.count()?, 1);
    assert!(
        metadata(&books.show(&books.original)?)?
            .get("reversed_by")
            .is_none()
    );
    Ok(())
}

#[test]
fn failed_audit_write_rolls_back_posting_and_original_link() -> TestResult {
    let books = Books::new()?;
    let before = books.show(&books.original)?;
    let conn = Connection::open(&books.path)?;
    conn.execute_batch(
        "CREATE TRIGGER fail_reverse_audit BEFORE INSERT ON audit_log \
        WHEN NEW.action = 'reverse' BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;",
    )?;
    books.assert_failure(&["txn", "reverse", &books.original], 4, "audit unavailable")?;
    assert_eq!(books.count()?, 1);
    assert_eq!(books.show(&books.original)?, before);
    assert_eq!(
        books.json(&["audit", "log", "--action", "reverse"])?["data"],
        json!([])
    );
    conn.execute_batch("DROP TRIGGER fail_reverse_audit;")?;
    books.reverse()?;
    assert_eq!(books.count()?, 0);
    Ok(())
}
