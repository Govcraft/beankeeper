//! Statement reconciliation for AR/AP. Bank regression suites stay untouched.
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

struct Ledger {
    _dir: TempDir,
    path: String,
    payable: bool,
}

impl Ledger {
    fn new(payable: bool) -> Self {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("books.db").to_string_lossy().into_owned();
        let ledger = Self {
            _dir: dir,
            path,
            payable,
        };
        ledger.ok(&["init"]);
        ledger.ok(&["company", "create", "co", "Agency"]);
        for (code, kind) in [
            ("1000", "asset"),
            ("1200", if payable { "liability" } else { "asset" }),
            ("4000", "revenue"),
        ] {
            ledger.ok(&["account", "create", code, code, "--type", kind]);
        }
        ledger
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::cargo_bin("bk")
            .unwrap_or_else(|e| panic!("binary: {e}"))
            .args(["--db", &self.path, "--company", "co", "--no-color"])
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("execute: {e}"))
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap_or_else(|e| panic!("utf8: {e}"))
    }

    fn post(
        &self,
        reference: &str,
        amount: &str,
        payment: bool,
        metadata: &str,
        date: &str,
    ) -> i64 {
        let (debit, credit) = if payment == self.payable {
            ("1200", "4000")
        } else {
            ("1000", "1200")
        };
        let text = self.ok(&[
            "--json",
            "txn",
            "post",
            "-d",
            reference,
            "--date",
            date,
            "--debit",
            &format!("{debit}:{amount}"),
            "--credit",
            &format!("{credit}:{amount}"),
            "--reference",
            reference,
            "--metadata",
            metadata,
        ]);
        let json: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("post JSON: {e}"));
        json["data"]["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("post id: {json}"))
    }

    fn clear(&self, reference: &str, amount: &str, statement: &str, date: &str) {
        self.ok(&[
            "txn",
            "clear",
            "--reference",
            reference,
            "--account",
            "1200",
            "--amount",
            amount,
            "--statement",
            statement,
            "--date",
            date,
        ]);
    }

    fn report(&self, statement_balance: &str, extra: &[&str]) -> Value {
        let mut args = vec![
            "--json",
            "report",
            "reconciliation",
            "--mode",
            "settlement",
            "--account",
            "1200",
            "--statement",
            "S1",
            "--to",
            "2026-10-31",
            "--statement-balance",
            statement_balance,
        ];
        if extra.contains(&"--to") {
            let i = args
                .iter()
                .position(|a| *a == "--to")
                .unwrap_or_else(|| panic!("to flag"));
            args.drain(i..i + 2);
        }
        args.extend_from_slice(extra);
        let text = self.ok(&args);
        let json: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("report JSON: {e}"));
        json["data"].clone()
    }
}

