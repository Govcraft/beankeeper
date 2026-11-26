//! Owner-facing summary calculations, transport contract and read-only behavior.

use std::error::Error;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper::types::Currency;
use beankeeper_cli::{
    cli::{Cli, Command as BkCommand},
    commands::{
        report::DateRange,
        summary::{
            OwnerSummaryReader, SummaryAccounts, SummaryArgs, SummaryRequest, SummaryWindow,
        },
    },
    db::{Actor, companies::CompanySlug, connection::Db},
    error::CliError,
    operation::Operation,
    request::{LedgerRequest, LedgerResponse},
};
use clap::Parser;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let ledger = Self {
            dir: tempfile::tempdir()?,
        };
        ledger.ok(&["init"])?;
        ledger.ok(&["company", "create", "co", "Company"])?;
        for (code, name, kind) in [
            ("1000", "Cash", "asset"),
            ("1100", "Accounts Receivable", "asset"),
            ("1200", "Equipment", "asset"),
            ("2000", "Accounts Payable", "liability"),
            ("2100", "Loan", "liability"),
            ("3000", "Owner funds", "equity"),
            ("4000", "Sales", "revenue"),
            ("5000", "Dining", "expense"),
            ("5100", "Rent, office", "expense"),
            ("5200", "Unused", "expense"),
        ] {
            ledger.ok(&["account", "create", code, name, "--type", kind])?;
        }
        ledger.ok(&["account", "classify", "1000", "cash"])?;
        Ok(ledger)
    }
    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let mut cmd = Command::cargo_bin("bk")?;
        cmd.arg("--db")
            .arg(self.dir.path().join("books.db"))
            .args(["--quiet", "--company", "co"]);
        for var in [
            "BEANKEEPER_PASSPHRASE_CMD",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
            "BK_AGENT_TOKEN",
        ] {
            cmd.env_remove(var);
        }
        Ok(cmd.args(args).output()?)
    }
    fn ok(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = self.run(args)?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }
    fn post(&self, date: &str, debit: &str, credit: &str, currency: &str) -> TestResult {
        self.ok(&[
            "txn",
            "post",
            "-d",
            "Summary fixture",
            "--date",
            date,
            "--debit",
            debit,
            "--credit",
            credit,
            "--currency",
            currency,
        ])?;
        Ok(())
    }
    fn summary(&self, options: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut args = vec!["summary", "--json"];
        args.extend(options);
        Ok(serde_json::from_str(&self.ok(&args)?)?)
    }
}

fn read_summary(db: &Db, company: &str, args: &SummaryArgs) -> Result<Value, Box<dyn Error>> {
    let actor = Actor::new("fixture".parse()?);
    let request = SummaryRequest::try_from((&company.parse()?, args, (false, false)))?;
    let LedgerResponse::Summary(response) = LedgerRequest::Summary(request).execute(db, &actor)?
    else {
        return Err("expected an in-process summary response".into());
    };
    Ok(serde_json::to_value(response.envelope(&actor))?)
}

fn metric<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["data"]["comparison"]["series"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id))
        .unwrap_or_else(|| panic!("missing metric {id}: {report}"))
}

#[test]
fn empty_books_are_useful_and_unknown_due_dates_are_not_reported_as_zero() -> TestResult {
    let ledger = Ledger::new()?;
    let report = ledger.summary(&["--to", "2024-03-15"])?;
    assert_eq!(report["ok"], true);
    assert_eq!(report["meta"]["command"], "summary");
    assert_eq!(report["meta"]["company"], "co");
    assert_eq!(
        report["data"]["periods"],
        json!({"current":{"from":"2024-03-01","to":"2024-03-15"},"prior":{"from":"2024-02-01","to":"2024-02-29"}})
    );
    for key in ["cash_on_hand", "owed_to_you", "you_owe", "net_worth"] {
        assert_eq!(report["data"]["standing"][key], 0);
    }
    for id in ["income", "spending", "left_over"] {
        assert_eq!(metric(&report, id)["current"], 0);
        assert_eq!(metric(&report, id)["change_percent"], Value::Null);
    }
    assert_eq!(report["data"]["biggest_movers"]["series"], json!([]));
    let attention = &report["data"]["needs_attention"]["items"];
    assert_eq!(attention["recurring_due"]["count"], 0);
    assert_eq!(attention["unreconciled_accounts"]["count"], 0);
    assert_eq!(attention["overdue_receivables"]["available"], false);
    assert!(attention["overdue_receivables"]["total"].is_null());
    assert_eq!(report["data"]["desktop_view"]["view"], "financial-summary");
    assert!(
        report["data"]["desktop_view"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("beankeeper://summary?company=co&"))
    );
    Ok(())
}

