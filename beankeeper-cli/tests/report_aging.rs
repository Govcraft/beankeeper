//! Integration tests for `bk report aging` (issue #26).
//!
//! Each test builds a fresh ledger in a temp directory, posts charges and
//! settlements to a receivable or payable account, and checks the FIFO aging
//! buckets, unapplied credits, grouping and output formats. Amounts in JSON
//! and CSV are minor units (cents).

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

const CO: &str = "acme";
const AS_OF: &str = "2026-06-30";

struct Ledger {
    _dir: TempDir,
    db: String,
}

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
}

impl Ledger {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let db = dir
            .path()
            .join("books.db")
            .to_str()
            .unwrap_or_else(|| panic!("utf-8 path"))
            .to_string();
        let l = Self { _dir: dir, db };
        l.ok(&["init"]);
        l.ok(&["company", "create", CO, "Acme Inc"]);
        for (code, name, ty) in [
            ("1000", "Cash", "asset"),
            ("1100", "Accounts Receivable", "asset"),
            ("2000", "Accounts Payable", "liability"),
            ("4000", "Revenue", "revenue"),
            ("5000", "Expenses", "expense"),
        ] {
            l.ok(&[
                "--company",
                CO,
                "account",
                "create",
                code,
                name,
                "--type",
                ty,
            ]);
        }
        l
    }

    fn run(&self, args: &[&str]) -> Out {
        let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
        cmd.arg("--db").arg(&self.db).arg("--quiet").args(args);
        let output = cmd.output().unwrap_or_else(|e| panic!("run bk: {e}"));
        Out {
            ok: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap_or_else(|e| panic!("utf-8: {e}")),
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.ok, "bk {args:?} failed (exit {:?})", out.code);
        out.stdout
    }

    /// Post a two-line transaction (amounts in major units).
    fn post(&self, date: &str, debit: &str, credit: &str, amount: &str, meta: Option<&str>) {
        let d = format!("{debit}:{amount}");
        let c = format!("{credit}:{amount}");
        let mut args = vec![
            "--company",
            CO,
            "txn",
            "post",
            "-d",
            "txn",
            "--date",
            date,
            "--debit",
            &d,
            "--credit",
            &c,
        ];
        if let Some(m) = meta {
            args.extend(["-m", m]);
        }
        self.ok(&args);
    }

    fn post_ccy(&self, date: &str, debit: &str, credit: &str, amount: &str, ccy: &str) {
        let d = format!("{debit}:{amount}");
        let c = format!("{credit}:{amount}");
        self.ok(&[
            "--company",
            CO,
            "txn",
            "post",
            "-d",
            "txn",
            "--date",
            date,
            "--debit",
            &d,
            "--credit",
            &c,
            "--currency",
            ccy,
        ]);
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut full = vec!["--json", "--company", CO];
        full.extend_from_slice(args);
        let out = self.ok(&full);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("invalid JSON ({e}): {out}"))
    }

    fn aging(&self, extra: &[&str]) -> Value {
        let mut args = vec!["report", "aging", "--as-of", AS_OF];
        args.extend_from_slice(extra);
        self.json(&args)
    }

    /// Balance in the account's normal direction as of `AS_OF`, via `report balance`.
    fn normal_balance(&self, account: &str, asset: bool) -> i64 {
        let v = self.json(&["report", "balance", "--account", account, "--to", AS_OF]);
        let dr = int(&v["data"]["debit_total"]);
        let cr = int(&v["data"]["credit_total"]);
        if asset { dr - cr } else { cr - dr }
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64()
        .unwrap_or_else(|| panic!("expected integer, got {v}"))
}

fn bucket_amounts(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap_or_else(|| panic!("expected bucket array, got {v}"))
        .iter()
        .map(|b| int(&b["amount"]))
        .collect()
}

fn assert_ties_to_balance(l: &Ledger, report: &Value, account: &str, asset: bool) {
    let totals = &report["data"]["totals"];
    let open = int(&totals["total_open"]);
    let unapplied = int(&totals["unapplied_credit"]);
    let balance = l.normal_balance(account, asset);
    assert_eq!(
        open - unapplied,
        balance,
        "grand total minus unapplied credit must equal the account balance"
    );
    assert_eq!(int(&report["data"]["account_balance"]), balance);
    assert_eq!(bucket_amounts(&totals["buckets"]).iter().sum::<i64>(), open);
}

