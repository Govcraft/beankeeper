//! Integration tests for statement reconciliation with partial clearings
//! (issue #25 follow-ups): partial clears across statements, point-in-time
//! `--to`, the `--statement` filter, clear-by-reference, batch clearing from
//! stdin, idempotent re-runs, over-clear rejection, item output fields,
//! metadata filtering/grouping, and the audit trail.
//!
//! Scenario: commission receivables on account 1200, one transaction per
//! policy, each with a reference and carrier metadata.

use std::io::Write as _;
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

const CO: &str = "co";

struct Ledger {
    dir: TempDir,
    db: String,
}

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ledger {
    fn run_with(&self, args: &[&str], stdin: Option<&str>) -> Out {
        let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk should build: {e}"));
        cmd.arg("--db")
            .arg(&self.db)
            .arg("--no-color")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn bk: {e}"));
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap_or_else(|| panic!("stdin is piped"))
                .write_all(input.as_bytes())
                .unwrap_or_else(|e| panic!("write stdin: {e}"));
        }
        let output = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("wait bk: {e}"));
        Out {
            ok: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn co_run(&self, args: &[&str], stdin: Option<&str>) -> Out {
        let mut full = vec!["--company", CO];
        full.extend_from_slice(args);
        self.run_with(&full, stdin)
    }

    fn co(&self, args: &[&str]) -> String {
        let out = self.co_run(args, None);
        assert!(out.ok, "bk {args:?} should succeed: {}", out.stderr);
        out.stdout
    }

    fn json(&self, args: &[&str], stdin: Option<&str>) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.co_run(&full, stdin);
        assert!(out.ok, "bk {args:?} should succeed: {}", out.stderr);
        let v: Value = serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("valid JSON: {e}\n{}", out.stdout));
        assert_eq!(v["ok"], true);
        v["data"].clone()
    }

    fn report(&self, extra: &[&str]) -> Value {
        let mut args = vec!["report", "reconciliation", "--account", "1200"];
        args.extend_from_slice(extra);
        let data = self.json(&args, None);
        assert_invariant(&data);
        data
    }

    /// Clear by reference on 1200 with extra flags; returns the JSON data.
    fn clear_ref(&self, reference: &str, extra: &[&str]) -> Value {
        let mut args = vec![
            "txn",
            "clear",
            "--reference",
            reference,
            "--account",
            "1200",
        ];
        args.extend_from_slice(extra);
        self.json(&args, None)
    }

    fn batch(&self, input: &str, extra: &[&str]) -> Out {
        let mut args = vec![
            "--json",
            "txn",
            "clear",
            "--from-file",
            "-",
            "--account",
            "1200",
        ];
        args.extend_from_slice(extra);
        self.co_run(&args, Some(input))
    }
}

fn int(v: &Value, key: &str) -> i64 {
    v[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key} is an integer: {v}"))
}

fn items(data: &Value) -> Vec<Value> {
    data["uncleared_entries"]
        .as_array()
        .unwrap_or_else(|| panic!("uncleared_entries is an array: {data}"))
        .clone()
}

fn item<'a>(list: &'a [Value], reference: &str) -> Option<&'a Value> {
    list.iter().find(|e| e["reference"] == reference)
}

fn refs(data: &Value) -> Vec<String> {
    items(data)
        .iter()
        .map(|e| e["reference"].as_str().unwrap_or("").to_string())
        .collect()
}

fn assert_invariant(data: &Value) {
    let listed: i64 = items(data).iter().map(|e| int(e, "signed_remaining")).sum();
    assert_eq!(int(data, "uncleared_total"), listed, "{data}");
    assert_eq!(
        int(data, "book_balance") - int(data, "total_cleared_balance"),
        int(data, "uncleared_total"),
        "{data}"
    );
}