#[test]
fn one_month_preserves_book_values_and_separates_cash_from_income() -> TestResult {
    let ledger = Ledger::new()?;
    for (date, debit, credit) in [
        ("2024-02-29", "1000:1000", "3000:1000"),
        ("2024-03-01", "1000:500", "2100:500"),
        ("2024-03-02", "1100:400", "4000:400"),
        ("2024-03-03", "5000:100", "1000:100"),
        ("2024-03-04", "1200:50", "1000:50"),
        ("2024-03-05", "5100:200", "2000:200"),
        ("2024-03-16", "5000:999", "1000:999"),
    ] {
        ledger.post(date, debit, credit, "USD")?;
    }
    let report = ledger.summary(&["--to", "2024-03-15"])?;
    let standing = &report["data"]["standing"];
    assert_eq!(standing["cash_on_hand"], 135_000);
    assert_eq!(standing["cash_on_hand_display"], "1350.00");
    assert_eq!(standing["owed_to_you"], 40000);
    assert_eq!(standing["you_owe"], 70000);
    assert_eq!(standing["net_worth"], 110_000);
    assert_eq!(metric(&report, "income")["current"], 40000);
    assert_eq!(metric(&report, "spending")["current"], 30000);
    assert_eq!(metric(&report, "left_over")["current"], 10000);
    assert!(metric(&report, "spending")["change_percent"].is_null());
    let highlights = report["data"]["highlights"].to_string();
    for forbidden in ["debit", "credit", "1000", "5000"] {
        assert!(!highlights.contains(forbidden), "{highlights}");
    }
    Ok(())
}

