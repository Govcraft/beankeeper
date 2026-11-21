//! Integration tests for `bk report reconciliation` (issue #25).
//!
//! Each test builds a fresh ledger in a temp directory, posts a handful of
//! transactions, sets clearance statuses with `bk txn clear`, and checks the
//! book / cleared / reconciled balances and the uncleared-entry list.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

const CO: &str = "co";

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
    fn run(&self, args: &[&str]) -> Out {
        let output = Command::cargo_bin("bk")
            .unwrap_or_else(|e| panic!("bk binary should build: {e}"))
            .arg("--db")
            .arg(&self.db)
            .arg("--no-color")
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("bk should execute: {e}"));
        Out {
            ok: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout)
                .unwrap_or_else(|e| panic!("stdout is utf-8: {e}")),
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.ok, "bk {args:?} should succeed");
        out.stdout
    }

    fn co(&self, args: &[&str]) -> String {
        let mut full = vec!["--company", CO];
        full.extend_from_slice(args);
        self.ok(&full)
    }

    fn post(&self, date: &str, desc: &str, debit: &str, credit: &str) {
        self.co(&[
            "txn", "post", "-d", desc, "--date", date, "--debit", debit, "--credit", credit,
        ]);
    }

    /// Run the reconciliation report as JSON and return its `data` object.
    fn report(&self, extra: &[&str]) -> Value {
        let mut args = vec!["--json", "--company", CO, "report", "reconciliation"];
        args.extend_from_slice(extra);
        let stdout = self.ok(&args);
        let v: Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("valid JSON: {e}\n{stdout}"));
        assert_eq!(v["ok"], true);
        assert_eq!(v["meta"]["command"], "report.reconciliation");
        assert_eq!(v["meta"]["company"], CO);
        v["data"].clone()
    }

    /// Set the clearance status of the (uncleared) entry that `txn_id` posted
    /// to `account`, discovering the entry id from the report itself. The
    /// clearing is dated 2026-01-31 (clearings only count from their date).
    fn clear(&self, account: &str, txn_id: i64, status: &str) {
        let data = self.report(&["--account", account]);
        let entry_id = entries(&data)
            .iter()
            .find(|e| e["transaction_id"].as_i64() == Some(txn_id))
            .and_then(|e| e["entry_id"].as_i64())
            .unwrap_or_else(|| panic!("txn {txn_id} has an uncleared entry on {account}"));
        let (txn, entry) = (txn_id.to_string(), entry_id.to_string());
        self.co(&[
            "txn",
            "clear",
            &txn,
            "--entry",
            &entry,
            "--status",
            status,
            "--date",
            "2026-01-31",
        ]);
    }
}

fn entries(data: &Value) -> Vec<Value> {
    data["uncleared_entries"]
        .as_array()
        .unwrap_or_else(|| panic!("uncleared_entries is an array: {data}"))
        .clone()
}

fn int(v: &Value, key: &str) -> i64 {
    v[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key} is an integer: {v}"))
}

/// Assert the core invariant: book - total cleared == uncleared total == sum
/// of the listed uncleared entries' signed remaining amounts.
fn assert_invariant(data: &Value) {
    let listed: i64 = entries(data)
        .iter()
        .map(|e| int(e, "signed_remaining"))
        .sum();
    assert_eq!(int(data, "uncleared_total"), listed);
    assert_eq!(
        int(data, "book_balance") - int(data, "total_cleared_balance"),
        int(data, "uncleared_total"),
        "book - cleared must equal the uncleared total: {data}"
    );
    assert_eq!(
        int(data, "uncleared_count"),
        i64::try_from(entries(data).len()).unwrap_or(-1)
    );
}

fn new_ledger() -> Ledger {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("utf-8 path"))
        .to_string();
    let l = Ledger { _dir: dir, db };
    l.ok(&["init"]);
    l.ok(&["company", "create", CO, "Test Co"]);
    for (code, name, ty) in [
        ("1000", "Checking", "asset"),
        ("2100", "Credit Card", "liability"),
        ("3000", "Owner Equity", "equity"),
        ("5000", "Expenses", "expense"),
    ] {
        l.co(&["account", "create", code, name, "--type", ty]);
    }
    l
}

