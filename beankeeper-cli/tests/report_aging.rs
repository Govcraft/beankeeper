//! Integration tests for `bk report aging` (issue #26).
//!
//! Each test builds a fresh ledger in a temp directory, posts charges and
//! settlements to a receivable or payable account, and checks the FIFO and
//! specific-identification aging buckets, unapplied credits and net credits,
//! unmatched links, grouping, `--detail`, as-of reruns and output formats.
//! Amounts in JSON and CSV are minor units (cents).

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
    stderr: String,
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
        let mut full = vec!["--quiet"];
        full.extend_from_slice(args);
        self.run_loud(&full)
    }

    /// Run without forcing `--quiet` (so warnings reach stderr).
    fn run_loud(&self, args: &[&str]) -> Out {
        let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
        cmd.arg("--db").arg(&self.db).args(args);
        let output = cmd.output().unwrap_or_else(|e| panic!("run bk: {e}"));
        Out {
            ok: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap_or_else(|e| panic!("utf-8: {e}")),
            stderr: String::from_utf8(output.stderr).unwrap_or_else(|e| panic!("utf-8: {e}")),
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

    /// Post a two-line transaction with a reference (also used as the
    /// description) and optional metadata.
    #[allow(clippy::too_many_arguments)] // mirrors `post` plus a reference
    fn post_ref(
        &self,
        date: &str,
        debit: &str,
        credit: &str,
        amount: &str,
        reference: &str,
        meta: Option<&str>,
    ) {
        let d = format!("{debit}:{amount}");
        let c = format!("{credit}:{amount}");
        let mut args = vec![
            "--company",
            CO,
            "txn",
            "post",
            "-d",
            reference,
            "-r",
            reference,
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

    /// Table or CSV output of `report aging` as of `AS_OF`.
    fn aging_text(&self, format: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "--format",
            format,
            "--company",
            CO,
            "report",
            "aging",
            "--as-of",
            AS_OF,
        ];
        args.extend_from_slice(extra);
        self.ok(&args)
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

/// `(reference or transaction id, open_amount)` of a group's open items.
fn open_items(group: &Value) -> Vec<(String, i64)> {
    group["open_items"]
        .as_array()
        .unwrap_or_else(|| panic!("open_items array: {group}"))
        .iter()
        .map(|i| {
            let key = i["reference"]
                .as_str()
                .map_or_else(|| i["transaction_id"].to_string(), str::to_string);
            (key, int(&i["open_amount"]))
        })
        .collect()
}

/// `(link as JSON text, reason)` of each unmatched link.
fn unmatched_reasons(report: &Value) -> Vec<(String, String)> {
    report["data"]["unmatched_links"]
        .as_array()
        .unwrap_or_else(|| panic!("unmatched_links array"))
        .iter()
        .map(|u| {
            (
                u["link"].to_string(),
                u["reason"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
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
    assert_eq!(int(&find("(unassigned)")["total_open"]), 10_000);
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
        "row_type,account,counterparty,Current,1-30,31-60,61-90,90+,total_open,\
         unapplied_credit,net"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("counterparty,1100,(unassigned),")),
        "{csv}"
    );
    // A single grouped account has no subtotal row: the total is the subtotal.
    assert!(!lines.iter().any(|l| l.starts_with("account,")), "{csv}");
    assert_eq!(
        lines.last().copied(),
        Some("total,,,0,14000,0,0,10000,24000,0,24000")
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
    let accounts = data["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("accounts"));
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["code"], "1100");
    assert_eq!(accounts[0]["type"], "asset");
    assert_eq!(accounts[0]["totals"], data["totals"]);
    assert_eq!(accounts[0]["groups"], data["groups"]);
    assert_eq!(accounts[0]["account_balance"], data["account_balance"]);
    assert_eq!(data["include_children"], false);
    assert_eq!(data["group_across_accounts"], false);
    assert_eq!(data["metadata_filters"], serde_json::json!([]));
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
    assert_eq!(item["account"], "1100");
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

// ---------------------------------------------------------------------------
// Issue #26 follow-ups: specific identification, credits and reversals, net
// credits, --detail, as-of reruns, payables.
// ---------------------------------------------------------------------------

const GLOBEX: &str = r#"{"customer":"Globex"}"#;
const INITECH: &str = r#"{"customer":"Initech"}"#;

/// `report aging` JSON as of an explicit date.
fn aging_at(l: &Ledger, as_of: &str, extra: &[&str]) -> Value {
    let mut args = vec!["report", "aging", "--as-of", as_of];
    args.extend_from_slice(extra);
    l.json(&args)
}

fn items(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
    pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
}

#[test]
fn specific_short_payment_on_recent_item() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", None); // 180 days
    l.post_ref("2026-06-01", "1100", "4000", "300", "INV-102", None); // 29 days
    // A short payment on the recent invoice.
    l.post_ref(
        "2026-06-15",
        "1000",
        "1100",
        "100",
        "PAY-1",
        Some(r#"{"applies_to":"INV-102"}"#),
    );

    // FIFO ages the oldest invoice down instead.
    let fifo = l.aging(&["--account", "1100"]);
    assert_eq!(fifo["data"]["method"], "fifo");
    assert!(fifo["data"]["link_key"].is_null());
    assert_eq!(
        open_items(&fifo["data"]["groups"][0]),
        items(&[("INV-101", 30_000), ("INV-102", 30_000)])
    );
    assert_eq!(
        bucket_amounts(&fifo["data"]["totals"]["buckets"]),
        vec![0, 30_000, 0, 0, 30_000]
    );

    let r = l.aging(&["--account", "1100", "--method", "specific"]);
    assert_eq!(r["data"]["method"], "specific");
    assert_eq!(r["data"]["link_key"], "applies_to");
    assert_eq!(r["data"]["basis"], "specific-then-fifo-by-transaction-date");
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 40_000), ("INV-102", 20_000)])
    );
    assert_eq!(
        bucket_amounts(&r["data"]["totals"]["buckets"]),
        vec![0, 20_000, 0, 0, 40_000]
    );
    assert_eq!(unmatched_reasons(&r), Vec::<(String, String)>::new());
    assert_ties_to_balance(&l, &r, "1100", true);
}

#[test]
fn specific_link_by_transaction_id_and_custom_link_key() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", None);
    l.post_ref("2026-06-01", "1100", "4000", "300", "INV-102", None);
    let before = l.aging(&["--account", "1100"]);
    let id = int(&before["data"]["groups"][0]["open_items"][1]["transaction_id"]);
    let meta = format!(r#"{{"invoice":{id}}}"#);
    l.post_ref("2026-06-15", "1000", "1100", "100", "PAY-1", Some(&meta));

    let r = l.aging(&[
        "--account",
        "1100",
        "--method",
        "specific",
        "--link-key",
        "invoice",
    ]);
    assert_eq!(r["data"]["link_key"], "invoice");
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 40_000), ("INV-102", 20_000)])
    );

    // The default key does not see the `invoice` link: plain FIFO.
    let r = l.aging(&["--account", "1100", "--method", "specific"]);
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 30_000), ("INV-102", 30_000)])
    );

    // --link-key needs --method specific.
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1100",
        "--link-key",
        "invoice",
    ]);
    assert_eq!(out.code, Some(3));
}

