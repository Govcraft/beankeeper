use std::path::Path;

use assert_cmd::prelude::*;
use serde_json::{Value, json};
use tempfile::tempdir;

use super::{Client, TestResult, bk, payload, seed};

fn add_company(path: &Path, slug: &str) -> TestResult {
    bk(path)?
        .args(["company", "create", slug, slug])
        .assert()
        .success();
    for (code, name, kind) in [("1000", "Cash", "asset"), ("4000", "Receipts", "revenue")] {
        bk(path)?
            .args([
                "--company",
                slug,
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ])
            .assert()
            .success();
    }
    Ok(())
}

fn post(
    path: &Path,
    slug: &str,
    date: &str,
    debit: &str,
    credit: &str,
    extra: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = bk(path)?
        .args([
            "--json",
            "--company",
            slug,
            "txn",
            "post",
            "-d",
            "Group report fixture",
            "--date",
            date,
            "--debit",
            debit,
            "--credit",
            credit,
        ])
        .args(extra)
        .assert()
        .success();
    let parsed: Value = serde_json::from_slice(&output.get_output().stdout)?;
    Ok(parsed["data"]["id"]
        .as_i64()
        .ok_or("journal ID")?
        .to_string())
}

const MEMBERS: [&str; 6] = ["--member", "beta", "--member", "acme", "--currency", "USD"];

fn read_cli(
    path: &Path,
    statement: &str,
    dates: &[&str],
) -> Result<Value, Box<dyn std::error::Error>> {
    let output = bk(path)?
        .args(["--json", "group-report", statement])
        .args(MEMBERS)
        .args(dates)
        .assert()
        .success();
    Ok(serde_json::from_slice(&output.get_output().stdout)?)
}

fn read_only_client(path: &Path) -> Result<Client, Box<dyn std::error::Error>> {
    let mut command = bk(path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    Ok(client)
}

fn seed_group(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    seed(path)?;
    add_company(path, "beta")?;
    add_company(path, "outside")?;
    bk(path)?
        .args([
            "--company",
            "beta",
            "account",
            "create",
            "5000",
            "Purchases",
            "--type",
            "expense",
        ])
        .assert()
        .success();

    post(path, "acme", "2025-12-31", "1000:10", "4000:10", &[])?;
    post(path, "beta", "2026-01-01", "1000:200", "4000:200", &[])?;
    let sale = post(path, "acme", "2026-01-31", "1000:30", "4000:30", &[])?;
    let purchase = post(
        path,
        "beta",
        "2026-01-31",
        "5000:30",
        "1000:30",
        &["--correlate", &sale],
    )?;
    let outside = post(path, "outside", "2026-01-15", "1000:500", "4000:500", &[])?;
    let linked_to_outside = post(
        path,
        "acme",
        "2026-01-15",
        "1000:5",
        "4000:5",
        &["--correlate", &outside],
    )?;
    let deferred = post(path, "beta", "2026-02-01", "1000:3", "4000:3", &[])?;
    let linked_after_cutoff = post(
        path,
        "acme",
        "2026-01-15",
        "1000:3",
        "4000:3",
        &["--correlate", &deferred],
    )?;

    // Earlier ledgers retained a time suffix on some posting dates.
    rusqlite::Connection::open(path)?.execute(
        "UPDATE transactions SET date = ?1 WHERE id = ?2",
        rusqlite::params!["2026-01-31T16:00:00Z", sale.parse::<i64>()?],
    )?;

    Ok(json!([
        {"status":"paired","source":{"company":"acme","transaction_id":sale.parse::<i64>()?,"date":"2026-01-31"},
            "partner":{"company":"beta","transaction_id":purchase.parse::<i64>()?,"date":"2026-01-31"}},
        {"status":"no_reciprocal_in_scope","source":{"company":"acme","transaction_id":linked_to_outside.parse::<i64>()?,"date":"2026-01-15"}},
        {"status":"no_reciprocal_in_scope","source":{"company":"acme","transaction_id":linked_after_cutoff.parse::<i64>()?,"date":"2026-01-15"}}
    ]))
}

fn check_text_output(path: &Path, dates: &[&str]) -> TestResult {
    let csv = bk(path)?
        .args(["--format", "csv", "group-report", "income-statement"])
        .args(MEMBERS)
        .args(dates)
        .assert()
        .success();
    let output = String::from_utf8_lossy(&csv.get_output().stdout);
    assert!(output.contains("Combined before eliminations,,,,net_income,208.00"));
    assert!(output.contains("Company,acme,revenue,4000,Sales,38.00"));
    assert!(output.contains("Company,beta,revenue,4000,Receipts,200.00"));
    let table = bk(path)?
        .args(["group-report", "income-statement"])
        .args(MEMBERS)
        .args(dates)
        .assert()
        .success();
    let output = String::from_utf8_lossy(&table.get_output().stdout);
    assert!(output.contains("Combined before eliminations"));
    assert!(output.contains("Group income statement"));
    Ok(())
}
#[test]
fn group_reports_match_read_only_mcp_and_keep_company_details_separate() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let expected_links = seed_group(&path)?;
    let mut client = read_only_client(&path)?;

    let dates = ["--from", "2026-01-01", "--to", "2026-01-31"];
    let income = read_cli(&path, "income-statement", &dates)?;
    let arguments =
        json!({"members":["beta","acme"],"currency":"USD","from":"2026-01-01","to":"2026-01-31"});
    assert_eq!(income["meta"]["command"], "group-report.income-statement");
    assert!(income["meta"].get("company").is_none());
    let data = &income["data"];
    assert_eq!(*data, client.success("group_income_statement", &arguments)?);
    assert_eq!(data["currency"], "USD");
    assert_eq!(data["kind"], "income_statement");
    assert_eq!(data["from"], "2026-01-01");
    assert_eq!(data["to"], "2026-01-31");
    assert_eq!(data["combined_before_eliminations"]["total_revenue"], 23800);
    assert_eq!(data["combined_before_eliminations"]["total_expenses"], 3000);
    assert_eq!(data["combined_before_eliminations"]["net_income"], 20800);
    assert_eq!(data["members"][0]["company"], "acme");
    assert_eq!(
        data["members"][0]["statement"]["sections"][0]["accounts"][0]["code"],
        "4000"
    );
    assert_eq!(
        data["members"][0]["statement"]["sections"][0]["accounts"][0]["name"],
        "Sales"
    );
    assert_eq!(data["members"][1]["company"], "beta");
    assert_eq!(
        data["members"][1]["statement"]["sections"][0]["accounts"][0]["code"],
        "4000"
    );
    assert_eq!(
        data["members"][1]["statement"]["sections"][0]["accounts"][0]["name"],
        "Receipts"
    );
    assert_eq!(data["intercompany_links"], expected_links);

    let balance = read_cli(&path, "balance-sheet", &["--as-of", "2026-01-31"])?;
    let data = &balance["data"];
    assert_eq!(
        *data,
        client.success(
            "group_balance_sheet",
            &json!({"members":["acme","beta"],"currency":"USD","as_of":"2026-01-31"}),
        )?
    );
    assert_eq!(data["as_of"], "2026-01-31");
    let combined = &data["combined_before_eliminations"];
    assert_eq!(combined["total_assets"], 21800);
    assert_eq!(combined["computed_earnings"], 21800);
    assert_eq!(combined["total_liabilities_and_equity"], 21800);
    assert_eq!(combined["difference"], 0);
    assert_eq!(combined["balanced"], true);

    check_text_output(&path, &dates)
}