#[test]
fn comparisons_include_refunds_new_categories_and_both_increases_and_decreases() -> TestResult {
    let ledger = Ledger::new()?;
    for (date, debit, credit) in [
        ("2024-02-01", "1000:1000", "4000:1000"),
        ("2024-02-01", "5000:100", "1000:100"),
        ("2024-02-01", "5100:300", "1000:300"),
        ("2024-03-01", "1000:1200", "4000:1200"),
        ("2024-03-01", "5000:150", "1000:150"),
        ("2024-03-02", "1000:30", "5000:30"),
        ("2024-03-01", "5100:200", "1000:200"),
    ] {
        ledger.post(date, debit, credit, "USD")?;
    }
    let report = ledger.summary(&["--to", "2024-03-31"])?;
    assert_eq!(metric(&report, "income")["change"], 20000);
    assert_eq!(metric(&report, "income")["change_percent"], 20.0);
    assert_eq!(metric(&report, "spending")["current"], 32000);
    assert_eq!(metric(&report, "spending")["prior"], 40000);
    assert_eq!(metric(&report, "spending")["change"], -8000);
    assert_eq!(metric(&report, "spending")["change_percent"], -20.0);
    assert_eq!(metric(&report, "left_over")["change"], 28000);
    let movers = &report["data"]["biggest_movers"]["series"];
    assert_eq!(movers[0]["id"], "5100");
    assert_eq!(movers[0]["change"], -10000);
    assert_eq!(movers[1]["id"], "5000");
    assert_eq!(movers[1]["change_percent"], 20.0);
    assert_eq!(movers.as_array().map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn single_currency_selectors_and_company_isolation_are_enforced() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-03-01", "1000:100", "4000:100", "JPY")?;
    ledger.post("2024-03-01", "1000:12", "4000:12", "USD")?;
    let failed = ledger.run(&["summary", "--json", "--to", "2024-03-31"])?;
    assert_eq!(failed.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&failed.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    let report = ledger.summary(&[
        "--to",
        "2024-03-31",
        "--currency",
        "jpy",
        "--cash-account",
        "1000",
        "--cash-account",
        "1000",
    ])?;
    assert_eq!(report["data"]["currency"], "JPY");
    assert_eq!(metric(&report, "income")["current"], 100);
    assert_eq!(metric(&report, "income")["current_display"], "100");
    assert_eq!(report["data"]["standing"]["cash_accounts"], json!(["1000"]));
    ledger.ok(&["company", "create", "other", "Other"])?;
    let db = Db::open_snapshot(&ledger.dir.path().join("books.db"), None)?;
    let cli = Cli::try_parse_from(["bk", "summary", "--to", "2024-03-31"])?;
    let BkCommand::Summary(args) = &cli.command else {
        return Err("expected summary".into());
    };
    let isolated = read_summary(&db, "other", args)?;
    assert_eq!(isolated["data"]["standing"]["cash_on_hand"], 0);
    assert_eq!(
        isolated["data"]["standing"]["cash_accounts_identified"],
        false
    );
    assert!(
        isolated["data"]["highlights"][0]
            .as_str()
            .is_some_and(|fact| fact.contains("No cash accounts are classified"))
    );
    assert_eq!(isolated["data"]["standing"]["cash_accounts"], json!([]));
    Ok(())
}

#[test]
fn read_only_snapshot_matches_cli_and_summary_adds_no_audit_or_ledger_rows() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-03-01", "1000:12.34", "4000:12.34", "USD")?;
    let db = Db::open_snapshot(&ledger.dir.path().join("books.db"), None)?;
    let cli = Cli::try_parse_from(["bk", "--company", "co", "summary", "--to", "2024-03-31"])?;
    let BkCommand::Summary(args) = &cli.command else {
        return Err("expected summary".into());
    };
    let total_changes = || {
        db.read()
            .query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
    };
    let changes = total_changes()?;
    let mut actual = read_summary(&db, "co", args)?;
    let mut expected = ledger.summary(&["--to", "2024-03-31"])?;
    actual["meta"]
        .as_object_mut()
        .ok_or("meta")?
        .remove("timestamp");
    expected["meta"]
        .as_object_mut()
        .ok_or("meta")?
        .remove("timestamp");
    assert_eq!(actual, expected);
    assert_eq!(total_changes()?, changes);
    ledger.ok(&["verify"])?;
    Ok(())
}

#[test]
fn owner_reads_enforced_snapshot_with_access_attribution_and_without_an_agent_token() -> TestResult
{
    let ledger = Ledger::new()?;
    ledger.post("2024-03-01", "1000:12.34", "4000:12.34", "USD")?;
    ledger.ok(&["policy", "set", "--enforce", "on"])?;
    let db = Db::open_snapshot(&ledger.dir.path().join("books.db"), None)?;

    let company: CompanySlug = "co".parse()?;
    let current = DateRange::new("2024-03-01".parse()?, "2024-03-31".parse()?)?;
    let previous = DateRange::new("2024-02-01".parse()?, "2024-02-29".parse()?)?;
    let dates = (SummaryWindow::Exact(current), Some(previous)).into();
    let request = || -> Result<_, Box<dyn Error>> {
        let accounts = SummaryAccounts::from([vec!["1000".parse()?], vec![], vec![]]);
        Ok(SummaryRequest::try_from((
            company.clone(),
            dates,
            Some(Currency::USD),
            accounts,
        ))?)
    };
    let actor = Actor::new("Desktop owner".parse()?);
    assert!(matches!(
        LedgerRequest::Summary(request()?).execute(&db, &actor),
        Err(CliError::Unauthorized(_))
    ));
    let cli = ledger.run(&["summary", "--json"])?;
    assert!(!cli.status.success());
    let denied: Value = serde_json::from_slice(&cli.stderr)?;
    assert_eq!(denied["error"]["code"], "UNAUTHORIZED");

    let owner = OwnerSummaryReader::from_authenticated_owner(&db, actor.label().clone());
    let mut recorded = None;
    let summary = owner.summary(request()?, |read| {
        recorded = Some((
            read.operation(),
            read.actor().as_str().to_owned(),
            read.company().clone(),
        ));
    })?;
    assert_eq!(
        recorded,
        Some((Operation::Summary, "Desktop owner".into(), company))
    );
    assert_eq!(
        summary.period_windows(),
        [
            (current.from(), current.to()),
            (previous.from(), previous.to())
        ]
    );
    assert!(
        summary
            .highlights()
            .iter()
            .any(|line| line.contains("12.34"))
    );
    let changes: i64 = db
        .read()
        .query_row("SELECT total_changes()", [], |row| row.get(0))?;
    assert_eq!(changes, 0);
    Ok(())
}

#[test]
fn invalid_options_fail_with_named_errors_and_custom_comparison_is_respected() -> TestResult {
    let ledger = Ledger::new()?;
    for options in [
        vec!["--from", "2024-03-02", "--to", "2024-03-01"],
        vec!["--to", "2024-02-30"],
        vec!["--currency", "BAD"],
        vec!["--cash-account", "4000"],
        vec!["--receivable-account", "2000"],
        vec!["--payable-account", "1000"],
        vec!["--compare-from", "2024-02-02", "--compare-to", "2024-02-01"],
    ] {
        let mut args = vec!["summary", "--json"];
        args.extend(options);
        let output = ledger.run(&args)?;
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        let value: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(value["meta"]["command"], "summary");
        assert_eq!(value["error"]["code"], "VALIDATION");
    }
    assert_eq!(
        ledger
            .run(&["summary", "--compare-from", "2024-02-01"])?
            .status
            .code(),
        Some(2)
    );
    let report = ledger.summary(&[
        "--from",
        "2024-03-10",
        "--to",
        "2024-03-15",
        "--compare-from",
        "2023-01-01",
        "--compare-to",
        "2023-01-31",
    ])?;
    assert_eq!(report["data"]["periods"]["prior"]["from"], "2023-01-01");
    assert_eq!(report["data"]["periods"]["prior"]["to"], "2023-01-31");
    Ok(())
}

#[test]
fn plain_words_and_csv_include_comparisons_and_optional_view() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2024-03-01", "5000:12.34", "1000:12.34", "USD")?;
    let plain = ledger.ok(&["summary", "--to", "2024-03-31"])?;
    assert!(plain.contains("Spending was USD 12.34"), "{plain}");
    assert!(plain.contains("Optional Desktop view: beankeeper://summary?"));
    let csv = ledger.ok(&["summary", "--to", "2024-03-31", "--format", "csv"])?;
    let rows = csv::Reader::from_reader(csv.as_bytes())
        .records()
        .collect::<Result<Vec<_>, _>>()?;
    let spending = rows
        .iter()
        .find(|row| row.get(0) == Some("comparison") && row.get(1) == Some("spending"))
        .ok_or("spending row")?;
    assert_eq!(spending.get(5), Some("1234"));
    Ok(())
}

