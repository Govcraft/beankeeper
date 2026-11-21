//! Tax exports preserve unmatched activity and exact vendor payment totals.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use chrono::{Datelike, Local};
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    _dir: TempDir,
    path: PathBuf,
}

fn bk(path: &Path, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command.arg("--db").arg(path).args(args);
    for key in [
        "BEANKEEPER_PASSPHRASE_CMD",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_CURRENCY",
        "BK_AGENT_TOKEN",
        "BK_ACTOR",
    ] {
        command.env_remove(key);
    }
    Ok(command.output()?)
}

fn ok(path: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = bk(path, args)?;
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tax.db");
        ok(&path, &["init"])?;
        ok(&path, &["company", "create", "llc", "LLC"])?;
        for (code, name, kind, tag) in [
            ("1000", "Cash", "asset", None),
            ("4000", "Sales", "revenue", Some("sched-c:1")),
            ("5000", "Office, supplies", "expense", Some("sched-c:18")),
            ("5100", "Meals", "expense", Some("sched-c:24b")),
            ("5200", "Research", "expense", Some("misc-research")),
            ("5300", "Uncategorized", "expense", None),
        ] {
            let mut args = vec![
                "--company",
                "llc",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ];
            if let Some(tag) = tag {
                args.extend(["--default-tax-category", tag]);
            }
            ok(&path, &args)?;
        }
        Ok(Self { _dir: dir, path })
    }

    fn post(&self, date: &str, debit: &str, credit: &str, extra: &[&str]) -> TestResult {
        let mut args = vec![
            "--company",
            "llc",
            "txn",
            "post",
            "-d",
            "Tax fixture",
            "--date",
            date,
            "--debit",
            debit,
            "--credit",
            credit,
        ];
        args.extend_from_slice(extra);
        ok(&self.path, &args)?;
        Ok(())
    }

    fn report(&self, kind: &str, extra: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut args = vec!["--company", "llc", "report", kind, "--json"];
        args.extend_from_slice(extra);
        Ok(serde_json::from_str(&ok(&self.path, &args)?)?)
    }

    fn seeded() -> Result<Self, Box<dyn Error>> {
        let books = Self::new()?;
        books.post("2026-01-01", "1000:1000", "4000:1000", &[])?;
        books.post(
            "2026-01-02",
            "5000:200",
            "1000:200",
            &[
                "--meta",
                "payee=Jordan Key",
                "--meta",
                "tin=001-02-0003",
                "--meta",
                "payee_address=12 Main St",
            ],
        )?;
        books.post(
            "2026-01-03",
            "5000:100",
            "1000:100",
            &[
                "--meta",
                "payee=Jordan Key",
                "--debit",
                "5100:150",
                "--credit",
                "1000:150",
            ],
        )?;
        books.post(
            "2026-12-31",
            "5100:250",
            "1000:250",
            &["--meta", "payee=Jordan Key"],
        )?;
        books.post(
            "2026-01-05",
            "5200:100",
            "1000:100",
            &["--meta", "payee=Other, Vendor"],
        )?;
        books.post("2026-01-06", "5300:50", "1000:50", &[])?;
        books.post(
            "2025-12-31",
            "5000:999",
            "1000:999",
            &["--meta", "payee=Jordan Key"],
        )?;
        books.post(
            "2027-01-01",
            "5000:999",
            "1000:999",
            &["--meta", "payee=Jordan Key"],
        )?;
        Ok(books)
    }
}

const YEAR: &[&str] = &["--from", "2026-01-01", "--to", "2026-12-31"];

fn rows<'a>(data: &'a Value, key: &str) -> Result<&'a Vec<Value>, Box<dyn Error>> {
    data[key]
        .as_array()
        .ok_or_else(|| format!("missing array {key}: {data}").into())
}