#[test]
fn specific_excess_and_unlinked_settlements_fall_back_to_fifo() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", None);
    l.post_ref("2026-06-01", "1100", "4000", "300", "INV-102", None);
    // 300 settles INV-102; the 50 excess goes FIFO to INV-101.
    l.post_ref(
        "2026-06-10",
        "1000",
        "1100",
        "350",
        "PAY-1",
        Some(r#"{"applies_to":"INV-102"}"#),
    );
    // No link: FIFO.
    l.post_ref("2026-06-20", "1000", "1100", "100", "PAY-2", None);

    let r = l.aging(&["--account", "1100", "--method", "specific"]);
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 25_000)])
    );
    assert_eq!(unmatched_reasons(&r), Vec::<(String, String)>::new());
    assert_ties_to_balance(&l, &r, "1100", true);
}

#[test]
fn specific_invalid_links_warn_and_fall_back_to_fifo() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", Some(GLOBEX));
    l.post_ref(
        "2026-06-01",
        "1100",
        "4000",
        "300",
        "INV-201",
        Some(INITECH),
    );
    l.post_ref("2026-05-01", "5000", "1000", "10", "EXP-1", None); // not on AR
    l.post_ref("2026-07-05", "1100", "4000", "50", "INV-LATE", Some(GLOBEX));
    for (date, reference, link) in [
        ("2026-06-10", "PAY-1", r#""INV-999""#),
        ("2026-06-11", "PAY-2", r#""EXP-1""#),
        ("2026-06-12", "PAY-3", r#""INV-201""#),
        ("2026-06-13", "PAY-4", r#""INV-LATE""#),
        ("2026-06-14", "PAY-5", "true"),
        ("2026-06-15", "PAY-6", r#""PAY-1""#),
    ] {
        let meta = format!(r#"{{"customer":"Globex","applies_to":{link}}}"#);
        l.post_ref(date, "1000", "1100", "10", reference, Some(&meta));
    }

    let args = [
        "--account",
        "1100",
        "--method",
        "specific",
        "--group-by",
        "customer",
    ];
    let r = l.aging(&args);
    let expected: Vec<(String, String)> = [
        (r#""INV-999""#, "not_found"),
        (r#""EXP-1""#, "not_on_account"),
        (r#""INV-201""#, "other_counterparty"),
        (r#""INV-LATE""#, "after_as_of"),
        ("true", "invalid"),
        (r#""PAY-1""#, "not_a_charge"),
    ]
    .iter()
    .map(|&(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(unmatched_reasons(&r), expected);
    let first = &r["data"]["unmatched_links"][0];
    assert_eq!(first["counterparty"], "Globex");
    assert_eq!(first["fallback"], "fifo");
    assert_eq!(first["settlement_date"], "2026-06-10");
    assert_eq!(int(&first["settlement_amount"]), 1_000);

    // All six payments were applied FIFO within Globex.
    let groups = r["data"]["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups"));
    assert_eq!(open_items(&groups[0]), items(&[("INV-101", 34_000)]));
    assert_eq!(open_items(&groups[1]), items(&[("INV-201", 30_000)]));
    assert_ties_to_balance(&l, &r, "1100", true);

    // Table output lists them, and each one is a warning on stderr.
    let mut loud = vec!["--company", CO, "report", "aging", "--as-of", AS_OF];
    loud.extend_from_slice(&args);
    let out = l.run_loud(&loud);
    assert!(out.ok);
    assert!(out.stdout.contains("Unmatched links"), "{}", out.stdout);
    assert_eq!(out.stderr.matches("[warn]").count(), 6, "{}", out.stderr);
    assert!(
        out.stderr.contains(r#"applies_to="INV-999""#),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("applied FIFO"), "{}", out.stderr);
}

#[test]
fn reversal_linked_to_a_charge_cancels_it() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", None);
    l.post_ref("2026-06-01", "1100", "4000", "300", "INV-102", None);
    // Credit note reversing INV-102 (debit revenue, credit receivable).
    l.post_ref(
        "2026-06-05",
        "4000",
        "1100",
        "300",
        "CN-1",
        Some(r#"{"applies_to":"INV-102"}"#),
    );

    let r = l.aging(&["--account", "1100", "--method", "specific"]);
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 40_000)])
    );
    assert_ties_to_balance(&l, &r, "1100", true);

    // FIFO treats the credit note like a payment on the oldest invoice.
    let r = l.aging(&["--account", "1100"]);
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("INV-101", 10_000), ("INV-102", 30_000)])
    );
}

#[test]
fn refund_of_overpayment_clears_unapplied_credit() {
    let l = Ledger::new();
    l.post_ref("2026-06-01", "1100", "4000", "100", "INV-1", None);
    l.post_ref("2026-06-05", "1000", "1100", "150", "PAY-1", None);

    let r = aging_at(&l, "2026-06-07", &["--account", "1100"]);
    let totals = &r["data"]["totals"];
    assert_eq!(int(&totals["unapplied_credit"]), 5_000);
    assert_eq!(int(&totals["net"]), -5_000);
    assert_eq!(totals["net_credit"], true);

    // Refund paid out to the customer: a debit to AR, i.e. a charge that
    // consumes the unapplied credit.
    l.post_ref("2026-06-10", "1100", "1000", "50", "REFUND-1", None);
    let r = l.aging(&["--account", "1100"]);
    let totals = &r["data"]["totals"];
    assert_eq!(int(&totals["total_open"]), 0);
    assert_eq!(int(&totals["unapplied_credit"]), 0);
    assert_eq!(int(&totals["net"]), 0);
    assert_eq!(totals["net_credit"], false);
    assert_ties_to_balance(&l, &r, "1100", true);
}

#[test]
fn net_credit_account_is_obvious_in_all_formats() {
    let l = Ledger::new();
    l.post("2026-06-01", "1100", "4000", "100", Some(GLOBEX));
    l.post("2026-06-05", "1000", "1100", "250", Some(GLOBEX));
    l.post("2026-06-20", "1100", "4000", "70", Some(INITECH));
    let args = ["--account", "1100", "--group-by-metadata", "customer"];

    let r = l.aging(&args);
    let globex = &r["data"]["groups"][0];
    assert_eq!(globex["counterparty"], "Globex");
    assert_eq!(int(&globex["total_open"]), 0);
    assert_eq!(int(&globex["unapplied_credit"]), 15_000);
    assert_eq!(int(&globex["net"]), -15_000);
    assert_eq!(globex["net_credit"], true);
    assert_eq!(r["data"]["groups"][1]["net_credit"], false);
    let totals = &r["data"]["totals"];
    assert_eq!(int(&totals["net"]), -8_000);
    assert_eq!(totals["net_credit"], true);
    assert_eq!(int(&r["data"]["account_balance"]), -8_000);
    assert_ties_to_balance(&l, &r, "1100", true);

    let csv = l.aging_text("csv", &args);
    let lines: Vec<&str> = csv.lines().collect();
    assert!(
        lines.contains(&"counterparty,1100,Globex,0,0,0,0,0,0,15000,-15000"),
        "{csv}"
    );
    assert_eq!(
        lines.last().copied(),
        Some("total,,,0,7000,0,0,0,7000,15000,-8000")
    );

    let table = l.aging_text("table", &args);
    assert!(table.contains("-150.00 CR"), "{table}");
    assert!(table.contains("-80.00 CR"), "{table}");
    assert!(table.contains("net credit"), "{table}");
}

#[test]
fn detail_lists_open_items_in_table_csv_and_json() {
    let l = Ledger::new();
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-101", Some(GLOBEX));
    l.post_ref("2026-06-01", "1100", "4000", "300", "INV-102", Some(GLOBEX));
    l.post("2026-06-20", "1100", "4000", "70", Some(INITECH)); // no reference
    l.post_ref("2026-06-15", "1000", "1100", "100", "PAY-1", Some(GLOBEX));
    let args = ["--account", "1100", "--group-by", "customer", "--detail"];

    // JSON: open items carry the reference (null when there is none).
    let r = l.aging(&args);
    let globex = &r["data"]["groups"][0];
    assert_eq!(globex["open_items"][0]["reference"], "INV-101");
    let initech = &r["data"]["groups"][1];
    assert!(initech["open_items"][0]["reference"].is_null());
    let id_101 = int(&globex["open_items"][0]["transaction_id"]);
    let id_ini = int(&initech["open_items"][0]["transaction_id"]);

    let table = l.aging_text("table", &args);
    for needle in [
        "Open items -- Globex",
        "Open items -- Initech",
        "Reference",
        "INV-101",
        "INV-102",
        "400.00",
        "300.00",
        "180",
    ] {
        assert!(table.contains(needle), "missing {needle}: {table}");
    }
    let summary = l.aging_text("table", &args[..4]);
    assert!(!summary.contains("Open items"), "{summary}");

    let csv = l.aging_text("csv", &args);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "row_type,account,counterparty,transaction_id,date,reference,description,age_days,bucket,\
         original_amount,open_amount,Current,1-30,31-60,61-90,90+,total_open,\
         unapplied_credit,net"
    );
    let item_101 = format!(
        "item,1100,Globex,{id_101},2026-01-01,INV-101,INV-101,180,90+,40000,30000,,,,,30000,30000,,"
    );
    let item_ini =
        format!("item,1100,Initech,{id_ini},2026-06-20,,txn,10,1-30,7000,7000,,7000,,,,7000,,");
    assert_eq!(lines[1], item_101);
    assert!(lines[2].starts_with("item,1100,Globex,"), "{csv}");
    assert_eq!(
        lines[3],
        "counterparty,1100,Globex,,,,,,,,,0,30000,0,0,30000,60000,0,60000"
    );
    assert_eq!(lines[4], item_ini);
    assert_eq!(
        lines[5],
        "counterparty,1100,Initech,,,,,,,,,0,7000,0,0,0,7000,0,7000"
    );
    assert_eq!(lines[6], "total,,,,,,,,,,,0,37000,0,0,30000,67000,0,67000");
    assert_eq!(lines.len(), 7);
}

#[test]
fn as_of_rerun_ignores_later_payments() {
    let l = Ledger::new();
    l.post_ref("2026-01-15", "1100", "4000", "400", "INV-101", None);
    l.post_ref("2026-03-10", "1100", "4000", "300", "INV-102", None);
    l.post_ref("2026-02-01", "1000", "1100", "100", "PAY-1", None);
    let period_end = "2026-03-31";
    let run = |method: &str| {
        aging_at(
            &l,
            period_end,
            &[
                "--account",
                "1100",
                "--method",
                method,
                "--buckets",
                "30,60",
            ],
        )
    };
    let fifo_before = run("fifo");
    let specific_before = run("specific");
    assert_eq!(int(&fifo_before["data"]["totals"]["total_open"]), 60_000);

    // Later activity: a linked payment, an unlinked payment and a new invoice.
    l.post_ref(
        "2026-04-05",
        "1000",
        "1100",
        "300",
        "PAY-2",
        Some(r#"{"applies_to":"INV-102"}"#),
    );
    l.post_ref("2026-05-01", "1000", "1100", "250", "PAY-3", None);
    l.post_ref("2026-04-10", "1100", "4000", "80", "INV-103", None);

    let strip = |mut v: Value| {
        v["meta"] = Value::Null;
        v
    };
    assert_eq!(strip(run("fifo")), strip(fifo_before));
    assert_eq!(strip(run("specific")), strip(specific_before));

    // The current view does reflect the later payments.
    let now = l.aging(&["--account", "1100"]);
    assert_eq!(int(&now["data"]["totals"]["total_open"]), 13_000);
}

#[test]
fn payable_specific_identification() {
    let l = Ledger::new();
    l.post_ref("2026-02-15", "5000", "2000", "250", "BILL-1", None); // 135 days
    l.post_ref("2026-05-20", "5000", "2000", "80", "BILL-2", None); // 41 days
    // Debit-side settlements: a payment of BILL-2 and a vendor credit on BILL-1.
    l.post_ref(
        "2026-06-01",
        "2000",
        "1000",
        "80",
        "PAY-1",
        Some(r#"{"applies_to":"BILL-2"}"#),
    );
    l.post_ref(
        "2026-06-02",
        "2000",
        "5000",
        "50",
        "VCN-1",
        Some(r#"{"applies_to":"BILL-1"}"#),
    );

    let r = l.aging(&["--account", "2000", "--method", "specific"]);
    assert_eq!(r["data"]["kind"], "payable");
    assert_eq!(
        open_items(&r["data"]["groups"][0]),
        items(&[("BILL-1", 20_000)])
    );
    assert_eq!(
        bucket_amounts(&r["data"]["totals"]["buckets"]),
        vec![0, 0, 0, 0, 20_000]
    );
    assert_eq!(unmatched_reasons(&r), Vec::<(String, String)>::new());
    assert_ties_to_balance(&l, &r, "2000", false);

    let fifo = l.aging(&["--account", "2000"]);
    assert_eq!(
        bucket_amounts(&fifo["data"]["totals"]["buckets"]),
        vec![0, 0, 8_000, 0, 12_000]
    );
}

// ---------------------------------------------------------------------------
// Several accounts (issue #29)
// ---------------------------------------------------------------------------

/// A ledger with receivable sub-ledgers 1110 (east) and 1120 (west) under the
/// 1100 control account, holding charges, payments (one linked across
/// accounts), an overpayment and a charge after `AS_OF`.
fn sub_ledgers() -> Ledger {
    let l = Ledger::new();
    for (code, name) in [("1110", "AR East"), ("1120", "AR West")] {
        l.ok(&[
            "--company",
            CO,
            "account",
            "create",
            code,
            name,
            "--type",
            "asset",
        ]);
    }
    l.post_ref("2026-01-10", "1100", "4000", "40", "INV-C1", Some(GLOBEX));
    l.post_ref("2026-02-01", "1110", "4000", "400", "INV-E1", Some(GLOBEX));
    l.post_ref("2026-05-20", "1110", "4000", "300", "INV-E2", Some(INITECH));
    l.post_ref("2026-03-15", "1120", "4000", "250", "INV-W1", Some(INITECH));
    l.post_ref("2026-06-25", "1120", "4000", "80", "INV-W2", Some(GLOBEX));
    // Partial FIFO payment on the east ledger.
    l.post("2026-06-01", "1000", "1110", "150", Some(GLOBEX));
    // A west payment linked to the west invoice.
    l.post(
        "2026-06-10",
        "1000",
        "1120",
        "100",
        Some(r#"{"customer":"Initech","applies_to":"INV-W1"}"#),
    );
    // A west payment linked to an *east* invoice: not honoured.
    l.post(
        "2026-06-12",
        "1000",
        "1120",
        "30",
        Some(r#"{"customer":"Globex","applies_to":"INV-E1"}"#),
    );
    // Overpayment on the control account.
    l.post("2026-06-15", "1000", "1100", "60", Some(GLOBEX));
    // After AS_OF: ignored.
    l.post("2026-07-05", "1000", "1110", "500", Some(GLOBEX));
    l
}

const SUB_LEDGERS: [&str; 3] = ["1100", "1110", "1120"];

fn account_args<'a>(codes: &[&'a str]) -> Vec<&'a str> {
    codes.iter().flat_map(|c| ["--account", c]).collect()
}

fn totals_tuple(v: &Value) -> (Vec<i64>, i64, i64, i64) {
    (
        bucket_amounts(&v["buckets"]),
        int(&v["total_open"]),
        int(&v["unapplied_credit"]),
        int(&v["net"]),
    )
}

/// Element-wise sum of single-account totals.
fn summed(single: &[(Vec<i64>, i64, i64, i64)]) -> (Vec<i64>, i64, i64, i64) {
    let n = single.first().map_or(0, |s| s.0.len());
    let mut buckets = vec![0; n];
    let (mut open, mut unapplied, mut net) = (0, 0, 0);
    for (b, o, u, t) in single {
        for (acc, v) in buckets.iter_mut().zip(b) {
            *acc += v;
        }
        open += o;
        unapplied += u;
        net += t;
    }
    (buckets, open, unapplied, net)
}

#[test]
fn multi_account_totals_equal_sum_of_single_runs() {
    let l = sub_ledgers();
    let variants: [(&str, &[&str]); 6] = [
        (AS_OF, &[]),
        (AS_OF, &["--method", "specific"]),
        (AS_OF, &["--group-by", "customer"]),
        (
            AS_OF,
            &["--group-by", "customer", "--method", "specific", "--detail"],
        ),
        ("2026-03-31", &[]),
        (
            "2026-03-31",
            &["--group-by", "customer", "--group-across-accounts"],
        ),
    ];
    for (as_of, extra) in variants {
        let mut args = account_args(&SUB_LEDGERS);
        args.extend_from_slice(extra);
        let combined = aging_at(&l, as_of, &args);
        let data = &combined["data"];

        let mut singles = Vec::new();
        let mut balance = 0;
        for (i, code) in SUB_LEDGERS.iter().enumerate() {
            let mut one = vec!["--account", code];
            one.extend_from_slice(extra);
            let single = aging_at(&l, as_of, &one);
            singles.push(totals_tuple(&single["data"]["totals"]));
            balance += int(&single["data"]["account_balance"]);
            // Each section equals the single-account run.
            let section = &data["accounts"][i];
            assert_eq!(section["code"], *code, "{extra:?}");
            assert_eq!(
                section["totals"], single["data"]["totals"],
                "{extra:?} {code}"
            );
            assert_eq!(
                section["groups"], single["data"]["groups"],
                "{extra:?} {code}"
            );
            assert_eq!(
                section["unmatched_links"].as_array().map(Vec::len),
                single["data"]["unmatched_links"].as_array().map(Vec::len),
                "{extra:?} {code}"
            );
        }
        assert_eq!(totals_tuple(&data["totals"]), summed(&singles), "{extra:?}");
        assert_eq!(int(&data["account_balance"]), balance, "{extra:?}");

        // The combined groups add up to the same totals.
        let groups = data["groups"]
            .as_array()
            .unwrap_or_else(|| panic!("groups"));
        let open: i64 = groups.iter().map(|g| int(&g["total_open"])).sum();
        assert_eq!(open, int(&data["totals"]["total_open"]), "{extra:?}");
    }
}

#[test]
fn multi_account_ties_to_balances_and_ignores_later_entries() {
    let l = sub_ledgers();
    let r = l.aging(&account_args(&SUB_LEDGERS));
    let balance: i64 = SUB_LEDGERS.iter().map(|c| l.normal_balance(c, true)).sum();
    let totals = &r["data"]["totals"];
    assert_eq!(
        int(&totals["total_open"]) - int(&totals["unapplied_credit"]),
        balance
    );
    assert_eq!(int(&r["data"]["account_balance"]), balance);
    // East: 400 + 300 - 150 (the 500 payment is after AS_OF).
    assert_eq!(
        int(&r["data"]["accounts"][1]["totals"]["total_open"]),
        55_000
    );
}

#[test]
fn include_children_rolls_up_by_code_prefix() {
    let l = sub_ledgers();
    let rollup = l.aging(&["--account", "1100", "--include-children"]);
    let codes: Vec<&str> = rollup["data"]["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("accounts"))
        .iter()
        .map(|a| a["code"].as_str().unwrap_or_default())
        .collect();
    // 1100 -> prefix "11": never 1000 (cash) or 2000.
    assert_eq!(codes, SUB_LEDGERS);
    assert_eq!(rollup["data"]["include_children"], true);
    let explicit = l.aging(&account_args(&SUB_LEDGERS));
    assert_eq!(rollup["data"]["totals"], explicit["data"]["totals"]);
    assert_eq!(rollup["data"]["groups"], explicit["data"]["groups"]);

    // A leaf with --include-children is just itself; duplicates collapse.
    let leaf = l.aging(&[
        "--account",
        "1110",
        "--account",
        "1110",
        "--include-children",
    ]);
    assert_eq!(leaf["data"]["accounts"].as_array().map(Vec::len), Some(1));
}

#[test]
fn mixing_receivables_and_payables_is_rejected() {
    let l = sub_ledgers();
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1110",
        "--account",
        "2000",
    ]);
    assert!(!out.ok);
    assert!(
        out.stderr
            .contains("cannot age asset account 1110 and liability account 2000"),
        "{}",
        out.stderr
    );
    // A non-AR/AP account is rejected even when other accounts are valid.
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1110",
        "--account",
        "4000",
    ]);
    assert!(!out.ok);
    assert!(out.stderr.contains("revenue account"), "{}", out.stderr);
    // An unknown account is not found.
    let out = l.run(&["--company", CO, "report", "aging", "--account", "1999"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("not found"), "{}", out.stderr);
}

#[test]
fn specific_links_resolve_within_one_account() {
    let l = sub_ledgers();
    let mut args = account_args(&SUB_LEDGERS);
    args.extend(["--method", "specific"]);
    let r = l.aging(&args);
    let links = r["data"]["unmatched_links"]
        .as_array()
        .unwrap_or_else(|| panic!("links"));
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0]["account"], "1120");
    assert_eq!(links[0]["link"], "INV-E1");
    assert_eq!(links[0]["reason"], "other_account");
    // The east invoice keeps its FIFO-settled amount: only the east payment
    // touched it.
    let east = &r["data"]["accounts"][1]["groups"][0]["open_items"];
    assert_eq!(
        open_items_of(east),
        items(&[("INV-E1", 25_000), ("INV-E2", 30_000)])
    );
    // The linked west payment settled its own invoice; the unhonoured one
    // went FIFO on the west ledger (oldest first: INV-W1).
    let west = &r["data"]["accounts"][2]["groups"][0]["open_items"];
    assert_eq!(
        open_items_of(west),
        items(&[("INV-W1", 12_000), ("INV-W2", 8_000)])
    );

    // Alone, the same link is "not on this account".
    let single = l.aging(&["--account", "1120", "--method", "specific"]);
    assert_eq!(
        single["data"]["unmatched_links"][0]["reason"],
        "not_on_account"
    );
}

fn open_items_of(items: &Value) -> Vec<(String, i64)> {
    open_items(&serde_json::json!({ "open_items": items }))
}

#[test]
fn grouping_within_and_across_accounts_in_csv() {
    let l = sub_ledgers();
    let mut args = account_args(&["1110", "1120"]);
    args.extend(["--group-by", "customer"]);
    let csv = l.aging_text("csv", &args);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines,
        [
            "row_type,account,counterparty,Current,1-30,31-60,61-90,90+,total_open,\
             unapplied_credit,net",
            "counterparty,1110,Globex,0,0,0,0,25000,25000,0,25000",
            "counterparty,1110,Initech,0,0,30000,0,0,30000,0,30000",
            "account,1110,,0,0,30000,0,25000,55000,0,55000",
            // The west Globex payment predates INV-W2: it was unapplied
            // credit until INV-W2 consumed it.
            "counterparty,1120,Globex,0,5000,0,0,0,5000,0,5000",
            "counterparty,1120,Initech,0,0,0,0,15000,15000,0,15000",
            "account,1120,,0,5000,0,0,15000,20000,0,20000",
            "total,,,0,5000,30000,0,40000,75000,0,75000",
        ],
        "{csv}"
    );

    args.push("--group-across-accounts");
    let csv = l.aging_text("csv", &args);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[1..],
        [
            "counterparty,,Globex,0,5000,0,0,25000,30000,0,30000",
            "counterparty,,Initech,0,0,30000,0,15000,45000,0,45000",
            "account,1110,,0,0,30000,0,25000,55000,0,55000",
            "account,1120,,0,5000,0,0,15000,20000,0,20000",
            "total,,,0,5000,30000,0,40000,75000,0,75000",
        ],
        "{csv}"
    );

    // --group-across-accounts needs --group-by.
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1110",
        "--group-across-accounts",
    ]);
    assert!(!out.ok);
}