/// Ledger with receivables on 1200:
///
/// | ref     | date       | amount | metadata                          |
/// |---------|------------|--------|-----------------------------------|
/// | POL-1   | 2026-01-05 | 100    | {"carrier":"Acme","policy":"P1"}  |
/// | POL-2   | 2026-01-10 | 200    | {"carrier":"Beta"}                |
/// | POL-3   | 2026-01-15 | 50     | {"carrier":"Acme","policy":"P3"}  |
/// | POL-4   | 2026-01-20 | 80     | free text (not JSON)              |
fn seeded() -> Ledger {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("utf-8 path"))
        .to_string();
    let l = Ledger { dir, db };
    assert!(l.run_with(&["init"], None).ok);
    assert!(l.run_with(&["company", "create", CO, "Agency"], None).ok);
    for (code, name, ty) in [
        ("1000", "Checking", "asset"),
        ("1200", "Commission Receivable", "asset"),
        ("4000", "Commission Revenue", "revenue"),
    ] {
        l.co(&["account", "create", code, name, "--type", ty]);
    }
    for (reference, date, amount, meta) in [
        (
            "POL-1",
            "2026-01-05",
            "100",
            r#"{"carrier":"Acme","policy":"P1"}"#,
        ),
        ("POL-2", "2026-01-10", "200", r#"{"carrier":"Beta"}"#),
        (
            "POL-3",
            "2026-01-15",
            "50",
            r#"{"carrier":"Acme","policy":"P3"}"#,
        ),
        ("POL-4", "2026-01-20", "80", "free text"),
    ] {
        let debit = format!("1200:{amount}");
        let credit = format!("4000:{amount}");
        l.co(&[
            "txn",
            "post",
            "-d",
            &format!("Commission {reference}"),
            "--date",
            date,
            "--debit",
            &debit,
            "--credit",
            &credit,
            "--reference",
            reference,
            "--metadata",
            meta,
        ]);
    }
    l
}

#[test]
fn partial_clears_across_two_statements() {
    let l = seeded();

    let d = l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    assert_eq!(d["action"], "cleared");
    assert_eq!(int(&d, "amount"), 4_000);
    assert_eq!(int(&d, "cleared_amount"), 4_000);
    assert_eq!(int(&d, "remaining_amount"), 6_000);
    assert_eq!(d["entry_status"], "uncleared");
    assert_eq!(d["statement"], "S-JAN");
    assert_eq!(d["cleared_date"], "2026-01-31");

    // Partially cleared items stay listed with cleared/remaining amounts.
    let r = l.report(&[]);
    assert_eq!(int(&r, "partially_cleared_count"), 1);
    let list = items(&r);
    let pol1 = item(&list, "POL-1").unwrap_or_else(|| panic!("POL-1 listed: {r}"));
    assert_eq!(pol1["partially_cleared"], true);
    assert_eq!(int(pol1, "cleared_amount"), 4_000);
    assert_eq!(int(pol1, "remaining_amount"), 6_000);
    assert_eq!(int(pol1, "signed_remaining"), 6_000);
    assert_eq!(int(&r, "cleared_balance"), 4_000);

    // Second statement clears the rest (default: full remaining amount).
    let d = l.clear_ref("POL-1", &["--date", "2026-02-28", "--statement", "S-FEB"]);
    assert_eq!(int(&d, "amount"), 6_000);
    assert_eq!(int(&d, "remaining_amount"), 0);
    assert_eq!(d["entry_status"], "cleared");

    let r = l.report(&[]);
    assert!(item(&items(&r), "POL-1").is_none(), "{r}");
    assert_eq!(int(&r, "cleared_balance"), 10_000);
    assert_eq!(int(&r, "partially_cleared_count"), 0);
}

#[test]
fn to_is_point_in_time_for_clearings() {
    let l = seeded();
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    l.clear_ref("POL-1", &["--date", "2026-02-28", "--statement", "S-FEB"]);
    l.clear_ref("POL-2", &["--date", "2026-03-15", "--statement", "S-MAR"]);

    // Before any clearing: nothing cleared, even though entries are now
    // cleared in the current state.
    let r = l.report(&["--to", "2026-01-30"]);
    assert_eq!(int(&r, "cleared_balance"), 0);
    assert_eq!(int(&r, "book_balance"), 43_000);
    assert_eq!(int(&r, "uncleared_count"), 4);

    // As of January: POL-1 partially cleared.
    let r = l.report(&["--to", "2026-01-31"]);
    assert_eq!(int(&r, "cleared_balance"), 4_000);
    let pol1 = items(&r);
    let pol1 = item(&pol1, "POL-1").unwrap_or_else(|| panic!("POL-1 listed: {r}"));
    assert_eq!(int(pol1, "remaining_amount"), 6_000);

    // As of February: POL-1 fully cleared, POL-2 (cleared in March) not yet.
    let r = l.report(&["--to", "2026-02-28"]);
    assert_eq!(int(&r, "cleared_balance"), 10_000);
    assert_eq!(refs(&r), vec!["POL-2", "POL-3", "POL-4"]);
    let pol2 = items(&r);
    let pol2 = item(&pol2, "POL-2").unwrap_or_else(|| panic!("POL-2 listed: {r}"));
    assert_eq!(int(pol2, "cleared_amount"), 0);

    // Current state.
    let r = l.report(&[]);
    assert_eq!(int(&r, "cleared_balance"), 30_000);
    assert_eq!(refs(&r), vec!["POL-3", "POL-4"]);
}