/// Ledger with seven transactions (ids 1..=7):
///
/// | id | date       | description     | debit       | credit      |
/// |----|------------|-----------------|-------------|-------------|
/// | 1  | 2026-01-01 | Opening deposit | 1000 1000   | 3000 1000   |
/// | 2  | 2026-01-10 | Rent            | 5000 300    | 1000 300    |
/// | 3  | 2026-01-20 | Owner top-up    | 1000 250.50 | 3000 250.50 |
/// | 4  | 2026-02-05 | Groceries       | 5000 40     | 1000 40     |
/// | 5  | 2026-01-12 | Card purchase   | 5000 75.25  | 2100 75.25  |
/// | 6  | 2026-01-25 | Card payment    | 2100 50     | 1000 50     |
/// | 7  | 2026-02-10 | Card purchase 2 | 5000 20     | 2100 20     |
///
/// On checking (1000): txn 1 reconciled, txn 2 cleared, the rest uncleared.
fn seeded() -> Ledger {
    let l = new_ledger();
    l.post("2026-01-01", "Opening deposit", "1000:1000", "3000:1000");
    l.post("2026-01-10", "Rent", "5000:300", "1000:300");
    l.post("2026-01-20", "Owner top-up", "1000:250.50", "3000:250.50");
    l.post("2026-02-05", "Groceries", "5000:40", "1000:40");
    l.post("2026-01-12", "Card purchase", "5000:75.25", "2100:75.25");
    l.post("2026-01-25", "Card payment", "2100:50", "1000:50");
    l.post("2026-02-10", "Card purchase 2", "5000:20", "2100:20");
    l.clear("1000", 1, "reconciled");
    l.clear("1000", 2, "cleared");
    l
}

#[test]
fn mixed_clearance_statuses() {
    let l = seeded();
    let data = l.report(&["--account", "1000"]);

    // 1000 - 300 + 250.50 - 40 - 50
    assert_eq!(int(&data, "book_balance"), 86_050);
    // reconciled (1000) + cleared (-300)
    assert_eq!(int(&data, "cleared_balance"), 70_000);
    assert_eq!(int(&data, "reconciled_balance"), 100_000);
    // 250.50 - 50 - 40
    assert_eq!(int(&data, "uncleared_total"), 16_050);
    assert_invariant(&data);

    let list = entries(&data);
    let txns: Vec<i64> = list.iter().map(|e| int(e, "transaction_id")).collect();
    // Ordered by date: 2026-01-20, 2026-01-25, 2026-02-05.
    assert_eq!(txns, vec![3, 6, 4]);
    let signed: Vec<i64> = list.iter().map(|e| int(e, "signed_amount")).collect();
    assert_eq!(signed, vec![25_050, -5_000, -4_000]);
    assert_eq!(list[0]["description"], "Owner top-up");
    assert_eq!(list[0]["date"], "2026-01-20");
}

#[test]
fn to_cutoff_excludes_later_entries() {
    let l = seeded();
    let data = l.report(&["--account", "1000", "--to", "2026-01-31"]);

    assert_eq!(data["as_of"], "2026-01-31");
    // 1000 - 300 + 250.50 - 50 (groceries on 2026-02-05 excluded)
    assert_eq!(int(&data, "book_balance"), 90_050);
    assert_eq!(int(&data, "cleared_balance"), 70_000);
    assert_eq!(int(&data, "uncleared_total"), 20_050);
    assert_invariant(&data);
    let txns: Vec<i64> = entries(&data)
        .iter()
        .map(|e| int(e, "transaction_id"))
        .collect();
    assert_eq!(txns, vec![3, 6]);

    // Before anything was cleared: everything is uncleared, nothing cleared.
    let early = l.report(&["--account", "1000", "--to", "2025-12-31"]);
    assert_eq!(int(&early, "book_balance"), 0);
    assert_eq!(int(&early, "uncleared_count"), 0);
    assert_invariant(&early);

    // Invalid dates are rejected as validation errors.
    let bad = l.run(&[
        "--company",
        CO,
        "report",
        "reconciliation",
        "--account",
        "1000",
        "--to",
        "Jan 31",
    ]);
    assert_eq!(bad.code, Some(3));
}