#[test]
fn short_pay_keeps_original_charge_age_whether_payment_is_cleared_or_not() {
    for payable in [false, true] {
        let l = Ledger::new(payable);
        l.post("P1", "1800", false, r#"{"policy":"P1"}"#, "2026-10-01");
        l.post("PAY", "1620", true, r#"{"applies_to":"P1"}"#, "2026-10-25");
        l.clear("P1", "1620", "S1", "2026-10-31");
        for cleared in [false, true] {
            if cleared {
                l.clear("PAY", "1620", "S1", "2026-10-31");
            }
            let d = l.report("1620", &[]);
            assert_eq!(d["reconciled"], true);
            assert_eq!(d["difference"], 0);
            assert_eq!(d["uncleared_total"], 18000);
            assert_eq!(d["uncleared_count"], 1);
            assert_eq!(d["uncleared_entries"][0]["reference"], "P1");
            assert_eq!(d["charges"][0]["expected"], 180_000);
            assert_eq!(d["charges"][0]["applied_this_statement"], 162_000);
            assert_eq!(d["charges"][0]["short_pay"], 18000);
            assert_eq!(d["charges"][0]["age_days"], 30);
            assert_eq!(d["applied_settlements"].as_array().map(Vec::len), Some(1));
        }
    }
}

#[test]
fn overpayment_and_duplicate_lines_are_explicit_and_statement_difference_is_not_hidden() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    l.post("PAY1", "120", true, r#"{"applies_to":"P1"}"#, "2026-10-20");
    l.clear("P1", "100", "S1", "2026-10-31");
    let d = l.report("120", &[]);
    assert_eq!(d["reconciled"], false);
    assert_eq!(d["difference"], 2000);
    assert_eq!(d["charges"][0]["overpay"], 2000);
    assert_eq!(d["applied_settlements"][0]["excess"], 2000);
    l.post(
        "DUPLICATE",
        "120",
        true,
        r#"{"applies_to":"P1"}"#,
        "2026-10-20",
    );
    let d = l.report("240", &[]);
    assert_eq!(d["difference"], 14000);
    assert_eq!(d["charges"][0]["overpay"], 14000);
    assert_eq!(d["applied_settlements"].as_array().map(Vec::len), Some(2));
    assert_eq!(d["uncleared_count"], 0);
}

#[test]
fn unmatched_and_unlinked_payments_never_apply_fifo_or_hide_as_open_charges() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    for (reference, meta) in [
        ("BAD", r#"{"applies_to":"MISSING"}"#),
        ("NO-LINK", ""),
        ("INVALID", r#"{"applies_to":true}"#),
    ] {
        l.post(reference, "10", true, meta, "2026-10-20");
        l.clear(reference, "10", "S1", "2026-10-31");
    }
    let d = l.report("30", &[]);
    assert_eq!(d["difference"], 3000);
    assert_eq!(d["uncleared_total"], 10000);
    assert_eq!(d["uncleared_count"], 1);
    assert_eq!(d["unmatched_settlements"][0]["reasons"][0], "not_found");
    assert_eq!(d["unmatched_settlements"][1]["reasons"][0], "missing_link");
    assert_eq!(d["unmatched_settlements"][2]["reasons"][0], "invalid");
}

#[test]
fn charge_reversal_is_a_negative_charge_on_its_own_statement_and_date() {
    let l = Ledger::new(false);
    let id = l.post("P1", "100", false, "", "2026-10-01");
    l.ok(&[
        "txn",
        "reverse",
        &id.to_string(),
        "--date",
        "2026-10-20",
        "--reference",
        "VOID",
    ]);
    l.clear("P1", "100", "S1", "2026-10-31");
    l.clear("VOID", "100", "S1", "2026-10-31");
    let d = l.report("0", &[]);
    assert_eq!(d["reconciled"], true);
    assert_eq!(d["uncleared_total"], 0);
    assert_eq!(d["charges"][0]["expected"], 0);
    assert_eq!(d["negative_charges"][0]["expected"], -10000);
    assert_eq!(d["negative_charges"][0]["applied_this_statement"], -10000);
    assert_eq!(d["applied_settlements"].as_array().map(Vec::len), Some(0));
    let before = l.report("0", &["--to", "2026-10-19"]);
    assert_eq!(before["charges"][0]["expected"], 10000);
}

#[test]
fn filters_select_charges_after_linking_and_groups_follow_original_metadata() {
    let l = Ledger::new(false);
    let id = l.post("P1", "100", false, r#"{"policy":"A"}"#, "2026-10-01");
    l.post("P2", "100", false, r#"{"policy":"B"}"#, "2026-10-01");
    l.post(
        "PAY",
        "40",
        true,
        &format!(r#"{{"invoice":{id}}}"#),
        "2026-10-20",
    );
    l.clear("P1", "40", "S1", "2026-10-31");
    let d = l.report(
        "40",
        &[
            "--link-key",
            "invoice",
            "--filter-metadata",
            "policy=A",
            "--group-by-metadata",
            "policy",
        ],
    );
    assert_eq!(d["uncleared_total"], 6000);
    assert_eq!(d["charges"].as_array().map(Vec::len), Some(1));
    assert_eq!(d["applied_settlements"].as_array().map(Vec::len), Some(1));
    assert_eq!(d["groups"][0]["value"], "A");
    assert_eq!(d["groups"][0]["uncleared_total"], 6000);
}

#[test]
fn as_of_and_statement_only_count_eligible_charge_clearings() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    l.clear("P1", "20", "OLD", "2026-10-10");
    l.clear("P1", "30", "S1", "2026-10-31");
    l.clear("P1", "50", "FUTURE", "2026-11-30");
    l.post(
        "FUTURE-PAY",
        "50",
        true,
        r#"{"applies_to":"P1"}"#,
        "2026-11-25",
    );
    let d = l.report("30", &[]);
    assert_eq!(d["difference"], 0);
    assert_eq!(d["charges"][0]["remaining"], 5000);
    assert_eq!(d["charges"][0]["applied_this_statement"], 3000);
    assert_eq!(d["applied_settlements"].as_array().map(Vec::len), Some(0));
}

#[test]
fn link_arrays_and_reference_precedence_match_aging() {
    let l = Ledger::new(false);
    let id = l.post("A", "100", false, "", "2026-10-01");
    l.post(&id.to_string(), "200", false, "", "2026-10-01");
    l.post(
        "PAY",
        "250",
        true,
        &format!(r#"{{"applies_to":["{id}","A"]}}"#),
        "2026-10-20",
    );
    let d = l.report("0", &[]);
    assert_eq!(d["charges"][0]["remaining"], 5000);
    assert_eq!(d["charges"][1]["remaining"], 0);
}

#[test]
fn table_csv_and_validation_use_the_same_mode() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    l.post("PAY", "40", true, r#"{"applies_to":"P1"}"#, "2026-10-20");
    l.clear("P1", "40", "S1", "2026-10-31");
    let args = [
        "report",
        "reconciliation",
        "--mode",
        "settlement",
        "--account",
        "1200",
        "--statement",
        "S1",
        "--to",
        "2026-10-31",
    ];
    let table = l.ok(&args);
    assert!(table.contains("Applied settlements"));
    assert!(table.contains("short pay 60.00"));
    let mut csv_args = vec!["--format", "csv"];
    csv_args.extend_from_slice(&args);
    let csv = l.ok(&csv_args);
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    assert_eq!(
        reader
            .headers()
            .unwrap_or_else(|e| panic!("csv header: {e}"))
            .len(),
        12
    );
    let kinds: Vec<String> = reader
        .records()
        .map(|r| r.unwrap_or_else(|e| panic!("csv row: {e}"))[0].into())
        .collect();
    assert!(kinds.contains(&"charge".into()));
    assert!(kinds.contains(&"applied_settlement".into()));
    for extra in [
        vec!["--mode", "settlement"],
        vec!["--link-key", "invoice"],
        vec![
            "--mode",
            "settlement",
            "--statement",
            "S1",
            "--link-key",
            " ",
        ],
        vec![
            "--mode",
            "settlement",
            "--statement",
            "S1",
            "--account",
            "4000",
        ],
    ] {
        let mut args = vec!["report", "reconciliation", "--account", "1200"];
        args.extend(extra);
        assert!(!l.run(&args).status.success());
    }
}

#[test]
fn unusable_links_keep_specific_matching_reasons_and_account_currency_isolation() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    l.post("LATER", "100", false, "", "2026-10-28");
    l.post("AFTER", "100", false, "", "2026-11-01");
    l.ok(&[
        "txn",
        "post",
        "-d",
        "Other account",
        "--date",
        "2026-10-01",
        "--debit",
        "1000:10",
        "--credit",
        "4000:10",
        "--reference",
        "CASH",
    ]);
    l.ok(&[
        "txn",
        "post",
        "-d",
        "Other currency",
        "--date",
        "2026-10-01",
        "--currency",
        "EUR",
        "--debit",
        "1200:10",
        "--credit",
        "4000:10",
        "--reference",
        "EUR",
    ]);
    for (reference, target) in [
        ("BAD-ACCOUNT", "CASH"),
        ("BAD-DATE", "LATER"),
        ("BAD-ASOF", "AFTER"),
        ("BAD-CHARGE", "BAD-ACCOUNT"),
        ("BAD-CURRENCY", "EUR"),
    ] {
        l.post(
            reference,
            "10",
            true,
            &format!(r#"{{"applies_to":"{target}"}}"#),
            "2026-10-20",
        );
    }
    let d = l.report("50", &["--currency", "USD"]);
    let reasons: Vec<&str> = d["unmatched_settlements"]
        .as_array()
        .unwrap_or_else(|| panic!("unmatched array"))
        .iter()
        .map(|s| s["reasons"][0].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        reasons,
        [
            "not_on_account",
            "charge_after_settlement",
            "after_as_of",
            "not_a_charge",
            "not_on_account"
        ]
    );
    assert_eq!(d["uncleared_total"], 20000);
    assert_eq!(d["difference"], 5000);
}

#[test]
fn explicit_bank_mode_has_identical_data_and_no_settlement_fields() {
    let l = Ledger::new(false);
    l.post("P1", "100", false, "", "2026-10-01");
    let common = [
        "--json",
        "report",
        "reconciliation",
        "--account",
        "1200",
        "--to",
        "2026-10-31",
    ];
    let default: Value =
        serde_json::from_str(&l.ok(&common)).unwrap_or_else(|e| panic!("default: {e}"));
    let mut explicit = common.to_vec();
    explicit.extend(["--mode", "bank"]);
    let bank: Value =
        serde_json::from_str(&l.ok(&explicit)).unwrap_or_else(|e| panic!("bank: {e}"));
    assert_eq!(bank["data"], default["data"]);
    for field in [
        "mode",
        "link_key",
        "charges",
        "applied_settlements",
        "negative_charges",
        "unmatched_settlements",
    ] {
        assert!(
            bank["data"].get(field).is_none(),
            "extra bank field {field}"
        );
    }
    let help = l.ok(&["report", "reconciliation", "--help"]);
    assert!(help.contains("--mode settlement"));
    assert!(help.contains("--link-key"));
}

#[test]
fn reversal_reconciled_balance_is_signed_and_missing_statement_balance_stays_null() {
    let l = Ledger::new(false);
    let id = l.post("P1", "100", false, "", "2026-10-01");
    l.ok(&[
        "txn",
        "reverse",
        &id.to_string(),
        "--date",
        "2026-10-20",
        "--reference",
        "VOID",
    ]);
    for reference in ["P1", "VOID"] {
        l.ok(&[
            "txn",
            "clear",
            "--reference",
            reference,
            "--account",
            "1200",
            "--statement",
            "S1",
            "--date",
            "2026-10-31",
            "--status",
            "reconciled",
        ]);
    }
    let d = l.report("0", &[]);
    assert_eq!(d["reconciled_balance"], 0);
    assert_eq!(d["total_cleared_balance"], 0);
    let text = l.ok(&[
        "--json",
        "report",
        "reconciliation",
        "--mode",
        "settlement",
        "--account",
        "1200",
        "--statement",
        "S1",
    ]);
    let json: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("JSON: {e}"));
    assert!(json["data"]["statement_balance"].is_null());
    assert!(json["data"]["difference"].is_null());
    assert!(json["data"]["reconciled"].is_null());
    let out = l.run(&[
        "report",
        "reconciliation",
        "--mode",
        "settlement",
        "--account",
        "4000",
        "--statement",
        "S1",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("asset (AR) or liability (AP)"));
}

#[test]
fn multiple_entries_on_account_are_netted_before_linking() {
    let l = Ledger::new(false);
    l.ok(&[
        "txn",
        "post",
        "-d",
        "Net charge",
        "--date",
        "2026-10-01",
        "--debit",
        "1200:120",
        "--credit",
        "1200:20",
        "--credit",
        "4000:100",
        "--reference",
        "NET",
    ]);
    l.post("PAY", "40", true, r#"{"applies_to":"NET"}"#, "2026-10-20");
    let d = l.report("0", &[]);
    assert_eq!(d["charges"].as_array().map(Vec::len), Some(1));
    assert_eq!(d["charges"][0]["expected"], 10000);
    assert_eq!(d["charges"][0]["remaining"], 6000);
    assert_eq!(d["uncleared_entries"][0]["direction"], "debit");
}

#[test]
fn typed_settlement_exposes_charge_and_payment_money() -> Result<(), Box<dyn std::error::Error>> {
    use beankeeper::types::{AccountCode, Currency, Money};
    use beankeeper_cli::commands::report_reconciliation::{
        ReconciliationRequest, compute_settlement_reconciliation,
    };
    use beankeeper_cli::db::connection::Db;
    use chrono::NaiveDate;

    let ledger = Ledger::new(false);
    ledger.post("P1", "100", false, "", "2026-10-01");
    ledger.post("PAY", "90", true, r#"{"applies_to":"P1"}"#, "2026-10-25");
    ledger.clear("P1", "90", "S1", "2026-10-31");
    let db = Db::open_snapshot(std::path::Path::new(&ledger.path), None)?;
    let account = AccountCode::new("1200")?;
    let mut request = ReconciliationRequest::new(&account, Currency::USD);
    request.as_of = NaiveDate::from_ymd_opt(2026, 10, 31);
    request.statement = Some("S1");
    let request = request.with_statement_balance(Money::usd(9_000))?;
    let report = compute_settlement_reconciliation(&db, "co", &request, None)?;
    let details = report
        .settlement
        .as_ref()
        .ok_or("missing settlement details")?;
    let [charge] = details.charges.as_slice() else {
        panic!("expected one charge")
    };
    let [payment] = details.applied_settlements.as_slice() else {
        panic!("expected one payment")
    };
    assert_eq!(charge.expected, Money::usd(10_000));
    assert_eq!(charge.short_pay, Money::usd(1_000));
    assert_eq!(payment.amount, Money::usd(9_000));
    assert_eq!(payment.charge_transaction_ids, [charge.transaction_id]);
    assert_eq!(report.uncleared_total, charge.short_pay);
    let cli = ledger.report("90", &[]);
    assert_eq!(serde_json::to_value(&report)?, cli);
    assert_eq!(cli["charges"][0]["short_pay_display"], "10.00");
    assert_eq!(cli["applied_settlements"][0]["amount_display"], "90.00");
    Ok(())
}