#[test]
fn statement_filter_checks_one_statement() {
    let l = seeded();
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    l.clear_ref("POL-2", &["--date", "2026-01-31", "--statement", "S-JAN"]);
    l.clear_ref("POL-3", &["--date", "2026-02-28", "--statement", "S-FEB"]);

    let r = l.report(&["--statement", "S-JAN", "--statement-balance", "240"]);
    assert_eq!(r["statement"], "S-JAN");
    assert_eq!(int(&r, "cleared_balance"), 24_000);
    assert_eq!(int(&r, "total_cleared_balance"), 29_000);
    assert_eq!(int(&r, "difference"), 0);
    assert_eq!(r["reconciled"], true);
    // Outstanding items reflect every statement's clearings.
    assert_eq!(refs(&r), vec!["POL-1", "POL-4"]);

    let r = l.report(&["--statement", "S-FEB", "--statement-balance", "240"]);
    assert_eq!(int(&r, "cleared_balance"), 5_000);
    assert_eq!(r["reconciled"], false);

    // Unknown statement: nothing cleared for it.
    let r = l.report(&["--statement", "S-NONE"]);
    assert_eq!(int(&r, "cleared_balance"), 0);

    // Table labels the two cleared balances.
    let table = l.co(&[
        "report",
        "reconciliation",
        "--account",
        "1200",
        "--statement",
        "S-JAN",
        "--statement-balance",
        "240",
    ]);
    assert!(table.contains("Statement: S-JAN"), "{table}");
    assert!(table.contains("this statement"), "{table}");
    assert!(table.contains("reconciled: yes"), "{table}");
}

#[test]
fn clear_by_reference_resolution_errors() {
    let l = seeded();
    // A transaction with two entries on 1200.
    l.co(&[
        "txn",
        "post",
        "-d",
        "Split",
        "--date",
        "2026-01-25",
        "--debit",
        "1200:10",
        "--debit",
        "1200:5",
        "--credit",
        "4000:15",
        "--reference",
        "POL-SPLIT",
    ]);

    let missing = l.co_run(
        &["txn", "clear", "--reference", "NOPE", "--account", "1200"],
        None,
    );
    assert_eq!(missing.code, Some(5), "{}", missing.stderr);

    let no_entry = l.co_run(
        &["txn", "clear", "--reference", "POL-1", "--account", "1000"],
        None,
    );
    assert_eq!(no_entry.code, Some(5), "{}", no_entry.stderr);

    let ambiguous = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-SPLIT",
            "--account",
            "1200",
        ],
        None,
    );
    assert_eq!(ambiguous.code, Some(3), "{}", ambiguous.stderr);
    assert!(
        ambiguous.stderr.contains("ambiguous"),
        "{}",
        ambiguous.stderr
    );

    // --reference requires --account.
    let usage = l.co_run(&["txn", "clear", "--reference", "POL-1"], None);
    assert_eq!(usage.code, Some(2));

    // Neither target given.
    let usage = l.co_run(&["txn", "clear"], None);
    assert_eq!(usage.code, Some(2));

    // Invalid date.
    let bad = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--date",
            "Jan 31",
        ],
        None,
    );
    assert_eq!(bad.code, Some(3));
}

#[test]
fn over_clearing_is_rejected() {
    let l = seeded();
    let over = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--amount",
            "100.01",
        ],
        None,
    );
    assert_eq!(over.code, Some(3), "{}", over.stderr);
    assert!(over.stderr.contains("over-clearing"), "{}", over.stderr);

    l.clear_ref("POL-1", &["--amount", "60"]);
    let over = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--amount",
            "41",
        ],
        None,
    );
    assert_eq!(over.code, Some(3));
    let r = l.report(&[]);
    let list = items(&r);
    let pol1 = item(&list, "POL-1").unwrap_or_else(|| panic!("POL-1 listed"));
    assert_eq!(int(pol1, "cleared_amount"), 6_000);
}