#[test]
fn typed_tax_queries_expose_money_and_share_cli_exports() -> TestResult {
    use beankeeper::types::{Currency, Money};
    use beankeeper_cli::commands::report_tax::{
        Period, VendorThreshold, build_schedule_c, build_vendors, render_schedule_c_csv,
        render_vendors_csv,
    };
    use beankeeper_cli::db::Db;
    use chrono::NaiveDate;

    let books = Books::new()?;
    for currency in ["USD", "BHD"] {
        books.post(
            "2026-02-19",
            "5000:1.20",
            "1000:1.20",
            &["--currency", currency, "--meta", "payee=Ledger Vendor"],
        )?;
    }
    let db = Db::open_snapshot(&books.path, None)?;
    let from = NaiveDate::parse_from_str("2026-01-01", "%Y-%m-%d")?;
    let to = NaiveDate::parse_from_str("2026-12-31", "%Y-%m-%d")?;
    let period = Period::new(from, to)?;
    assert!(Period::new(to, from).is_err());
    assert!(VendorThreshold::new(Money::usd(-1)).is_err());

    let currency = Currency::BHD;
    let paid = Money::from_minor(1200, currency);
    let schedule = build_schedule_c(&db, "llc", period, currency)?;
    assert_eq!(schedule.period(), period);
    assert_eq!(schedule.currency(), currency);
    assert_eq!(schedule.summary().lines()[0].line(), Some("18"));
    assert_eq!(schedule.summary().lines()[0].debit_total(), paid);
    assert_eq!(schedule.summary().lines()[0].net(), paid);

    let vendors = build_vendors(&db, "llc", period, VendorThreshold::new(paid)?)?;
    assert_eq!(vendors.summary().threshold(), paid);
    assert_eq!(vendors.summary().vendors()[0].total(), paid);
    assert_eq!(vendors.summary().vendors()[0].payment_count, 1);

    for (command, data, csv, threshold_args) in [
        (
            "schedule-c",
            serde_json::to_value(&schedule)?,
            render_schedule_c_csv(&schedule)?,
            Vec::new(),
        ),
        (
            "1099",
            serde_json::to_value(&vendors)?,
            render_vendors_csv(&vendors)?,
            vec!["--threshold", "1.20"],
        ),
    ] {
        let mut args = vec!["--currency", "BHD"];
        args.extend_from_slice(YEAR);
        args.extend(threshold_args);
        assert_eq!(books.report(command, &args)?["data"], data);
        let mut csv_args = vec!["--company", "llc", "report", command, "--format", "csv"];
        csv_args.extend(args);
        let cli_csv = ok(&books.path, &csv_args)?;
        assert_eq!(cli_csv.trim_end_matches('\n'), csv.trim_end_matches('\n'));
    }
    Ok(())
}

#[test]
fn schedule_c_matches_tax_summary_and_preserves_unmatched_activity_in_all_formats() -> TestResult {
    let books = Books::seeded()?;
    let schedule = books.report("schedule-c", YEAR)?;
    assert_eq!(schedule["meta"]["command"], "report.schedule-c");
    let data = &schedule["data"];
    let tax = books.report("tax-summary", YEAR)?;
    for (index, (line, tag, debit, credit, net)) in [
        ("1", "sched-c:1", 0, 100_000, 100_000),
        ("18", "sched-c:18", 30_000, 0, 30_000),
        ("24b", "sched-c:24b", 40_000, 0, 40_000),
    ]
    .into_iter()
    .enumerate()
    {
        let row = &data["lines"][index];
        assert_eq!(row["line"], line);
        assert_eq!(row["debit_total"], debit);
        assert_eq!(row["credit_total"], credit);
        assert_eq!(row["net"], net);
        let summary = tax["data"]
            .as_array()
            .ok_or("tax summary array")?
            .iter()
            .find(|row| row["tax_category"] == tag)
            .ok_or("missing tag")?;
        assert_eq!(row["debit_total"], summary["debit_total"]);
        assert_eq!(row["credit_total"], summary["credit_total"]);
    }
    assert_eq!(data["lines"][1]["net_display"], "300.00");
    assert_eq!(data["unmapped"][0]["tax_category"], "misc-research");
    assert_eq!(data["unmapped"][0]["net"], 10_000);
    assert_eq!(data["untagged"][0]["account_code"], "5300");
    assert_eq!(data["untagged"][0]["net"], 5000);
    assert_eq!(rows(data, "untagged")?.len(), 1); // Excludes untagged cash legs.
    let csv = ok(
        &books.path,
        &[
            "--company",
            "llc",
            "report",
            "schedule-c",
            "--to",
            "2026-12-31",
            "--output-format",
            "csv",
        ],
    )?;
    let records = csv::Reader::from_reader(csv.as_bytes())
        .deserialize::<BTreeMap<String, String>>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 5);
    assert_eq!(records[1]["title"], "Office expense");
    assert_eq!(records[3]["section"], "unmapped");
    assert_eq!(records[4]["section"], "untagged");
    assert_eq!(records[4]["currency"], "USD");
    let table = ok(
        &books.path,
        &[
            "--company",
            "llc",
            "report",
            "schedule-c",
            "--to",
            "2026-12-31",
        ],
    )?;
    for label in [
        "unmapped",
        "untagged",
        "misc-research",
        "5300",
        "300.00",
        "Not tax advice",
    ] {
        assert!(table.contains(label), "{table}");
    }
    Ok(())
}