#[test]
fn statement_balance_difference() {
    let l = seeded();

    // Statement matches the cleared balance exactly.
    let data = l.report(&["--account", "1000", "--statement-balance", "700"]);
    assert_eq!(int(&data, "statement_balance"), 70_000);
    assert_eq!(int(&data, "difference"), 0);
    assert_eq!(data["reconciled"], true);

    // Statement 650.25: difference = 650.25 - 700.00 = -49.75.
    let data = l.report(&["--account", "1000", "--statement-balance", "650.25"]);
    assert_eq!(int(&data, "statement_balance"), 65_025);
    assert_eq!(int(&data, "difference"), -4_975);
    assert_eq!(data["reconciled"], false);

    // Negative statement balances (overdrawn) parse with their sign.
    let data = l.report(&["--account", "1000", "--statement-balance", "-12.34"]);
    assert_eq!(int(&data, "statement_balance"), -1_234);
    assert_eq!(int(&data, "difference"), -1_234 - 70_000);

    // Without a statement balance, the statement fields are null.
    let data = l.report(&["--account", "1000"]);
    assert!(data["statement_balance"].is_null());
    assert!(data["difference"].is_null());
    assert!(data["reconciled"].is_null());

    // Table output shows the indicator.
    let table = l.co(&[
        "report",
        "reconciliation",
        "--account",
        "1000",
        "--statement-balance",
        "700.00",
    ]);
    assert!(table.contains("reconciled: yes"), "{table}");
    assert!(table.contains("Owner top-up"), "{table}");
    let table = l.co(&[
        "report",
        "reconciliation",
        "--account",
        "1000",
        "--statement-balance",
        "1",
    ]);
    assert!(table.contains("reconciled: no"), "{table}");

    // Too many decimals is rejected.
    let bad = l.run(&[
        "--company",
        CO,
        "report",
        "reconciliation",
        "--account",
        "1000",
        "--statement-balance",
        "1.234",
    ]);
    assert!(!bad.ok);
}

#[test]
fn credit_normal_liability_account() {
    let l = seeded();
    l.clear("2100", 5, "cleared");
    let data = l.report(&["--account", "2100", "--statement-balance", "75.25"]);

    assert_eq!(data["account"]["normal_balance"], "credit");
    // Credit-normal: 75.25 owed - 50 paid + 20 owed = 45.25 owed (positive).
    assert_eq!(int(&data, "book_balance"), 4_525);
    assert_eq!(int(&data, "cleared_balance"), 7_525);
    assert_eq!(int(&data, "reconciled_balance"), 0);
    assert_eq!(int(&data, "uncleared_total"), -3_000);
    assert_invariant(&data);

    let signed: Vec<(i64, i64)> = entries(&data)
        .iter()
        .map(|e| (int(e, "transaction_id"), int(e, "signed_amount")))
        .collect();
    // Payment (debit) reduces the liability; new purchase (credit) increases it.
    assert_eq!(signed, vec![(6, -5_000), (7, 2_000)]);
    assert_eq!(int(&data, "difference"), 0);
    assert_eq!(data["reconciled"], true);
}

#[test]
fn json_shape() {
    let l = seeded();
    let data = l.report(&["--account", "1000", "--to", "2026-12-31"]);

    let obj = data
        .as_object()
        .unwrap_or_else(|| panic!("data is an object"));
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "account",
            "as_of",
            "book_balance",
            "book_balance_display",
            "cleared_balance",
            "cleared_balance_display",
            "currency",
            "difference",
            "difference_display",
            "group_by_metadata",
            "groups",
            "metadata_filters",
            "partially_cleared_count",
            "reconciled",
            "reconciled_balance",
            "reconciled_balance_display",
            "statement",
            "statement_balance",
            "statement_balance_display",
            "total_cleared_balance",
            "total_cleared_balance_display",
            "uncleared_count",
            "uncleared_entries",
            "uncleared_total",
            "uncleared_total_display",
        ]
    );
    assert_eq!(data["account"]["code"], "1000");
    assert_eq!(data["account"]["name"], "Checking");
    assert_eq!(data["account"]["type"], "asset");
    assert_eq!(data["account"]["normal_balance"], "debit");
    assert_eq!(data["currency"], "USD");

    let first = &entries(&data)[0];
    let mut entry_keys: Vec<&str> = first
        .as_object()
        .unwrap_or_else(|| panic!("entry is an object"))
        .keys()
        .map(String::as_str)
        .collect();
    entry_keys.sort_unstable();
    assert_eq!(
        entry_keys,
        vec![
            "amount",
            "amount_display",
            "cleared_amount",
            "cleared_amount_display",
            "clearings",
            "date",
            "description",
            "direction",
            "entry_id",
            "memo",
            "metadata",
            "partially_cleared",
            "reconciled_amount",
            "reconciled_amount_display",
            "reference",
            "remaining_amount",
            "remaining_amount_display",
            "signed_amount",
            "signed_amount_display",
            "signed_remaining",
            "signed_remaining_display",
            "transaction_id",
        ]
    );
    assert_eq!(int(first, "cleared_amount"), 0);
    assert_eq!(int(first, "remaining_amount"), 25_050);
    assert_eq!(first["partially_cleared"], false);
    assert!(first["reference"].is_null());
    assert!(first["metadata"].is_null());
    assert_eq!(first["direction"], "debit");
    assert_eq!(int(first, "amount"), 25_050);
    assert_eq!(first["amount_display"], "250.50");
    assert_eq!(first["cleared_amount_display"], "0.00");
    assert!(data["difference_display"].is_null());
}