#[test]
fn batch_from_stdin_is_atomic_and_idempotent() {
    let l = seeded();
    let input = "reference,amount\n# January statement\nPOL-1,40\n\nPOL-2\n";
    let flags = ["--date", "2026-01-31", "--statement", "S-JAN"];

    let out = l.batch(input, &flags);
    assert!(out.ok, "{}", out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("json: {e}"));
    assert_eq!(v["meta"]["command"], "txn.clear");
    let d = &v["data"];
    assert_eq!(d["statement"], "S-JAN");
    assert_eq!(int(d, "cleared_count"), 2);
    assert_eq!(int(d, "skipped_count"), 0);
    assert_eq!(int(d, "cleared_total"), 24_000);
    assert_eq!(d["cleared"][0]["reference"], "POL-1");
    assert_eq!(int(&d["cleared"][0], "line"), 3);
    assert_eq!(int(&d["cleared"][0], "remaining_amount"), 6_000);
    assert_eq!(d["cleared"][1]["entry_status"], "cleared");

    // Re-running the same statement batch changes nothing.
    let out = l.batch(input, &flags);
    assert!(out.ok, "{}", out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("json: {e}"));
    let d = &v["data"];
    assert_eq!(int(d, "cleared_count"), 0);
    assert_eq!(int(d, "skipped_count"), 2);
    assert_eq!(
        d["skipped"][0]["skip_reason"],
        "already_cleared_by_statement"
    );
    let r = l.report(&[]);
    assert_eq!(int(&r, "cleared_balance"), 24_000);

    // Same statement, different amount: a conflict, and nothing changes.
    let out = l.batch("POL-1,45\n", &flags);
    assert_eq!(out.code, Some(3), "{}", out.stderr);

    // A failing line rolls back the whole batch (POL-3 stays uncleared).
    let out = l.batch(
        "POL-3\nPOL-4,500\n",
        &["--date", "2026-02-28", "--statement", "S-FEB"],
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("line 2"), "{}", out.stderr);
    let out = l.batch("POL-3\nPOL-404\n", &["--statement", "S-FEB"]);
    assert_eq!(out.code, Some(5), "{}", out.stderr);
    let r = l.report(&[]);
    let list = items(&r);
    let pol3 = item(&list, "POL-3").unwrap_or_else(|| panic!("POL-3 listed"));
    assert_eq!(int(pol3, "cleared_amount"), 0);
    assert_eq!(int(&r, "cleared_balance"), 24_000);

    // A batch from a file works the same way.
    let path = l.dir.path().join("feb.csv");
    std::fs::write(&path, "POL-3\n").unwrap_or_else(|e| panic!("write: {e}"));
    let path = path.to_str().unwrap_or_else(|| panic!("utf-8"));
    let d = l.json(
        &[
            "txn",
            "clear",
            "--from-file",
            path,
            "--account",
            "1200",
            "--statement",
            "S-FEB",
        ],
        None,
    );
    assert_eq!(int(&d, "cleared_count"), 1);
}

#[test]
fn reconciled_status_promotes_a_statement() {
    let l = seeded();
    let flags = ["--date", "2026-01-31", "--statement", "S-JAN"];
    assert!(l.batch("POL-1,40\nPOL-2\n", &flags).ok);

    let out = l.batch(
        "POL-1,40\nPOL-2\n",
        &[&flags[..], &["--status", "reconciled"]].concat(),
    );
    assert!(out.ok, "{}", out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("json: {e}"));
    assert_eq!(int(&v["data"], "cleared_count"), 2);
    assert_eq!(v["data"]["cleared"][0]["action"], "reconciled");
    // POL-2 is fully reconciled; POL-1 is only partially cleared.
    assert_eq!(v["data"]["cleared"][1]["entry_status"], "reconciled");
    assert_eq!(v["data"]["cleared"][0]["entry_status"], "uncleared");

    let r = l.report(&["--statement", "S-JAN"]);
    assert_eq!(int(&r, "reconciled_balance"), 24_000);

    // Unclearing the statement for POL-2 removes its clearing.
    let d = l.clear_ref("POL-2", &["--status", "uncleared", "--statement", "S-JAN"]);
    assert_eq!(d["action"], "uncleared");
    assert_eq!(int(&d, "amount"), 20_000);
    assert_eq!(d["entry_status"], "uncleared");
}

#[test]
fn legacy_txn_clear_without_new_flags() {
    let l = seeded();
    // POL-1 is transaction 1; its 1200 entry is the debit (entry 1).
    let d = l.json(&["txn", "clear", "1", "--entry", "1"], None);
    assert_eq!(d["status"], "cleared");
    assert_eq!(d["entry_status"], "cleared");
    assert_eq!(int(&d, "amount"), 10_000);

    // Clearing again is a harmless no-op.
    let d = l.json(&["txn", "clear", "1", "--entry", "1"], None);
    assert_eq!(d["action"], "skipped");
    assert_eq!(d["skip_reason"], "fully_cleared");

    let d = l.json(
        &[
            "txn",
            "clear",
            "1",
            "--entry",
            "1",
            "--status",
            "reconciled",
        ],
        None,
    );
    assert_eq!(d["entry_status"], "reconciled");

    let d = l.json(
        &["txn", "clear", "1", "--entry", "1", "--status", "uncleared"],
        None,
    );
    assert_eq!(d["entry_status"], "uncleared");
    assert_eq!(int(&d, "cleared_amount"), 0);

    // --amount with --status uncleared is a usage error.
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "1",
            "--entry",
            "1",
            "--status",
            "uncleared",
            "--amount",
            "1",
        ],
        None,
    );
    assert_eq!(out.code, Some(2));
}

