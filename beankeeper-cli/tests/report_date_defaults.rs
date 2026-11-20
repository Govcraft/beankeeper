//! Regression coverage for the shared report date defaults (issue #39).

use assert_cmd::cargo::CommandCargoExt;
use chrono::{Datelike, Local};
use serde_json::Value;
use std::process::Command;
use tempfile::TempDir;

struct Books {
    _dir: TempDir,
    path: String,
}

impl Books {
    fn run(&self, args: &[&str]) -> String {
        let output = Command::cargo_bin("bk")
            .unwrap_or_else(|e| panic!("binary: {e}"))
            .args([
                "--db",
                &self.path,
                "--company",
                "co",
                "--no-color",
                "--quiet",
            ])
            .env_remove("BEANKEEPER_CURRENCY")
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("execute: {e}"));
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut json_args = vec!["--json", "report"];
        json_args.extend_from_slice(args);
        let out = self.run(&json_args);
        serde_json::from_str::<Value>(&out).unwrap_or_else(|e| panic!("json: {e}: {out}"))["data"]
            .clone()
    }

    fn new() -> Self {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let books = Self {
            path: dir.path().join("books.db").to_string_lossy().into_owned(),
            _dir: dir,
        };
        books.run(&["init"]);
        books.run(&["company", "create", "co", "Company"]);
        for (code, name, kind) in [("1000", "Cash", "asset"), ("4000", "Sales", "revenue")] {
            books.run(&["account", "create", code, name, "--type", kind]);
        }
        let today = Local::now().date_naive();
        for (date, amount) in [
            (format!("{}-12-31", today.year() - 1), "10"),
            (format!("{}-01-01", today.year()), "20"),
            (format!("{}-01-01", today.year() + 1), "30"),
        ] {
            books.run(&[
                "txn",
                "post",
                "-d",
                "sale",
                "--date",
                &date,
                "--debit",
                &format!("1000:{amount}"),
                "--credit",
                &format!("4000:{amount}"),
            ]);
        }
        books
    }
}

#[test]
fn point_in_time_reports_share_today_cutoff_and_keep_prior_history() {
    let books = Books::new();
    let today = Local::now().date_naive().to_string();
    for report in ["balance", "reconciliation"] {
        let data = books.json(&[report, "--account", "1000"]);
        let amount = if report == "balance" {
            &data["debit_total"]
        } else {
            &data["book_balance"]
        };
        assert_eq!(amount, 3000, "{report}: {data}");
        let table = books.run(&["report", report, "--account", "1000"]);
        assert!(table.contains(&today), "{table}");
    }
    let ledger = books.json(&["ledger", "--account", "1000"]);
    assert_eq!(ledger["to"], today);
    assert_eq!(ledger["accounts"][0]["closing_balance"], 3000);
    let aging = books.json(&["aging", "--account", "1000"]);
    assert_eq!(aging["as_of"], today);
    let sheet = books.json(&["balance-sheet"]);
    assert_eq!(sheet["total_assets"], 3000, "{sheet}");
    let trial = books.json(&["trial-balance"]);
    assert_eq!(trial["total_debits"], 3000, "{trial}");
}

#[test]
fn activity_and_multi_column_reports_default_to_year_to_date() {
    let books = Books::new();
    let today = Local::now().date_naive();
    for args in [
        vec!["income-statement"],
        vec!["cash-flow"],
        vec!["income-statement", "--period", "month"],
        vec!["trial-balance", "--compare", "prior-year"],
    ] {
        let data = books.json(&args);
        assert_eq!(
            data["from"],
            format!("{}-01-01", today.year()),
            "{args:?}: {data}"
        );
        assert_eq!(data["to"], today.to_string(), "{args:?}: {data}");
        if args == ["income-statement"] {
            assert_eq!(data["net_income"], 2000);
        }
        if args == ["cash-flow"] {
            assert_eq!(data["net_change"], 2000, "{data}");
        }
    }
}

#[test]
fn historical_end_selects_its_year_and_explicit_future_end_includes_future_activity() {
    let books = Books::new();
    let year = Local::now().year();
    let historical = format!("{}-12-31", year - 1);
    let data = books.json(&["income-statement", "--to", &historical]);
    assert_eq!(data["from"], format!("{}-01-01", year - 1));
    assert_eq!(data["net_income"], 1000);
    let future = format!("{}-01-01", year + 1);
    let data = books.json(&["balance", "--account", "1000", "--to", &future]);
    assert_eq!(data["debit_total"], 6000);
}