#[test]
fn custom_account_selection_is_qualified_and_preserved_in_desktop_links() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.ok(&["account", "create", "1300", "Wallet", "--type", "asset"])?;
    ledger.post("2024-03-01", "1300:25", "3000:25", "USD")?;
    let inferred = ledger.summary(&["--to", "2024-03-31"])?;
    assert!(
        inferred["data"]["highlights"][0]
            .as_str()
            .is_some_and(|text| text.contains("selected accounts"))
    );
    let expected_state = json!({
        "company": "co",
        "currency": "USD",
        "from": "2024-03-01",
        "to": "2024-03-31",
        "compare_from": "2024-02-01",
        "compare_to": "2024-02-29",
        "cash_account": ["1000"],
        "receivable_account": ["1100"],
        "payable_account": ["2000"],
    });
    let inferred_view = &inferred["data"]["desktop_view"];
    assert_eq!(
        inferred_view["link"],
        json!({"view_id": "financial-summary", "state": expected_state}),
    );
    assert_eq!(inferred_view["cash_accounts"], json!(["1000"]));

    let explicit = ledger.summary(&[
        "--to",
        "2024-03-31",
        "--cash-account",
        "1300",
        "--receivable-account",
        "1200",
        "--payable-account",
        "2100",
    ])?;
    let mut expected_state = expected_state;
    expected_state["cash_account"] = json!(["1300"]);
    expected_state["receivable_account"] = json!(["1200"]);
    expected_state["payable_account"] = json!(["2100"]);
    assert_eq!(explicit["data"]["standing"]["cash_on_hand"], 2500);
    let view = &explicit["data"]["desktop_view"];
    assert_eq!(view["cash_accounts"], json!(["1300"]));
    assert_eq!(view["view"], "financial-summary");
    assert_eq!(view["compare_to"], "2024-02-29");
    assert_eq!(view["receivable_accounts"], json!(["1200"]));
    assert_eq!(view["payable_accounts"], json!(["2100"]));
    assert_eq!(
        view["link"],
        json!({"view_id": "financial-summary", "state": expected_state}),
    );
    assert!(
        view["uri"]
            .as_str()
            .is_some_and(|uri| uri.contains("&cash_account=1300")
                && uri.contains("&receivable_account=1200")
                && uri.contains("&payable_account=2100")
                && !uri.contains("&cash_account=1000"))
    );
    Ok(())
}