#[test]
fn output_fields_in_json_table_and_csv() {
    let l = seeded();
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );

    let r = l.report(&[]);
    let list = items(&r);
    let pol1 = item(&list, "POL-1").unwrap_or_else(|| panic!("POL-1 listed"));
    assert_eq!(pol1["metadata"]["carrier"], "Acme");
    assert_eq!(pol1["metadata"]["policy"], "P1");
    let pol4 = item(&list, "POL-4").unwrap_or_else(|| panic!("POL-4 listed"));
    // Free-text `-m` metadata is stored as `{"memo": ...}` (issue #17).
    assert_eq!(pol4["metadata"]["memo"], "free text");

    let table = l.co(&["report", "reconciliation", "--account", "1200"]);
    for needle in [
        "Reference",
        "Remaining",
        "POL-1",
        "40.00",
        "60.00",
        "Acme",
        "1 partially cleared",
    ] {
        assert!(table.contains(needle), "missing {needle}: {table}");
    }

    let csv = l.co(&[
        "--format",
        "csv",
        "report",
        "reconciliation",
        "--account",
        "1200",
    ]);
    let pol1_row = csv
        .lines()
        .find(|line| line.contains("POL-1"))
        .unwrap_or_else(|| panic!("POL-1 row: {csv}"));
    assert!(
        pol1_row.starts_with("entry,1,1,2026-01-05,POL-1,Commission POL-1,10000,4000,6000,USD,,"),
        "{pol1_row}"
    );
    assert!(pol1_row.contains("carrier"), "{pol1_row}");
}

#[test]
fn metadata_filter_and_group() {
    let l = seeded();
    l.clear_ref("POL-3", &["--amount", "20", "--statement", "S1"]);

    let r = l.report(&["--filter-metadata", "carrier=Acme"]);
    assert_eq!(refs(&r), vec!["POL-1", "POL-3"]);
    assert_eq!(int(&r, "book_balance"), 15_000);
    assert_eq!(int(&r, "cleared_balance"), 2_000);
    assert_eq!(r["metadata_filters"][0]["key"], "carrier");

    // Filters are AND-ed.
    let r = l.report(&[
        "--filter-metadata",
        "carrier=Acme",
        "--filter-metadata",
        "policy=P1",
    ]);
    assert_eq!(refs(&r), vec!["POL-1"]);

    // No match at all.
    let r = l.report(&["--filter-metadata", "carrier=Nope"]);
    assert_eq!(int(&r, "uncleared_count"), 0);
    assert_eq!(int(&r, "book_balance"), 0);

    // Malformed filter.
    let out = l.co_run(
        &[
            "report",
            "reconciliation",
            "--account",
            "1200",
            "--filter-metadata",
            "carrier",
        ],
        None,
    );
    assert_eq!(out.code, Some(2));

    let r = l.report(&["--group-by-metadata", "carrier"]);
    assert_eq!(r["group_by_metadata"], "carrier");
    let groups = r["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups: {r}"))
        .clone();
    let summary: Vec<(String, i64, i64)> = groups
        .iter()
        .map(|g| {
            (
                g["value"].as_str().unwrap_or("").to_string(),
                int(g, "count"),
                int(g, "uncleared_total"),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("Acme".to_string(), 2, 13_000),
            ("Beta".to_string(), 1, 20_000),
            ("(unassigned)".to_string(), 1, 8_000),
        ]
    );

    let table = l.co(&[
        "report",
        "reconciliation",
        "--account",
        "1200",
        "--group-by-metadata",
        "carrier",
    ]);
    assert!(table.contains("carrier = Acme (2 items"), "{table}");
    assert!(table.contains("carrier = (unassigned) (1 items"), "{table}");

    let csv = l.co(&[
        "--format",
        "csv",
        "report",
        "reconciliation",
        "--account",
        "1200",
        "--group-by-metadata",
        "carrier",
    ]);
    assert!(csv.contains("group,,,,,Acme,,,13000,USD,Acme,"), "{csv}");
    assert!(
        csv.contains("group,,,,,(unassigned),,,8000,USD,(unassigned),"),
        "{csv}"
    );

    // Ungrouped: groups is null.
    let r = l.report(&[]);
    assert!(r["groups"].is_null());
}