#[test]
fn multi_account_detail_names_the_account() {
    let l = sub_ledgers();
    let mut args = account_args(&["1110", "1120"]);
    args.extend(["--group-by", "customer", "--detail"]);
    let table = l.aging_text("table", &args);
    for needle in [
        "Receivables Aging -- 2 accounts (1110, 1120)",
        "Open items -- 1110 / Globex",
        "Open items -- 1120 / Initech",
        "Subtotal",
        "Combined balance of 2 accounts",
    ] {
        assert!(table.contains(needle), "missing {needle}: {table}");
    }
    let csv = l.aging_text("csv", &args);
    assert!(
        csv.lines()
            .any(|l| l.starts_with("item,1120,Initech,") && l.contains(",INV-W1,")),
        "{csv}"
    );
    // JSON items name their account in both views.
    let r = l.aging(&args);
    assert_eq!(
        r["data"]["accounts"][1]["groups"][1]["open_items"][0]["account"],
        "1120"
    );
    let combined_initech = &r["data"]["groups"][1];
    assert_eq!(combined_initech["counterparty"], "Initech");
    let accounts: Vec<&str> = combined_initech["open_items"]
        .as_array()
        .unwrap_or_else(|| panic!("items"))
        .iter()
        .map(|i| i["account"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(accounts, ["1120", "1110"]);
}

// ---------------------------------------------------------------------------
// Metadata filter (issue #30)
// ---------------------------------------------------------------------------

#[test]
fn filter_shows_disputed_items_at_original_age_net_of_payments() {
    let l = Ledger::new();
    let disputed = r#"{"customer":"Globex","status":"disputed"}"#;
    l.post_ref("2026-01-01", "1100", "4000", "400", "INV-1", Some(disputed)); // 180 days
    l.post_ref("2026-05-15", "1100", "4000", "300", "INV-2", Some(GLOBEX)); // 46 days
    l.post_ref("2026-06-20", "1100", "4000", "200", "INV-3", Some(disputed)); // 10 days
    // Payments carry no status: they still settle the disputed charges.
    l.post(
        "2026-06-22",
        "1000",
        "1100",
        "150",
        Some(r#"{"customer":"Globex","applies_to":"INV-3"}"#),
    );
    l.post("2026-06-25", "1000", "1100", "50", Some(GLOBEX));

    let fifo = l.aging(&["--account", "1100", "--filter-metadata", "status=disputed"]);
    let data = &fifo["data"];
    assert_eq!(
        data["metadata_filters"],
        serde_json::json!([{"key": "status", "value": "disputed"}])
    );
    // FIFO: 200 paid against INV-1 (the oldest); INV-3 untouched.
    assert_eq!(
        open_items(&data["groups"][0]),
        items(&[("INV-1", 20_000), ("INV-3", 20_000)])
    );
    let item = &data["groups"][0]["open_items"][0];
    assert_eq!(int(&item["age_days"]), 180);
    assert_eq!(item["bucket"], "90+");
    assert_eq!(int(&data["totals"]["total_open"]), 40_000);
    assert_eq!(int(&data["totals"]["unapplied_credit"]), 0);
    // The account balance is unfiltered.
    assert_eq!(int(&data["account_balance"]), 70_000);

    // Specific: the linked payment settles INV-3, the rest goes FIFO to INV-1.
    let specific = l.aging(&[
        "--account",
        "1100",
        "--method",
        "specific",
        "--filter-metadata",
        "status=disputed",
    ]);
    assert_eq!(
        open_items(&specific["data"]["groups"][0]),
        items(&[("INV-1", 35_000), ("INV-3", 5_000)])
    );

    // Filters are AND-ed; nothing matches -> no groups, zero totals.
    let none = l.aging(&[
        "--account",
        "1100",
        "--filter-metadata",
        "status=disputed",
        "--filter-metadata",
        "customer=Initech",
    ]);
    assert_eq!(none["data"]["groups"].as_array().map(Vec::len), Some(0));
    assert_eq!(int(&none["data"]["totals"]["total_open"]), 0);

    let table = l.aging_text(
        "table",
        &["--account", "1100", "--filter-metadata", "status=disputed"],
    );
    assert!(
        table.contains("Metadata filter: status=disputed"),
        "{table}"
    );
    assert!(
        table.contains("Account balance (unfiltered): 700.00"),
        "{table}"
    );

    let bad = l.run(&[
        "--company",
        CO,
        "report",
        "aging",
        "--account",
        "1100",
        "--filter-metadata",
        "status",
    ]);
    assert!(!bad.ok);
    assert!(bad.stderr.contains("expected KEY=VALUE"), "{}", bad.stderr);
}