#[test]
fn group_reports_reject_invalid_members_dates_and_mixed_currencies() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    add_company(&path, "beta")?;
    post(
        &path,
        "acme",
        "2026-01-01",
        "1000:5",
        "4000:5",
        &["--currency", "MXN"],
    )?;

    let mut client = read_only_client(&path)?;
    for (members, as_of, code, message) in [
        (json!(["acme"]), "2026-01-01", "VALIDATION", "at least two"),
        (
            json!(["acme", "acme"]),
            "2026-01-01",
            "VALIDATION",
            "more than once",
        ),
        (
            json!(["absent", "beta"]),
            "2026-01-01",
            "NOT_FOUND",
            "absent",
        ),
        (json!(["acme", "beta"]), "2026-01-01", "VALIDATION", "MXN"),
    ] {
        let response = client.call(
            "group_balance_sheet",
            &json!({
                "members":members,"as_of":as_of,"currency":"USD"
            }),
        )?;
        assert_eq!(response["result"]["isError"], true);
        let error = payload(&response)?;
        assert_eq!(error["code"], code);
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|actual| actual.contains(message)),
            "{error}"
        );
    }

    let args = ["--from", "2026-01-02", "--to", "2026-01-03"];
    assert_eq!(
        read_cli(&path, "income-statement", &args)?["data"]["combined_before_eliminations"]["net_income"],
        0
    );
    let response = client.call(
        "group_income_statement",
        &json!({
            "members":["acme","beta"],"from":"2026-01-02","to":"2026-01-01","currency":"USD"
        }),
    )?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "VALIDATION");

    bk(&path)?
        .args([
            "setup",
            "book-initialize",
            "beta",
            "--reference",
            "group-test-profile",
            "--purpose",
            "business",
            "--base-currency",
            "EUR",
            "--fiscal-year-start",
            "01-01",
        ])
        .assert()
        .success();
    let response = client.call(
        "group_income_statement",
        &json!({
            "members":["acme","beta"],"from":"2026-01-02","to":"2026-01-03","currency":"USD"
        }),
    )?;
    assert_eq!(response["result"]["isError"], true);
    let error = payload(&response)?;
    assert_eq!(error["code"], "VALIDATION");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("functional currency EUR"))
    );
    Ok(())
}

#[test]
fn group_reports_preserve_totals_wider_than_a_signed_64_bit_integer() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    add_company(&path, "beta")?;
    for company in ["acme", "beta"] {
        bk(&path)?
            .args([
                "--company",
                company,
                "account",
                "create",
                "5000",
                "Expense",
                "--type",
                "expense",
            ])
            .assert()
            .success();
        post(
            &path,
            company,
            "2026-01-01",
            "5000:60000000000000000",
            "1000:60000000000000000",
            &[],
        )?;
    }

    let mut client = read_only_client(&path)?;
    let data = client.success(
        "group_income_statement",
        &json!({
            "members":["acme","beta"],"currency":"USD","from":"2026-01-01","to":"2026-01-01"
        }),
    )?;
    assert_eq!(
        data,
        read_cli(
            &path,
            "income-statement",
            &["--from", "2026-01-01", "--to", "2026-01-01"],
        )?["data"]
    );
    let total = &data["combined_before_eliminations"]["net_income"];
    assert!(total.is_number());
    assert_eq!(
        serde_json::from_value::<i128>(total.clone())?,
        -12_000_000_000_000_000_000
    );
    Ok(())
}