#[test]
fn clearings_are_audited() {
    let l = seeded();
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    // An idempotent re-run records nothing.
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    l.clear_ref("POL-1", &["--date", "2026-02-28", "--statement", "S-FEB"]);

    let rows = l.json(
        &[
            "audit",
            "log",
            "--entity",
            "entry",
            "--action",
            "status-change",
        ],
        None,
    );
    let rows = rows
        .as_array()
        .unwrap_or_else(|| panic!("audit rows: {rows}"))
        .clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // Most recent first.
    let parse = |v: &Value| -> Value {
        serde_json::from_str(v.as_str().unwrap_or("null")).unwrap_or(Value::Null)
    };
    let (before, after) = (parse(&rows[0]["before"]), parse(&rows[0]["after"]));
    assert_eq!(before["status"], "uncleared");
    assert_eq!(before["cleared_amount"], 4_000);
    assert_eq!(after["status"], "cleared");
    assert_eq!(after["cleared_amount"], 10_000);
    assert_eq!(after["op"], "clear");
    assert_eq!(after["clearings"][0]["statement"], "S-FEB");
    assert_eq!(after["clearings"][0]["cleared_date"], "2026-02-28");
    assert_eq!(after["clearings"][0]["amount"], 6_000);
    let first = parse(&rows[1]["after"]);
    assert_eq!(first["status"], "uncleared");
    assert_eq!(first["clearings"][0]["statement"], "S-JAN");
}

// ---------------------------------------------------------------------------
// Reconciled dates, date validation, locked clearings, `txn show` clearings
// ---------------------------------------------------------------------------

/// Clearings of a listed report item.
fn item_clearings(item: &Value) -> Vec<Value> {
    item["clearings"]
        .as_array()
        .unwrap_or_else(|| panic!("clearings is an array: {item}"))
        .clone()
}

#[test]
fn reconciled_date_is_point_in_time() {
    let l = seeded();
    let jan = ["--date", "2026-01-31", "--statement", "S-JAN"];
    // Cleared in January: POL-1 in full, POL-2 partially.
    l.clear_ref("POL-1", &jan);
    l.clear_ref("POL-2", &[&jan[..], &["--amount", "50"]].concat());
    // Reconciled in February.
    let d = l.clear_ref(
        "POL-1",
        &[
            "--date",
            "2026-02-10",
            "--statement",
            "S-JAN",
            "--status",
            "reconciled",
        ],
    );
    assert_eq!(d["action"], "reconciled");
    assert_eq!(d["reconciled_date"], "2026-02-10");
    l.clear_ref(
        "POL-2",
        &[
            "--date",
            "2026-02-10",
            "--statement",
            "S-JAN",
            "--status",
            "reconciled",
        ],
    );

    // As of January: cleared but not yet reconciled.
    for extra in [&[][..], &["--statement", "S-JAN"][..]] {
        let r = l.report(&[&["--to", "2026-01-31"][..], extra].concat());
        assert_eq!(int(&r, "cleared_balance"), 15_000, "{r}");
        assert_eq!(int(&r, "reconciled_balance"), 0, "{r}");
        let list = items(&r);
        let pol2 = item(&list, "POL-2").unwrap_or_else(|| panic!("POL-2 listed: {r}"));
        assert_eq!(int(pol2, "reconciled_amount"), 0);
        let cs = item_clearings(pol2);
        assert_eq!(cs.len(), 1, "{pol2}");
        assert_eq!(cs[0]["status"], "cleared");
        assert_eq!(cs[0]["cleared_date"], "2026-01-31");
        assert!(cs[0]["reconciled_date"].is_null(), "{pol2}");
        assert_eq!(cs[0]["statement"], "S-JAN");
    }

    // As of February: reconciled.
    for extra in [&[][..], &["--statement", "S-JAN"][..]] {
        let r = l.report(&[&["--to", "2026-02-28"][..], extra].concat());
        assert_eq!(int(&r, "cleared_balance"), 15_000, "{r}");
        assert_eq!(int(&r, "reconciled_balance"), 15_000, "{r}");
        let list = items(&r);
        let pol2 = item(&list, "POL-2").unwrap_or_else(|| panic!("POL-2 listed: {r}"));
        assert_eq!(int(pol2, "reconciled_amount"), 5_000);
        let cs = item_clearings(pol2);
        assert_eq!(cs[0]["status"], "reconciled");
        assert_eq!(cs[0]["reconciled_date"], "2026-02-10");
    }

    // A clearing created directly as reconciled gets both dates.
    let d = l.clear_ref("POL-3", &["--date", "2026-02-15", "--status", "reconciled"]);
    assert_eq!(d["entry_status"], "reconciled");
    assert_eq!(d["cleared_date"], "2026-02-15");
    assert_eq!(d["reconciled_date"], "2026-02-15");
    let r = l.report(&["--to", "2026-02-14"]);
    assert_eq!(int(&r, "reconciled_balance"), 15_000);
    let r = l.report(&["--to", "2026-02-15"]);
    assert_eq!(int(&r, "reconciled_balance"), 20_000);

    // The audit payload carries the reconciled date.
    let rows = l.json(
        &[
            "audit",
            "log",
            "--entity",
            "entry",
            "--action",
            "status-change",
        ],
        None,
    );
    let latest: Value =
        serde_json::from_str(rows[0]["after"].as_str().unwrap_or("null")).unwrap_or(Value::Null);
    assert_eq!(latest["clearings"][0]["cleared_date"], "2026-02-15");
    assert_eq!(latest["clearings"][0]["reconciled_date"], "2026-02-15");
}