#[test]
fn vendor_threshold_counts_split_payments_once_and_retains_details() -> TestResult {
    let books = Books::seeded()?;
    for kind in ["1099", "1099-nec"] {
        let report = books.report(kind, YEAR)?;
        assert_eq!(report["meta"]["command"], "report.1099");
        let data = &report["data"];
        assert_eq!(data["threshold"], 60_000);
        assert_eq!(data["threshold_display"], "600.00");
        assert_eq!(rows(data, "vendors")?.len(), 1);
        let vendor = &data["vendors"][0];
        assert_eq!(vendor["payee"], "Jordan Key");
        assert_eq!(vendor["payment_count"], 3);
        assert_eq!(vendor["total"], 70_000);
        assert_eq!(vendor["total_display"], "700.00");
        assert_eq!(vendor["tin"], "001-02-0003");
        assert_eq!(vendor["payee_address"], "12 Main St");
        assert_eq!(vendor["meets_threshold"], true);
    }
    let everyone = books.report("1099", &["--to", "2026-12-31", "--threshold", "0"])?;
    assert_eq!(rows(&everyone["data"], "vendors")?.len(), 2);
    assert_eq!(everyone["data"]["vendors"][1]["total"], 10_000);
    assert!(everyone["data"]["vendors"][1]["tin"].is_null());
    let exact = books.report("1099", &["--to", "2026-12-31", "--threshold", "700"])?;
    assert_eq!(rows(&exact["data"], "vendors")?.len(), 1);
    let above = books.report("1099", &["--to", "2026-12-31", "--threshold", "700.01"])?;
    assert_eq!(
        rows(&above["data"], "vendors")?.as_slice(),
        [] as [Value; 0]
    );
    let csv = ok(
        &books.path,
        &[
            "--company",
            "llc",
            "report",
            "1099",
            "--to",
            "2026-12-31",
            "--threshold",
            "0",
            "--format",
            "csv",
        ],
    )?;
    let records = csv::Reader::from_reader(csv.as_bytes())
        .deserialize::<BTreeMap<String, String>>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["total"], "70000");
    assert_eq!(records[0]["payment_count"], "3");
    assert_eq!(records[1]["payee"], "Other, Vendor");
    let table = ok(
        &books.path,
        &["--company", "llc", "report", "1099", "--to", "2026-12-31"],
    )?;
    assert!(table.contains("700.00") && table.contains("Jordan Key") && table.contains("600.00"));
    let filtered = ok(
        &books.path,
        &[
            "--company",
            "llc",
            "txn",
            "list",
            "--filter-metadata",
            "payee=Jordan Key",
            "--json",
        ],
    )?;
    assert_eq!(
        serde_json::from_str::<Value>(&filtered)?["data"]
            .as_array()
            .map(Vec::len),
        Some(5)
    );
    Ok(())
}

#[test]
fn schedule_normalizes_case_and_accounts_for_refunds_and_tagged_assets() -> TestResult {
    let books = Books::new()?;
    books.post(
        "2026-02-01",
        "5000:10",
        "1000:10",
        &["--tax", "5000=SCHED-C:18", "--tax", "1000=unknown-asset"],
    )?;
    books.post("2026-02-02", "1000:10", "5000:10", &[])?;
    books.post(
        "2026-02-03",
        "4000:5",
        "1000:5",
        &["--tax", "4000=sched-c:2"],
    )?;
    books.post(
        "2026-02-04",
        "5000:1",
        "1000:1",
        &["--tax", "5000=sched-c:999"],
    )?;
    let report = books.report("schedule-c", YEAR)?;
    let lines = rows(&report["data"], "lines")?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["line"], "2");
    assert_eq!(lines[0]["net"], 500);
    assert_eq!(lines[1]["line"], "18");
    assert_eq!(lines[1]["debit_total"], 1000);
    assert_eq!(lines[1]["credit_total"], 1000);
    assert_eq!(lines[1]["net"], 0);
    let unmapped = rows(&report["data"], "unmapped")?;
    assert_eq!(unmapped.len(), 2);
    assert_eq!(unmapped[1]["tax_category"], "unknown-asset");
    assert_eq!(unmapped[1]["credit_total"], 1000);
    assert_eq!(unmapped[1]["net"], -1000);
    Ok(())
}