#[test]
fn csv_output_has_entries_then_summary() {
    let l = seeded();
    let csv = l.ok(&[
        "--company",
        CO,
        "--format",
        "csv",
        "report",
        "reconciliation",
        "--account",
        "1000",
        "--statement-balance",
        "700",
    ]);
    let today = chrono::Local::now().date_naive();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "row_type,entry_id,transaction_id,date,reference,description,amount,\
         cleared_amount,remaining_amount,currency,group,metadata"
    );
    assert!(lines[1].starts_with("entry,"), "{csv}");
    assert!(
        lines[1].ends_with(",Owner top-up,25050,0,25050,USD,,"),
        "{csv}"
    );
    assert!(
        csv.contains(&format!("summary,,,{today},,book_balance,86050,,,USD,,")),
        "{csv}"
    );
    assert!(
        csv.contains(&format!("summary,,,{today},,cleared_balance,70000,,,USD,,")),
        "{csv}"
    );
    assert!(
        csv.contains(&format!("summary,,,{today},,difference,0,,,USD,,")),
        "{csv}"
    );
    assert!(
        csv.contains(&format!("summary,,,{today},,reconciled,1,,,USD,,")),
        "{csv}"
    );
}

#[test]
fn currency_resolution() {
    let l = new_ledger();

    // No transactions: defaults to USD.
    let data = l.report(&["--account", "1000"]);
    assert_eq!(data["currency"], "USD");
    assert_eq!(int(&data, "book_balance"), 0);

    // A single currency is picked up automatically.
    l.co(&[
        "txn",
        "post",
        "-d",
        "MXN deposit",
        "--date",
        "2026-01-05",
        "--debit",
        "1000:500",
        "--credit",
        "3000:500",
        "--currency",
        "MXN",
    ]);
    let data = l.report(&["--account", "1000"]);
    assert_eq!(data["currency"], "MXN");
    assert_eq!(int(&data, "book_balance"), 50_000);

    // Several currencies without --currency is ambiguous.
    l.post("2026-01-06", "USD deposit", "1000:100", "3000:100");
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "reconciliation",
        "--account",
        "1000",
    ]);
    assert_eq!(
        out.code,
        Some(3),
        "ambiguous currency is a validation error"
    );

    // An explicit --currency filters to that currency only.
    let usd = l.report(&["--account", "1000", "--currency", "USD"]);
    assert_eq!(int(&usd, "book_balance"), 10_000);
    assert_invariant(&usd);
    let mxn = l.report(&["--account", "1000", "--currency", "mxn"]);
    assert_eq!(mxn["currency"], "MXN");
    assert_eq!(int(&mxn, "book_balance"), 50_000);
}

#[test]
fn unknown_account_is_not_found() {
    let l = new_ledger();
    let out = l.run(&[
        "--company",
        CO,
        "report",
        "reconciliation",
        "--account",
        "9999",
    ]);
    assert_eq!(out.code, Some(5));
}

fn typed_reconciliation_ledger() -> Ledger {
    let ledger = new_ledger();
    ledger.post("2026-01-05", "USD deposit", "1000:25", "3000:25");
    for (date, reference) in [
        ("2026-01-08", "BHD-DEPOSIT"),
        ("2026-02-12", "AFTER-CUTOFF"),
    ] {
        ledger.co(&[
            "txn",
            "post",
            "-d",
            reference,
            "--date",
            date,
            "--debit",
            "1000:100.125",
            "--credit",
            "3000:100.125",
            "--currency",
            "BHD",
            "--reference",
            reference,
            "--metadata",
            r#"{"region":"Gulf","amount":9}"#,
        ]);
    }
    for (statement, amount, date) in [
        ("S1", "40.005", "2026-01-20"),
        ("S2", "20.010", "2026-01-25"),
    ] {
        ledger.co(&[
            "txn",
            "clear",
            "--reference",
            "BHD-DEPOSIT",
            "--account",
            "1000",
            "--amount",
            amount,
            "--statement",
            statement,
            "--date",
            date,
        ]);
    }
    ledger.co(&[
        "txn",
        "clear",
        "--reference",
        "BHD-DEPOSIT",
        "--account",
        "1000",
        "--statement",
        "S1",
        "--date",
        "2026-02-10",
        "--status",
        "reconciled",
    ]);

    ledger
}