#[test]
fn receivable_with_partial_payments() {
    let l = Ledger::new();
    l.post("2026-01-01", "1100", "4000", "400", None); // 180 days
    l.post("2026-04-01", "1100", "4000", "300", None); // 90 days
    l.post("2026-06-10", "1100", "4000", "200", None); // 20 days
    l.post("2026-06-30", "1100", "4000", "100", None); // current
    // Settles the 400 charge and 100 of the 300 charge (FIFO).
    l.post("2026-06-15", "1000", "1100", "500", None);
    // After the as-of date: ignored.
    l.post("2026-07-05", "1100", "4000", "999", None);

    let r = l.aging(&["--account", "1100"]);
    assert_eq!(r["data"]["kind"], "receivable");
    let totals = &r["data"]["totals"];
    assert_eq!(
        bucket_amounts(&totals["buckets"]),
        vec![10_000, 20_000, 0, 20_000, 0]
    );
    assert_eq!(int(&totals["total_open"]), 50_000);
    assert_eq!(int(&totals["unapplied_credit"]), 0);
    assert_ties_to_balance(&l, &r, "1100", true);

    let items = r["data"]["groups"][0]["open_items"]
        .as_array()
        .unwrap_or_else(|| panic!("open_items array"));
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["date"], "2026-04-01");
    assert_eq!(int(&items[0]["original_amount"]), 30_000);
    assert_eq!(int(&items[0]["open_amount"]), 20_000);
    assert_eq!(int(&items[0]["age_days"]), 90);
    assert_eq!(items[0]["bucket"], "61-90");
}

#[test]
fn payable_aging() {
    let l = Ledger::new();
    l.post("2026-02-15", "5000", "2000", "250", None); // 135 days -> 90+
    l.post("2026-05-20", "5000", "2000", "80", None); // 41 days -> 31-60
    l.post("2026-06-01", "2000", "1000", "50", None); // pays 50 of the oldest

    let r = l.aging(&["--account", "2000"]);
    assert_eq!(r["data"]["kind"], "payable");
    assert_eq!(
        bucket_amounts(&r["data"]["totals"]["buckets"]),
        vec![0, 0, 8_000, 0, 20_000]
    );
    assert_ties_to_balance(&l, &r, "2000", false);
}

#[test]
fn overpayment_becomes_unapplied_credit() {
    let l = Ledger::new();
    l.post("2026-06-01", "1100", "4000", "100", None);
    l.post("2026-06-05", "1000", "1100", "150", None);

    let r = l.aging(&["--account", "1100"]);
    let totals = &r["data"]["totals"];
    assert_eq!(int(&totals["total_open"]), 0);
    assert_eq!(int(&totals["unapplied_credit"]), 5_000);
    assert_eq!(int(&totals["net"]), -5_000);
    assert_ties_to_balance(&l, &r, "1100", true);

    let table = l.ok(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1100",
        "--as-of",
        AS_OF,
    ]);
    assert!(table.contains("Unapplied Credit"), "{table}");
    assert!(table.contains("50.00"), "{table}");
    assert!(table.contains("FIFO"), "{table}");
}