#[test]
fn report_dates_and_currency_follow_tax_summary_defaults() -> TestResult {
    let books = Books::seeded()?;
    for kind in ["schedule-c", "1099"] {
        let default = books.report(kind, &[])?;
        let today = Local::now().date_naive();
        assert_eq!(default["data"]["to"], today.to_string());
        assert_eq!(default["data"]["from"], format!("{}-01-01", today.year()));
        let historical = books.report(kind, &["--to", "2025-12-31"])?;
        assert_eq!(historical["data"]["from"], "2025-01-01");
        for bounds in [
            vec!["--from", "2026-02-30"],
            vec!["--from", "2026-02-01", "--to", "2026-01-01"],
        ] {
            let mut args = vec!["--company", "llc", "report", kind];
            args.extend(bounds);
            assert_eq!(bk(&books.path, &args)?.status.code(), Some(3));
        }
    }
    books.post(
        "2026-01-01",
        "5000:1.234",
        "1000:1.234",
        &["--currency", "BHD", "--meta", "payee=BHD Vendor"],
    )?;
    for kind in ["schedule-c", "1099"] {
        let out = bk(&books.path, &["--company", "llc", "report", kind])?;
        assert_eq!(out.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&out.stderr).contains("--currency"));
        let report = books.report(kind, &["--to", "2026-12-31", "--currency", "USD"])?;
        assert_eq!(report["data"]["currency"], "USD");
    }
    let bhd = books.report(
        "1099",
        &[
            "--currency",
            "BHD",
            "--threshold",
            "1.234",
            "--to",
            "2026-12-31",
        ],
    )?;
    assert_eq!(bhd["data"]["threshold"], 1234);
    assert_eq!(bhd["data"]["vendors"][0]["total_display"], "1.234");
    books.post(
        "2026-01-01",
        "5000:600",
        "1000:600",
        &["--currency", "JPY", "--meta", "payee=JPY Vendor"],
    )?;
    let jpy = books.report("1099", &["--currency", "JPY", "--to", "2026-12-31"])?;
    assert_eq!(jpy["data"]["threshold"], 600);
    assert_eq!(jpy["data"]["vendors"][0]["total_display"], "600");
    for threshold in ["-1", "0.001", "no", "99999999999999999999999"] {
        let out = bk(
            &books.path,
            &[
                "--company",
                "llc",
                "report",
                "1099",
                "--currency",
                "USD",
                "--threshold",
                threshold,
            ],
        )?;
        assert!(!out.status.success(), "accepted {threshold}");
    }
    Ok(())
}

#[test]
fn empty_books_and_payee_metadata_edge_cases() -> TestResult {
    let books = Books::new()?;
    let empty = books.report("schedule-c", YEAR)?;
    for section in ["lines", "unmapped", "untagged"] {
        assert_eq!(rows(&empty["data"], section)?.as_slice(), [] as [Value; 0]);
    }
    assert_eq!(empty["data"]["currency"], "USD");
    assert_eq!(
        rows(&books.report("1099", YEAR)?["data"], "vendors")?.as_slice(),
        [] as [Value; 0]
    );
    for metadata in [
        r#"{"payee":null}"#,
        r#"{"payee":123}"#,
        r#"{"payee":" "}"#,
        r#"{"memo":"payee=Lost"}"#,
    ] {
        books.post("2026-01-01", "5000:700", "1000:700", &["-m", metadata])?;
    }
    assert_eq!(
        rows(&books.report("1099", YEAR)?["data"], "vendors")?.as_slice(),
        [] as [Value; 0]
    );
    books.post(
        "2026-01-01",
        "5000:700",
        "1000:700",
        &["--meta", "payee=Vendor", "--meta", "tin="],
    )?;
    assert!(books.report("1099", YEAR)?["data"]["vendors"][0]["tin"].is_null());
    Ok(())
}