#[test]
fn a_valid_book_with_an_unrepresentable_summary_total_fails_instead_of_wrapping() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.ok(&[
        "account",
        "create",
        "3100",
        "Other owner funds",
        "--type",
        "equity",
    ])?;
    ledger.post(
        "2024-02-01",
        "1000:92233720368547758.07",
        "3000:92233720368547758.07",
        "USD",
    )?;
    ledger.post("2024-02-01", "1200:0.01", "3100:0.01", "USD")?;
    let output = ledger.run(&["summary", "--json", "--to", "2024-03-31"])?;
    assert_eq!(output.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("minor-unit range"))
    );
    Ok(())
}

#[test]
fn statement_attention_names_accounts_and_months_newest_first() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("2023-12-12", "1000:10", "4000:10", "USD")?;
    ledger.post("2024-02-12", "1000:10", "4000:10", "USD")?;
    ledger.post("2024-02-12", "1200:10", "4000:10", "USD")?;
    ledger.post("2024-03-12", "1000:10", "4000:10", "USD")?;
    ledger.post("2024-01-12", "1000:10", "4000:10", "EUR")?;
    let report = ledger.summary(&["--to", "2024-03-31", "--currency", "USD"])?;
    let item = &report["data"]["needs_attention"]["items"]["unreconciled_months"];
    assert_eq!(item["count"], 3);
    let months = item["months"].as_array().ok_or("missing months")?;
    let messages = [
        "Cash (1000, USD) needs its February 2024 statement checked.",
        "Equipment (1200, USD) needs its February 2024 statement checked.",
        "Cash (1000, USD) needs its December 2023 statement checked.",
    ];
    let plain = ledger.ok(&["summary", "--to", "2024-03-31", "--currency", "USD"])?;
    let mut previous = 0;
    for (month, message) in months.iter().zip(messages) {
        assert_eq!(month["message"], message);
        assert!(month["account_name"].is_string());
        let position = plain.find(message).ok_or("missing actionable statement")?;
        assert!(position >= previous);
        previous = position;
    }
    assert_eq!(months[0]["period_end"], "2024-02-29");
    assert!(!plain.contains("3 unreconciled months"));
    let csv = ledger.ok(&[
        "summary",
        "--to",
        "2024-03-31",
        "--currency",
        "USD",
        "--format",
        "csv",
    ])?;
    assert!(csv.contains("statement_check,1000,"));
    assert!(csv.contains("December 2023 statement checked."));
    let empty = ledger.summary(&["--to", "2023-01-01", "--currency", "USD"])?;
    assert_eq!(
        empty["data"]["needs_attention"]["items"]["unreconciled_months"]["months"],
        json!([])
    );
    Ok(())
}
