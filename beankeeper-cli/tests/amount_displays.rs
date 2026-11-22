//! Monetary presentation across the CLI's report and transaction contracts.

use std::{error::Error, path::Path, process::Command};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::tempdir;

fn run(db: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("bk")?
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("--db")
        .arg(db)
        .arg("--json")
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn every_report_and_transaction_uses_its_currency_for_decimal_strings() -> Result<(), Box<dyn Error>>
{
    for (currency, minor, major, zero) in [
        ("USD", 180_000, "1800.00", "0.00"),
        ("JPY", 1800, "1800", "0"),
        ("BHD", 1_800_000, "1800.000", "0.000"),
    ] {
        let dir = tempdir()?;
        let db = dir.path().join("books.db");
        seed(&db, currency)?;
        for args in [vec!["txn", "list"], vec!["txn", "show", "1"]] {
            let mut full = vec!["--company", "co"];
            full.extend(args);
            let result = run(&db, &full)?;
            let entry = &result["data"][0]["entries"][0];
            assert_eq!(entry["amount"], minor);
            assert_eq!(entry["amount_display"], major);
        }
        run(
            &db,
            &[
                "--company",
                "co",
                "budget",
                "set",
                "1000",
                "--year",
                "2026",
                "--month",
                "3",
                "--amount",
                "1800",
                "--currency",
                currency,
            ],
        )?;
        let budget = run(
            &db,
            &["--company", "co", "budget", "list", "--year", "2026"],
        )?;
        assert_eq!(budget["data"][0]["amount_display"], major);

        verify_reports(&db, currency, minor, major)?;
        verify_columns(&db, major, zero)?;
    }
    Ok(())
}

#[test]
fn typed_budget_variance_retains_currency_and_matches_cli() -> Result<(), Box<dyn Error>> {
    use beankeeper::types::{AccountType, Currency, Money};
    use beankeeper_cli::db::{
        BudgetCoverage, BudgetMonths, BudgetVarianceParams, Db, compute_budget_variance,
    };
    use chrono::Month;

    let dir = tempdir()?;
    let path = dir.path().join("budget.db");
    seed(&path, "BHD")?;
    for currency in ["BHD", "USD"] {
        run(
            &path,
            &[
                "--company",
                "co",
                "budget",
                "set",
                "1000",
                "--year",
                "2026",
                "--month",
                "3",
                "--amount",
                "2000",
                "--currency",
                currency,
            ],
        )?;
    }
    let db = Db::open_snapshot(&path, None)?;
    let currency = Currency::BHD;
    let report = compute_budget_variance(
        &db,
        &BudgetVarianceParams {
            company_slug: "co",
            currency,
            months: BudgetMonths::new(2026, Month::March, Month::March)?,
            account_type: Some(AccountType::Asset),
            coverage: BudgetCoverage::Budgeted,
        },
    )?;
    assert_eq!(report.currency(), currency);
    assert_eq!(
        report.total_budget(),
        Money::from_minor(2_000_000, currency)
    );
    assert_eq!(
        report.total_actual(),
        Money::from_minor(1_800_000, currency)
    );
    let [line] = report.rows() else {
        return Err("expected one budgeted asset".into());
    };
    assert_eq!(line.account_type, AccountType::Asset);
    assert_eq!(line.variance_amount, Money::from_minor(-200_000, currency));
    let typed_data = serde_json::to_value(&report)?;
    assert_eq!(typed_data["lines"][0]["account_type"], "asset");
    assert_eq!(typed_data["lines"][0]["variance_percent"], -10.0);
    assert_eq!(typed_data["lines"][0]["variance_display"], "-200.000");
    let cli = run(
        &path,
        &[
            "--company",
            "co",
            "report",
            "budget-variance",
            "--year",
            "2026",
            "--month",
            "3",
            "--type",
            "asset",
            "--currency",
            "BHD",
        ],
    )?;
    assert_eq!(cli["data"], typed_data);
    Ok(())
}

#[test]
fn typed_statements_share_money_and_cli_output() -> Result<(), Box<dyn Error>> {
    use beankeeper::types::{AccountType, Currency, Money};
    use beankeeper_cli::commands::report::{
        DateRange, compute_balance_sheet, compute_income_statement,
    };
    use beankeeper_cli::db::Db;
    use chrono::NaiveDate;

    let dir = tempdir()?;
    let path = dir.path().join("statements.db");
    seed(&path, "BHD")?;
    let db = Db::open_snapshot(&path, None)?;
    let start = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d")?;
    let end = NaiveDate::parse_from_str("2026-03-31", "%Y-%m-%d")?;
    let dates = DateRange::new(start, end)?;
    let currency = Currency::BHD;
    let proceeds = Money::from_minor(1_800_000, currency);

    let income = compute_income_statement(&db, "co", dates, currency)?;
    assert_eq!(income.dates(), dates);
    assert_eq!(income.currency(), currency);
    assert_eq!(income.revenue().kind(), AccountType::Revenue);
    assert_eq!(income.revenue().total(), proceeds);
    assert_eq!(income.net_income(), proceeds);
    let income_json = serde_json::to_value(&income)?;
    assert_eq!(income_json["sections"][0]["normal_balance"], "credit");
    assert_eq!(income_json["net_income_display"], "1800.000");
    let cli_income = run(
        &path,
        &[
            "--company",
            "co",
            "report",
            "income-statement",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
            "--currency",
            "BHD",
        ],
    )?;
    assert_eq!(income_json, cli_income["data"]);

    let sheet = compute_balance_sheet(&db, "co", end, currency)?;
    assert!(sheet.balanced());
    assert_eq!(sheet.as_of(), end);
    assert_eq!(sheet.assets().total(), proceeds);
    assert_eq!(sheet.computed_earnings(), proceeds);
    assert!(sheet.equity().accounts()[0].is_computed());
    let sheet_json = serde_json::to_value(&sheet)?;
    assert_eq!(sheet_json["computed_earnings_display"], "1800.000");
    let cli_sheet = run(
        &path,
        &[
            "--company",
            "co",
            "report",
            "balance-sheet",
            "--to",
            "2026-03-31",
            "--currency",
            "BHD",
        ],
    )?;
    assert_eq!(sheet_json, cli_sheet["data"]);
    Ok(())
}

#[test]
fn typed_cash_flow_reuses_currency_scope_and_cli_payload() -> Result<(), Box<dyn Error>> {
    use beankeeper::types::{AccountCode, AccountType, Currency, Money};
    use beankeeper_cli::commands::report::DateRange;
    use beankeeper_cli::commands::report_cash_flow::{Section, compute_cash_flow};
    use beankeeper_cli::db::Db;
    use chrono::NaiveDate;

    let dir = tempdir()?;
    let path = dir.path().join("cash-flow.db");
    seed(&path, "BHD")?;
    let db = Db::open_snapshot(&path, None)?;
    let from = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d")?;
    let to = NaiveDate::parse_from_str("2026-03-31", "%Y-%m-%d")?;
    let dates = DateRange::new(from, to)?;
    let currency = Currency::BHD;
    let report = compute_cash_flow(&db, "co", dates, currency, &[AccountCode::new("1000")?])?;
    let proceeds = Money::from_minor(1_800_000, currency);
    assert_eq!(report.dates(), dates);
    assert_eq!(report.currency(), currency);
    assert_eq!(report.cash_accounts()[0].code, "1000");
    assert_eq!(report.net_change(), proceeds);
    assert_eq!(report.closing_cash(), proceeds);
    assert!(report.reconciled());
    let [line] = report.section(Section::Operating).lines() else {
        return Err("expected one cash-flow counter-account".into());
    };
    assert_eq!(line.account_type(), AccountType::Revenue);
    assert_eq!(line.amount(), proceeds);
    let typed_data = serde_json::to_value(&report)?;
    assert_eq!(typed_data["operating"]["total_display"], "1800.000");
    let cli = run(
        &path,
        &[
            "--company",
            "co",
            "report",
            "cash-flow",
            "--cash-account",
            "1000",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
            "--currency",
            "BHD",
        ],
    )?;
    assert_eq!(cli["data"], typed_data);
    Ok(())
}

#[test]
fn typed_trend_uses_report_dates_and_currency_in_every_value() -> Result<(), Box<dyn Error>> {
    use beankeeper::types::{Currency, Money};
    use beankeeper_cli::commands::report::DateRange;
    use beankeeper_cli::commands::report_periods::PeriodArg;
    use beankeeper_cli::commands::report_trend::{TrendMetric, TrendRequest, compute_trend};
    use beankeeper_cli::db::Db;
    use chrono::NaiveDate;

    let dir = tempdir()?;
    let path = dir.path().join("trends.db");
    seed(&path, "BHD")?;
    let db = Db::open_snapshot(&path, None)?;
    let from = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d")?;
    let to = NaiveDate::parse_from_str("2026-04-30", "%Y-%m-%d")?;
    let dates = DateRange::new(from, to)?;
    let currency = Currency::BHD;
    let request = TrendRequest {
        dates,
        currency,
        metric: TrendMetric::NetWorth,
        period: PeriodArg::Month,
    };
    let report = compute_trend(&db, "co", request)?;
    assert_eq!(report.dates(), dates);
    assert_eq!(report.currency(), currency);
    assert_eq!(report.columns()[0].label, "2026-03");
    let net_worth = report.series().last().ok_or("missing net worth")?;
    assert_eq!(net_worth.name(), "Net Worth");
    assert_eq!(
        net_worth.amounts(),
        &[Money::from_minor(1_800_000, currency); 2]
    );
    let typed_data = serde_json::to_value(&report)?;
    assert_eq!(
        typed_data["series"][2]["amounts_display"],
        serde_json::json!(["1800.000", "1800.000"])
    );
    let cli = run(
        &path,
        &[
            "--company",
            "co",
            "report",
            "trend",
            "--metric",
            "net-worth",
            "--from",
            "2026-03-01",
            "--to",
            "2026-04-30",
            "--currency",
            "BHD",
        ],
    )?;
    assert_eq!(cli["data"], typed_data);
    Ok(())
}

fn seed(db: &Path, currency: &str) -> Result<(), Box<dyn Error>> {
    run(db, &["init"])?;
    run(db, &["company", "create", "co", "Company"])?;
    for (code, name, kind) in [("1000", "Cash", "asset"), ("4000", "Sales", "revenue")] {
        run(
            db,
            &[
                "--company",
                "co",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ],
        )?;
    }
    run(
        db,
        &[
            "--company",
            "co",
            "txn",
            "post",
            "-d",
            "Sale",
            "--date",
            "2026-03-01",
            "--debit",
            "1000:1800",
            "--credit",
            "4000:1800",
            "--currency",
            currency,
            "--reference",
            "sale",
            "--tax",
            "4000=sales",
        ],
    )?;
    Ok(())
}

fn verify_reports(
    db: &Path,
    currency: &str,
    minor: i64,
    major: &str,
) -> Result<(), Box<dyn Error>> {
    let cases: &[(&[&str], &str)] = &[
        (&["trial-balance"], "/total_debits"),
        (&["balance", "--account", "1000"], "/debit_total"),
        (&["income-statement"], "/total_revenue"),
        (&["balance-sheet"], "/total_assets"),
        (&["tax-summary"], "/0/credit_total"),
        (
            &[
                "budget-variance",
                "--year",
                "2026",
                "--type",
                "asset",
                "--currency",
                currency,
            ],
            "/totals/actual",
        ),
        (
            &["ledger", "--account", "1000"],
            "/accounts/0/closing_balance",
        ),
        (
            &[
                "aging",
                "--account",
                "1000",
                "--as-of",
                "2026-03-31",
                "--detail",
            ],
            "/totals/total_open",
        ),
        (
            &["reconciliation", "--account", "1000", "--to", "2026-03-31"],
            "/book_balance",
        ),
        (&["cash-flow"], "/net_change"),
    ];
    for (args, pointer) in cases {
        let mut full = vec!["--company", "co", "report"];
        full.extend_from_slice(args);
        if !matches!(args[0], "aging" | "reconciliation" | "budget-variance") {
            full.extend(["--to", "2026-03-31"]);
        }
        let result = run(db, &full)?;
        let data = &result["data"];
        assert_eq!(
            data.pointer(pointer),
            Some(&Value::from(minor)),
            "{args:?}: {data}"
        );
        assert_eq!(
            data.pointer(&format!("{pointer}_display")),
            Some(&Value::from(major)),
            "{args:?}: {data}"
        );
    }
    Ok(())
}

fn verify_columns(db: &Path, major: &str, zero: &str) -> Result<(), Box<dyn Error>> {
    let periods = run(
        db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2026-03-01",
            "--to",
            "2026-04-30",
            "--period",
            "month",
        ],
    )?;
    assert_eq!(
        periods["data"]["accounts"][0]["amounts_display"],
        serde_json::json!([major, zero])
    );
    let comparison = run(
        db,
        &[
            "--company",
            "co",
            "report",
            "trial-balance",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
            "--compare",
            "prior-period",
        ],
    )?;
    assert_eq!(comparison["data"]["accounts"][0]["current_display"], major);
    assert_eq!(comparison["data"]["accounts"][0]["prior_display"], zero);
    Ok(())
}