#[test]
fn conflicting_or_nonstring_vendor_details_fail_without_disclosing_tins() -> TestResult {
    for key in ["tin", "payee_address"] {
        let books = Books::new()?;
        books.post(
            "2026-01-01",
            "5000:700",
            "1000:700",
            &[
                "--meta",
                "payee=Vendor",
                "--meta",
                &format!("{key}=first-detail"),
            ],
        )?;
        books.post(
            "2026-01-02",
            "5000:1",
            "1000:1",
            &[
                "--meta",
                "payee=Vendor",
                "--meta",
                &format!("{key}=second-detail"),
            ],
        )?;
        let out = bk(
            &books.path,
            &[
                "--company",
                "llc",
                "report",
                "1099",
                "--to",
                "2026-12-31",
                "--json",
            ],
        )?;
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, [] as [u8; 0]);
        let error: Value = serde_json::from_slice(&out.stderr)?;
        let message = error["error"]["message"].as_str().ok_or("message")?;
        assert!(message.contains(&format!("conflicting {key}")));
        assert!(!message.contains("first-detail") && !message.contains("second-detail"));
    }
    let books = Books::new()?;
    books.post(
        "2026-01-01",
        "5000:700",
        "1000:700",
        &["-m", r#"{"payee":"Vendor","tin":123}"#],
    )?;
    let out = bk(
        &books.path,
        &["--company", "llc", "report", "1099", "--to", "2026-12-31"],
    )?;
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("must be a string"));
    Ok(())
}

#[test]
fn aggregate_overflow_fails_both_reports_without_partial_output() -> TestResult {
    let books = Books::new()?;
    books.post(
        "2026-01-01",
        "5000:92233720368547758.07",
        "1000:92233720368547758.07",
        &["--meta", "payee=Vendor"],
    )?;
    books.post(
        "2026-01-02",
        "5000:0.01",
        "1000:0.01",
        &["--meta", "payee=Vendor"],
    )?;
    for kind in ["schedule-c", "1099"] {
        let out = bk(
            &books.path,
            &[
                "--company",
                "llc",
                "report",
                kind,
                "--to",
                "2026-12-31",
                "--output-format",
                "json",
            ],
        )?;
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, [] as [u8; 0]);
        let error: Value = serde_json::from_slice(&out.stderr)?;
        assert_eq!(error["error"]["code"], "VALIDATION");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|s| s.contains("minor-unit range"))
        );
    }
    Ok(())
}

#[test]
fn another_company_with_same_account_codes_never_affects_exports() -> TestResult {
    let books = Books::seeded()?;
    ok(&books.path, &["company", "create", "other", "Other"])?;
    for (code, kind) in [("1000", "asset"), ("4000", "revenue")] {
        ok(
            &books.path,
            &[
                "--company",
                "other",
                "account",
                "create",
                code,
                "Account",
                "--type",
                kind,
            ],
        )?;
    }
    ok(
        &books.path,
        &[
            "--company",
            "other",
            "txn",
            "post",
            "-d",
            "Other company",
            "--date",
            "2026-01-01",
            "--debit",
            "1000:999",
            "--credit",
            "4000:999",
            "--tax",
            "4000=sched-c:1",
            "--meta",
            "payee=Jordan Key",
            "--meta",
            "tin=DIFFERENT",
        ],
    )?;
    assert_eq!(
        books.report("schedule-c", YEAR)?["data"]["lines"][0]["net"],
        100_000
    );
    assert_eq!(
        books.report("1099", YEAR)?["data"]["vendors"][0]["total"],
        70_000
    );
    Ok(())
}

#[test]
fn current_form_interest_lines_remain_distinct_and_in_form_order() -> TestResult {
    let books = Books::new()?;
    for (tag, amount) in [
        ("sched-c:16c", "5000:3"),
        ("sched-c:16b", "5000:2"),
        ("sched-c:16a", "5000:1"),
    ] {
        let credit = match tag {
            "sched-c:16c" => "1000:3",
            "sched-c:16b" => "1000:2",
            _ => "1000:1",
        };
        books.post(
            "2026-01-01",
            amount,
            credit,
            &["--tax", &format!("5000={tag}")],
        )?;
    }
    let report = books.report("schedule-c", YEAR)?;
    let lines = rows(&report["data"], "lines")?;
    assert_eq!(
        lines
            .iter()
            .map(|line| line["line"].as_str())
            .collect::<Vec<_>>(),
        [Some("16a"), Some("16b"), Some("16c")]
    );
    assert_eq!(lines[1]["title"], "Vehicle loan interest");
    assert_eq!(lines[2]["title"], "Other interest");
    assert_eq!(lines[2]["net"], 300);
    Ok(())
}