#[test]
fn custom_buckets() {
    let l = Ledger::new();
    l.post("2026-06-20", "1100", "4000", "10", None); // 10 days
    l.post("2026-06-01", "1100", "4000", "20", None); // 29 days
    l.post("2026-05-01", "1100", "4000", "30", None); // 60 days

    let r = l.aging(&["--account", "1100", "--buckets", "15,45"]);
    let labels: Vec<&str> = r["data"]["buckets"]
        .as_array()
        .unwrap_or_else(|| panic!("buckets array"))
        .iter()
        .map(|b| b["label"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(labels, ["Current", "1-15", "16-45", "45+"]);
    assert_eq!(
        bucket_amounts(&r["data"]["totals"]["buckets"]),
        vec![0, 1_000, 2_000, 3_000]
    );

    for bad in ["45,15", "0,30", "x"] {
        let out = l.run(&[
            "--company",
            CO,
            "report",
            "aging",
            "--account",
            "1100",
            "--buckets",
            bad,
        ]);
        assert_eq!(
            out.code,
            Some(3),
            "--buckets {bad} should be a validation error"
        );
    }
}

#[test]
fn group_by_metadata_including_non_json() {
    let l = Ledger::new();
    l.post(
        "2026-03-01",
        "1100",
        "4000",
        "100",
        Some(r#"{"customer":"Globex"}"#),
    );
    l.post(
        "2026-06-01",
        "1100",
        "4000",
        "200",
        Some(r#"{"customer":"Initech"}"#),
    );
    l.post(
        "2026-06-20",
        "1100",
        "4000",
        "40",
        Some(r#"{"customer":7}"#),
    );
    l.post("2026-06-25", "1100", "4000", "70", Some("INV-9 plain text"));
    l.post("2026-06-26", "1100", "4000", "30", None);
    // Initech pays; grouped FIFO applies it to Initech, not Globex's older charge.
    l.post(
        "2026-06-10",
        "1000",
        "1100",
        "200",
        Some(r#"{"customer":"Initech"}"#),
    );

    let r = l.aging(&["--account", "1100", "--group-by-metadata", "customer"]);
    assert_eq!(r["data"]["group_by"], "customer");
    let groups = r["data"]["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups array"));
    let find = |name: &str| {
        groups
            .iter()
            .find(|g| g["counterparty"] == name)
            .unwrap_or_else(|| panic!("group {name} missing: {groups:?}"))
    };
    assert_eq!(int(&find("Globex")["total_open"]), 10_000);
    assert_eq!(int(&find("Initech")["total_open"]), 0);
    assert_eq!(int(&find("7")["total_open"]), 4_000);
    assert_eq!(int(&find("(none)")["total_open"]), 10_000);
    assert_eq!(groups.len(), 4);
    assert_ties_to_balance(&l, &r, "1100", true);

    let csv = l.ok(&[
        "--format",
        "csv",
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1100",
        "--as-of",
        AS_OF,
        "--group-by-metadata",
        "customer",
    ]);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "counterparty,Current,1-30,31-60,61-90,90+,total_open,unapplied_credit"
    );
    assert!(lines.iter().any(|l| l.starts_with("(none),")), "{csv}");
    assert_eq!(
        lines.last().copied(),
        Some("TOTAL,0,14000,0,0,10000,24000,0")
    );
}

#[test]
fn json_shape() {
    let l = Ledger::new();
    l.post("2026-06-01", "1100", "4000", "100", None);

    let r = l.aging(&["--account", "1100"]);
    assert_eq!(r["ok"], true);
    assert_eq!(r["meta"]["command"], "report.aging");
    assert_eq!(r["meta"]["company"], CO);

    let data = &r["data"];
    assert_eq!(data["account"]["code"], "1100");
    assert_eq!(data["account"]["type"], "asset");
    assert_eq!(data["as_of"], AS_OF);
    assert_eq!(data["currency"], "USD");
    assert!(data["group_by"].is_null());
    assert_eq!(data["basis"], "fifo-by-transaction-date");

    let buckets = data["buckets"]
        .as_array()
        .unwrap_or_else(|| panic!("buckets"));
    assert_eq!(buckets.len(), 5);
    assert_eq!(buckets[0]["label"], "Current");
    assert_eq!(int(&buckets[1]["min_days"]), 1);
    assert_eq!(int(&buckets[1]["max_days"]), 30);
    assert!(buckets[4]["max_days"].is_null());

    let groups = data["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups"));
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert!(g["counterparty"].is_null());
    for key in [
        "buckets",
        "total_open",
        "unapplied_credit",
        "net",
        "open_items",
    ] {
        assert!(!g[key].is_null(), "group is missing {key}");
    }
    let item = &g["open_items"][0];
    for key in [
        "transaction_id",
        "date",
        "description",
        "age_days",
        "bucket",
        "original_amount",
        "open_amount",
    ] {
        assert!(!item[key].is_null(), "open item is missing {key}");
    }
    assert_eq!(item["bucket"], "1-30");
    for key in ["buckets", "total_open", "unapplied_credit", "net"] {
        assert!(!data["totals"][key].is_null(), "totals missing {key}");
    }
}

#[test]
fn rejects_non_receivable_payable_accounts() {
    let l = Ledger::new();
    let out = l.run(&["--company", CO, "report", "aging", "--account", "4000"]);
    assert_eq!(out.code, Some(3));
    let out = l.run(&["--company", CO, "report", "aging", "--account", "9999"]);
    assert_eq!(out.code, Some(5));
}

#[test]
fn currency_resolution() {
    let l = Ledger::new();
    l.post_ccy("2026-06-01", "1100", "4000", "100", "USD");
    l.post_ccy("2026-06-02", "1100", "4000", "70", "EUR");

    // Two currencies and no --currency: validation error.
    let out = l.run(&["--company", CO, "report", "aging", "--account", "1100"]);
    assert_eq!(out.code, Some(3));

    let r = l.aging(&["--account", "1100", "--currency", "EUR"]);
    assert_eq!(r["data"]["currency"], "EUR");
    assert_eq!(int(&r["data"]["totals"]["total_open"]), 7_000);
}