#[test]
fn typed_bank_reconciliation_keeps_as_of_statement_and_currency_independent()
-> Result<(), Box<dyn std::error::Error>> {
    use beankeeper::types::{AccountCode, Currency, DebitOrCredit, Money};
    use beankeeper_cli::commands::report_reconciliation::{
        ReconciliationRequest, compute_reconciliation,
    };
    use beankeeper_cli::db::{ClearingState, connection::Db, metadata_filter};
    use chrono::NaiveDate;

    let ledger = typed_reconciliation_ledger();
    let db = Db::open_snapshot(std::path::Path::new(&ledger.db), None)?;
    let account = AccountCode::new("1000")?;
    let currency = Currency::from_code("BHD")?;
    let filters = metadata_filter::parse_filters(&["region=Gulf".to_owned()])?;
    let as_of = NaiveDate::from_ymd_opt(2026, 1, 31);
    let bhd = |units| Money::from_minor(units, currency);
    let mut request = ReconciliationRequest::new(&account, currency);
    request.as_of = as_of;
    request.statement = Some("S1");
    request.filters = &filters;
    request.group_by_metadata = Some("region");
    let request = request.with_statement_balance(bhd(40_005))?;
    let report = compute_reconciliation(&db, CO, &request)?;
    assert_eq!(report.as_of, as_of);
    assert_eq!(report.normal_balance, DebitOrCredit::Debit);
    assert_eq!(report.book_balance, bhd(100_125));
    assert_eq!(report.cleared_balance, bhd(40_005));
    assert_eq!(report.total_cleared_balance, bhd(60_015));
    assert_eq!(report.reconciled_balance, bhd(0));
    assert_eq!(report.uncleared_total, bhd(40_110));
    assert_eq!(report.difference(), Some(bhd(0)));
    assert_eq!(
        report
            .groups
            .as_deref()
            .map(|groups| groups[0].uncleared_total),
        Some(bhd(40_110))
    );
    let [entry] = report.uncleared.as_slice() else {
        panic!("expected one BHD entry")
    };
    assert_eq!(entry.remaining_amount, bhd(40_110));
    assert_eq!(entry.clearings.len(), 2);
    assert_eq!(entry.clearings[0].amount, bhd(40_005));
    assert_eq!(entry.clearings[0].state, ClearingState::Cleared);

    let cli = ledger.report(&[
        "--account",
        "1000",
        "--currency",
        "BHD",
        "--to",
        "2026-01-31",
        "--statement",
        "S1",
        "--statement-balance",
        "40.005",
        "--filter-metadata",
        "region=Gulf",
        "--group-by-metadata",
        "region",
    ]);
    assert_eq!(serde_json::to_value(&report)?, cli);
    assert_eq!(cli["cleared_balance_display"], "40.005");
    assert_eq!(
        cli["uncleared_entries"][0]["clearings"][0]["amount_display"],
        "40.005"
    );
    assert!(
        cli["uncleared_entries"][0]["metadata"]
            .get("amount_display")
            .is_none()
    );

    assert!(matches!(
        ReconciliationRequest::new(&account, currency).with_statement_balance(Money::usd(40_005)),
        Err(beankeeper_cli::error::CliError::Usage(_))
    ));
    assert!(matches!(
        ReconciliationRequest::new(&account, currency).with_statement_balance(bhd(i128::MAX)),
        Err(beankeeper_cli::error::CliError::Usage(_))
    ));
    Ok(())
}

#[test]
fn typed_reconciliation_looks_up_existing_account_keys_verbatim()
-> Result<(), Box<dyn std::error::Error>> {
    use beankeeper::types::{Currency, Money};
    use beankeeper_cli::commands::report_reconciliation::{
        ReconciliationRequest, compute_reconciliation,
    };
    use beankeeper_cli::db::connection::Db;
    use beankeeper_cli::error::CliError;

    let ledger = new_ledger();
    let path = std::path::Path::new(&ledger.db);
    rusqlite::Connection::open(path)?.execute(
        "INSERT INTO accounts (company_slug, code, name, type) VALUES (?1, 'BANK-OPERATING', 'Operating bank', 'asset')",
        [CO],
    )?;
    let db = Db::open_snapshot(path, None)?;
    let request = ReconciliationRequest::for_existing_account("BANK-OPERATING", Currency::USD);
    let report = compute_reconciliation(&db, CO, &request)?;
    assert_eq!(report.code, "BANK-OPERATING");
    assert_eq!(report.book_balance, Money::usd(0));

    for key in ["bank-operating", "BANK-MISSING"] {
        let request = ReconciliationRequest::for_existing_account(key, Currency::USD);
        assert!(matches!(
            compute_reconciliation(&db, CO, &request),
            Err(CliError::NotFound(_))
        ));
    }
    Ok(())
}