#[test]
fn clearing_dated_before_transaction_is_rejected() {
    let l = seeded();
    // POL-1 is dated 2026-01-05.
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--date",
            "2026-01-04",
        ],
        None,
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("before the transaction date 2026-01-05"),
        "{}",
        out.stderr
    );
    // Also for a clearing created as reconciled.
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "1",
            "--entry",
            "1",
            "--date",
            "2026-01-01",
            "--status",
            "reconciled",
        ],
        None,
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);

    // Batch: POL-2 (2026-01-10) fails on line 2, and POL-1 is not applied.
    let out = l.batch("POL-1\nPOL-2\n", &["--date", "2026-01-08"]);
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("line 2 (POL-2)"), "{}", out.stderr);
    assert!(
        out.stderr.contains("before the transaction date"),
        "{}",
        out.stderr
    );
    let r = l.report(&[]);
    assert_eq!(int(&r, "cleared_balance"), 0);
    assert_eq!(int(&r, "uncleared_count"), 4);
}

#[test]
fn reconciled_date_before_cleared_date_is_rejected() {
    let l = seeded();
    l.clear_ref("POL-1", &["--date", "2026-01-31", "--statement", "S-JAN"]);
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--date",
            "2026-01-20",
            "--statement",
            "S-JAN",
            "--status",
            "reconciled",
        ],
        None,
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(
        out.stderr.contains("before the cleared date 2026-01-31"),
        "{}",
        out.stderr
    );
    // Same for reconciling in full (no statement).
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "1",
            "--entry",
            "1",
            "--date",
            "2026-01-30",
            "--status",
            "reconciled",
        ],
        None,
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    let r = l.report(&[]);
    assert_eq!(int(&r, "reconciled_balance"), 0);
}

#[test]
fn txn_show_lists_clearings() {
    let l = seeded();
    // POL-1 is transaction 1; entry 1 is its 1200 debit, entry 2 the credit.
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "40",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
    );
    l.clear_ref(
        "POL-1",
        &[
            "--date",
            "2026-02-10",
            "--statement",
            "S-JAN",
            "--status",
            "reconciled",
        ],
    );
    l.clear_ref(
        "POL-1",
        &[
            "--amount",
            "25",
            "--date",
            "2026-02-28",
            "--statement",
            "S-FEB",
        ],
    );

    let d = l.json(&["txn", "show", "1"], None);
    let txn = &d[0];
    assert_eq!(txn["id"], 1);
    let e1 = &txn["entries"][0];
    // Existing fields are unchanged.
    assert_eq!(e1["account_code"], "1200");
    assert_eq!(e1["direction"], "debit");
    assert_eq!(int(e1, "amount"), 10_000);
    assert_eq!(e1["status"], "uncleared");
    // New fields.
    assert_eq!(int(e1, "id"), 1);
    assert_eq!(int(e1, "cleared_amount"), 6_500);
    assert_eq!(int(e1, "remaining_amount"), 3_500);
    let cs = e1["clearings"]
        .as_array()
        .unwrap_or_else(|| panic!("clearings: {e1}"));
    assert_eq!(cs.len(), 2, "{e1}");
    assert!(cs[0]["id"].is_i64());
    assert_eq!(int(&cs[0], "amount"), 4_000);
    assert_eq!(cs[0]["cleared_date"], "2026-01-31");
    assert_eq!(cs[0]["reconciled_date"], "2026-02-10");
    assert_eq!(cs[0]["statement"], "S-JAN");
    assert_eq!(cs[0]["status"], "reconciled");
    assert_eq!(int(&cs[1], "amount"), 2_500);
    assert_eq!(cs[1]["cleared_date"], "2026-02-28");
    assert!(cs[1]["reconciled_date"].is_null());
    assert_eq!(cs[1]["statement"], "S-FEB");
    assert_eq!(cs[1]["status"], "cleared");
    let e2 = &txn["entries"][1];
    assert_eq!(int(e2, "id"), 2);
    assert_eq!(int(e2, "cleared_amount"), 0);
    assert_eq!(int(e2, "remaining_amount"), 10_000);
    assert_eq!(e2["clearings"].as_array().map(Vec::len), Some(0));

    // `txn list` keeps its shape (no clearing breakdown).
    let list = l.json(&["txn", "list"], None);
    assert!(list[0]["entries"][0].get("clearings").is_none(), "{list}");

    let table = l.co(&["txn", "show", "1"]);
    for needle in [
        "entry #1: cleared 65.00 of 100.00, remaining 35.00",
        "40.00  reconciled  cleared 2026-01-31  reconciled 2026-02-10  statement S-JAN",
        "25.00  cleared  cleared 2026-02-28  statement S-FEB",
        "entry #2: cleared 0.00 of 100.00, remaining 100.00",
    ] {
        assert!(table.contains(needle), "missing {needle}: {table}");
    }
}

