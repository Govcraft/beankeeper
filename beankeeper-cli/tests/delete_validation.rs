//! Deletion rejects ledger dependencies with semantic errors (issue #37).

use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

struct Books {
    dir: TempDir,
    path: String,
}

impl Books {
    fn new() -> Self {
        let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let books = Self {
            path: dir.path().join("books.db").to_string_lossy().into_owned(),
            dir,
        };
        books.ok(&["init"]);
        books.ok(&["company", "create", "co", "Company"]);
        books
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::cargo_bin("bk")
            .unwrap_or_else(|e| panic!("binary: {e}"))
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_COMPANY")
            .env_remove("BEANKEEPER_CURRENCY")
            .args(["--db", &self.path, "--company", "co", "--no-color"])
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("execute: {e}"))
    }

    fn ok(&self, args: &[&str]) {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn account(&self, code: &str) {
        self.ok(&["account", "create", code, "Cash", "--type", "asset"]);
    }

    fn validation(&self, args: &[&str], expected: &[&str]) {
        let before = self.row_counts();
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stderr);
        let envelope: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("error JSON ({e}): {text}"));
        assert_eq!(envelope["error"]["code"], "VALIDATION");
        let message = envelope["error"]["message"].as_str().unwrap_or_default();
        for needle in expected {
            assert!(message.contains(needle), "missing {needle:?}: {message}");
        }
        assert!(!message.contains("FOREIGN KEY"), "{message}");
        assert_eq!(
            self.row_counts(),
            before,
            "failed deletion must preserve ledger and audit"
        );
    }

    fn row_counts(&self) -> Vec<i64> {
        let conn = Connection::open(&self.path).unwrap_or_else(|e| panic!("open: {e}"));
        [
            "companies",
            "accounts",
            "transactions",
            "entries",
            "budgets",
            "attachments",
            "audit_log",
        ]
        .iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|e| panic!("count {table}: {e}"))
        })
        .collect()
    }
}

#[test]
fn posted_deletions_report_all_dependencies_and_preserve_ledger_and_audit() {
    let books = Books::new();
    books.account("1000");
    books.account("5000");
    books.ok(&[
        "txn", "post", "-d", "Rent", "--debit", "5000:100", "--credit", "1000:100",
    ]);
    books.ok(&[
        "budget", "set", "5000", "--year", "2026", "--month", "1", "--amount", "100",
    ]);
    let conn = Connection::open(&books.path).unwrap_or_else(|e| panic!("open: {e}"));
    let entry: i64 = conn
        .query_row(
            "SELECT id FROM entries WHERE account_code = '5000'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|e| panic!("entry: {e}"));
    let receipt = books.dir.path().join("receipt.txt");
    std::fs::write(&receipt, "Rent receipt").unwrap_or_else(|e| panic!("receipt: {e}"));
    books.ok(&[
        "txn",
        "attach",
        "1",
        &receipt.to_string_lossy(),
        "--type",
        "receipt",
        "--entry",
        &entry.to_string(),
    ]);

    books.validation(
        &["--json", "account", "delete", "5000", "--force"],
        &[
            "account 5000",
            "posted entries=1",
            "budget entries=1",
            "attachments=1",
            "append-only",
        ],
    );
    books.validation(
        &["--json", "company", "delete", "co", "--force"],
        &[
            "company 'co'",
            "accounts=2",
            "transactions=1",
            "posted entries=2",
            "budget entries=1",
            "attachments=1",
        ],
    );
}

#[test]
fn budget_only_account_returns_validation_instead_of_foreign_key_error() {
    let books = Books::new();
    books.account("5000");
    books.ok(&[
        "budget", "set", "5000", "--year", "2026", "--month", "1", "--amount", "100",
    ]);
    books.validation(
        &["--json", "account", "delete", "5000", "--force"],
        &["posted entries=0", "budget entries=1"],
    );
}

#[test]
fn company_with_only_accounts_is_rejected_then_empty_deletes_succeed_and_are_audited() {
    let books = Books::new();
    books.account("1000");
    books.validation(
        &["--json", "company", "delete", "co", "--force"],
        &["accounts=1", "transactions=0", "posted entries=0"],
    );
    books.ok(&["account", "delete", "1000", "--force"]);
    books.ok(&["company", "delete", "co", "--force"]);
    let counts = books.row_counts();
    assert_eq!(counts[0], 0);
    assert_eq!(counts[1], 0);
    let conn = Connection::open(&books.path).unwrap_or_else(|e| panic!("open: {e}"));
    let deleted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action IN ('account_delete', 'company_delete')",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|e| panic!("audit: {e}"));
    assert_eq!(deleted, 2);
}

#[test]
fn missing_entities_keep_not_found_errors() {
    let books = Books::new();
    for args in [
        vec!["account", "delete", "missing", "--force"],
        vec!["company", "delete", "missing", "--force"],
    ] {
        let output = books.run(&args);
        assert_eq!(
            output.status.code(),
            Some(5),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn budget_and_rule_deletions_require_force_outside_a_terminal()
-> Result<(), Box<dyn std::error::Error>> {
    let books = Books::new();
    books.account("5000");
    books.ok(&[
        "budget", "set", "5000", "--year", "2026", "--month", "3", "--amount", "100",
    ]);
    books.ok(&["rule", "create", "--contains", "rent", "--account", "5000"]);
    let db = Connection::open(&books.path)?;
    let rule_id: String =
        db.query_row("SELECT id FROM categorization_rules", [], |row| row.get(0))?;
    let budget = [
        "budget",
        "delete",
        "5000",
        "--year",
        "2026",
        "--month",
        "3",
        "--currency",
        "usd",
    ];
    let rule = ["rule", "delete", rule_id.as_str()];
    let before = books.row_counts();
    for args in [budget.as_slice(), rule.as_slice()] {
        let output = books.run(args);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("use --force to confirm deletion when stdin is not a terminal")
        );
    }
    assert_eq!(books.row_counts(), before);
    let rule_count: i64 = db.query_row("SELECT COUNT(*) FROM categorization_rules", [], |row| {
        row.get(0)
    })?;
    assert_eq!(rule_count, 1);
    for args in [budget.as_slice(), rule.as_slice()] {
        let mut forced = args.to_vec();
        forced.push("--force");
        books.ok(&forced);
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM budgets", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM categorization_rules", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        0
    );
    Ok(())
}