#[test]
fn cleared_status_on_reconciled_clearings_is_an_error() {
    let l = seeded();
    let jan = ["--date", "2026-01-31", "--statement", "S-JAN"];
    assert!(l.batch("POL-1\nPOL-2\n", &jan).ok);
    assert!(
        l.batch(
            "POL-1\nPOL-2\n",
            &[&jan[..], &["--status", "reconciled"]].concat()
        )
        .ok
    );

    // Single, by statement.
    let out = l.co_run(
        &[
            "txn",
            "clear",
            "--reference",
            "POL-1",
            "--account",
            "1200",
            "--date",
            "2026-01-31",
            "--statement",
            "S-JAN",
        ],
        None,
    );
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("locked"), "{}", out.stderr);
    assert!(
        out.stderr.contains("--status uncleared --statement S-JAN"),
        "{}",
        out.stderr
    );

    // Single, without a statement (the legacy form).
    let out = l.co_run(&["txn", "clear", "1", "--entry", "1"], None);
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("locked"), "{}", out.stderr);

    // Batch: line 2 fails, POL-3 on line 1 is not applied.
    let out = l.batch("POL-3\nPOL-1\n", &jan);
    assert_eq!(out.code, Some(3), "{}", out.stderr);
    assert!(out.stderr.contains("line 2 (POL-1)"), "{}", out.stderr);
    assert!(out.stderr.contains("locked"), "{}", out.stderr);
    let r = l.report(&[]);
    let list = items(&r);
    let pol3 = item(&list, "POL-3").unwrap_or_else(|| panic!("POL-3 listed: {r}"));
    assert_eq!(int(pol3, "cleared_amount"), 0);
    assert_eq!(int(&r, "reconciled_balance"), 30_000);

    // The documented way back: unclear, then clear again.
    let d = l.clear_ref("POL-1", &["--status", "uncleared", "--statement", "S-JAN"]);
    assert_eq!(d["action"], "uncleared");
    let d = l.clear_ref("POL-1", &jan);
    assert_eq!(d["action"], "cleared");
    assert_eq!(d["entry_status"], "cleared");
}

#[test]
fn rerunning_a_cleared_statement_is_still_a_skip() {
    let l = seeded();
    let jan = ["--date", "2026-01-31", "--statement", "S-JAN"];
    let feb = ["--date", "2026-02-28", "--statement", "S-FEB"];
    assert!(l.batch("POL-1,40\n", &jan).ok);
    assert!(l.batch("POL-1,60\nPOL-2\n", &feb).ok);
    // Reconcile January only; February stays cleared.
    assert!(
        l.batch("POL-1\n", &[&jan[..], &["--status", "reconciled"]].concat())
            .ok
    );

    let out = l.batch("POL-1,60\nPOL-2\n", &feb);
    assert!(out.ok, "{}", out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("json: {e}"));
    assert_eq!(int(&v["data"], "skipped_count"), 2);
    assert_eq!(
        v["data"]["skipped"][0]["skip_reason"],
        "already_cleared_by_statement"
    );
}
