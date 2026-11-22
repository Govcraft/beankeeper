//! Scheduled obligations and explicit, retry-safe posting through the CLI.

use beankeeper_cli::db::CreateAccountParams;
use beankeeper_cli::identity::{Admin, Granted};
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper::types::Currency;
use beankeeper_cli::db::{self, Actor, Db, accounts, companies, transactions};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("schedules.db");
        let db = Db::open(&path, None)?;
        for company in ["acme", "other"] {
            Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                companies::create_company(grant, &company.parse()?, &company.parse()?, None)
            })?;
            for (code, kind, tax) in [
                ("1000", beankeeper::types::AccountType::Asset, None),
                (
                    "5000",
                    beankeeper::types::AccountType::Expense,
                    Some("default-expense"),
                ),
                (
                    "6000",
                    beankeeper::types::AccountType::Expense,
                    Some("split-expense"),
                ),
            ] {
                Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                    accounts::create_account(
                        grant,
                        &CreateAccountParams {
                            company_slug: &company.parse()?,
                            code,
                            name: code,
                            account_type: kind,
                            default_tax_category: tax,
                        },
                    )
                })?;
            }
        }
        Ok(Self { _dir: dir, path })
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .args(["--actor", "schedule-test"])
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_CURRENCY")
            .env_remove("BEANKEEPER_COMPANY")
            .env_remove("BK_AGENT_TOKEN");
        Ok(command)
    }

    fn run(&self, company: &str, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        data(
            &self
                .command()?
                .args(["--json", "--company", company])
                .args(args)
                .output()?,
        )
    }

    fn create(
        &self,
        company: &str,
        start: &str,
        entries: (&str, &str),
        extra: &[&str],
    ) -> Result<Value, Box<dyn Error>> {
        let mut args = vec![
            "schedule",
            "create",
            "--description",
            "Rent",
            "--start",
            start,
            "--debit",
            entries.0,
            "--credit",
            entries.1,
        ];
        args.extend_from_slice(extra);
        self.run(company, &args)
    }

    fn db(&self) -> Result<Db, Box<dyn Error>> {
        Ok(Db::open_existing(&self.path, None)?)
    }

    fn count(&self, table: &str) -> Result<i64, Box<dyn Error>> {
        Ok(self
            .db()?
            .read()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }
}

fn envelope(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(envelope["ok"], true);
    Ok(envelope)
}

fn data(output: &Output) -> Result<Value, Box<dyn Error>> {
    Ok(envelope(output)?["data"].clone())
}

#[test]
fn monthly_pending_preview_and_posting_preserve_calendar_and_accounting() -> TestResult {
    let ledger = Ledger::new()?;
    let created = ledger.create(
        "acme",
        "2026-01-31",
        ("5000:12.50:Office rent", "1000:12.50"),
        &[
            "--frequency",
            "monthly",
            "--end",
            "2026-03-31",
            "--tax",
            "5000=rent",
            "--meta",
            "lease=office",
            "--metadata",
            "{\"building\":\"north\"}",
        ],
    )?;
    assert_eq!(created["next_due"], "2026-01-31");
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_str().ok_or("missing schedule ID")?;
    assert!(id.starts_with("sched_"));
    assert_eq!(ledger.count("transactions")?, 0);
    let db = ledger.db()?;
    let scope = beankeeper_cli::db::companies::Company::bind(db.read(), "acme".parse()?)?;
    for row in db::compute_trial_balance(&scope, None, None, None, Currency::USD)? {
        assert_eq!((row.debit_total, row.credit_total), (0, 0));
    }
    let pending = ledger.run("acme", &["schedule", "list"])?;
    assert_eq!(pending[0]["id"], id);
    assert_eq!(pending[0]["next_due"], "2026-01-31");
    assert_eq!(
        ledger.run("acme", &["schedule", "list", "--due-through", "2026-01-30"])?,
        json!([])
    );
    let audits = ledger.count("audit_log")?;
    let preview = ledger.run(
        "acme",
        &["schedule", "run", "--through", "2026-03-31", "--dry-run"],
    )?;
    let expected_dates = ["2026-01-31", "2026-02-28", "2026-03-31"];
    assert_eq!(preview.as_array().map(Vec::len), Some(3));
    for (row, date) in preview
        .as_array()
        .ok_or("expected preview array")?
        .iter()
        .zip(expected_dates)
    {
        assert_eq!(row["date"], date);
        assert!(row["transaction_id"].is_null());
    }
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    assert_eq!(ledger.run("acme", &["schedule", "list"])?, pending);
    let posted = ledger.run("acme", &["schedule", "run", "--through", "2026-03-31"])?;
    assert_eq!(posted.as_array().map(Vec::len), Some(3));
    for (row, date) in posted
        .as_array()
        .ok_or("expected posting array")?
        .iter()
        .zip(expected_dates)
    {
        let transaction_id = row["transaction_id"]
            .as_i64()
            .ok_or("missing transaction ID")?;
        let (txn, entries) = transactions::get_transaction(&scope, transaction_id.try_into()?)?;
        assert_eq!(txn.date.to_string(), date);
        assert_eq!(txn.description, "Rent");
        assert_eq!(txn.posted_by.as_deref(), Some("schedule-test"));
        let metadata: Value =
            serde_json::from_str(txn.metadata.as_deref().ok_or("missing metadata")?)?;
        assert_eq!(metadata["lease"], "office");
        assert_eq!(metadata["building"], "north");
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.amount.minor_units() == 1250
                    && entry.status == beankeeper_cli::db::EntryStatus::Uncleared)
        );
        let debit = entries
            .iter()
            .find(|entry| entry.direction.is_debit())
            .ok_or("missing debit")?;
        assert_eq!(debit.memo.as_deref(), Some("Office rent"));
        assert_eq!(debit.tax_category.as_deref(), Some("rent"));
    }
    assert_eq!(
        ledger.run("acme", &["schedule", "run", "--through", "2026-03-31"])?,
        json!([])
    );
    assert_eq!(ledger.count("transactions")?, 3);
    assert_eq!(ledger.run("acme", &["schedule", "list"])?, json!([]));
    let all = ledger.run("acme", &["schedule", "list", "--all"])?;
    assert_eq!(all[0]["status"], "completed");
    assert!(all[0]["next_due"].is_null());
    ledger.run("acme", &["verify"])?;
    Ok(())
}

#[test]
fn one_time_splits_and_default_tax_categories_post_only_once() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create(
        "acme",
        "2026-04-10",
        ("5000:8.25", "1000:12.50"),
        &["--debit", "6000:4.25:Split"],
    )?;
    assert_eq!(
        ledger.run("acme", &["schedule", "run", "--through", "2026-04-09"])?,
        json!([])
    );
    let posted = ledger.run("acme", &["schedule", "run", "--through", "2026-04-10"])?;
    assert_eq!(posted.as_array().map(Vec::len), Some(1));
    let id = posted[0]["transaction_id"]
        .as_i64()
        .ok_or("missing transaction ID")?;
    let (_, entries) = transactions::get_transaction(
        &beankeeper_cli::db::companies::Company::bind(ledger.db()?.read(), "acme".parse()?)?,
        id.try_into()?,
    )?;
    assert_eq!(entries.len(), 3);
    for (account, amount, tax) in [
        ("5000", 825, "default-expense"),
        ("6000", 425, "split-expense"),
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry.account_code == account)
            .ok_or("missing split")?;
        assert_eq!(entry.amount.minor_units(), amount);
        assert_eq!(entry.tax_category.as_deref(), Some(tax));
    }
    assert_eq!(
        ledger.run("acme", &["schedule", "run", "--through", "2027-01-01"])?,
        json!([])
    );
    assert_eq!(ledger.count("transactions")?, 1);
    ledger.run("acme", &["verify"])?;
    Ok(())
}

#[test]
fn cancellation_and_company_scoping_preserve_posted_history() -> TestResult {
    let ledger = Ledger::new()?;
    let created = ledger.create(
        "acme",
        "2026-01-01",
        ("5000:5", "1000:5"),
        &["--frequency", "daily"],
    )?;
    let id = created["id"].as_str().ok_or("missing schedule ID")?;
    assert_eq!(
        ledger.run("other", &["schedule", "list", "--all"])?,
        json!([])
    );
    assert_eq!(
        ledger.run("other", &["schedule", "run", "--through", "2026-01-02"])?,
        json!([])
    );
    let wrong_company = ledger
        .command()?
        .args(["--json", "--company", "other", "schedule", "cancel", id])
        .output()?;
    assert_eq!(wrong_company.status.code(), Some(5));
    ledger.run("acme", &["schedule", "run", "--through", "2026-01-01"])?;
    let cancelled = ledger.run("acme", &["schedule", "cancel", id])?;
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        ledger.run("acme", &["schedule", "run", "--through", "2026-02-01"])?,
        json!([])
    );
    assert_eq!(ledger.count("transactions")?, 1);
    assert_eq!(ledger.run("acme", &["schedule", "list"])?, json!([]));
    assert_eq!(
        ledger.run("acme", &["schedule", "list", "--all"])?[0]["status"],
        "cancelled"
    );
    let audits = ledger.count("audit_log")?;
    ledger.run("acme", &["schedule", "cancel", id])?;
    assert_eq!(ledger.count("audit_log")?, audits);
    ledger.run("acme", &["verify"])?;
    Ok(())
}

#[test]
fn invalid_templates_and_missing_identifiers_leave_no_state() -> TestResult {
    let ledger = Ledger::new()?;
    let audits = ledger.count("audit_log")?;
    for (start, debit, credit, extra) in [
        ("2026-01-01", "5000:10", "1000:9", vec![]),
        ("2026-01-01", "missing:10", "1000:10", vec![]),
        ("2026-01-01", "5000:-10", "1000:-10", vec![]),
        ("2026-01-01", "5000:0", "1000:0", vec![]),
        ("2026-01-01", "5000:1.001", "1000:1.001", vec![]),
        ("2026-02-30", "5000:10", "1000:10", vec![]),
        (
            "2026-01-01",
            "5000:10",
            "1000:10",
            vec!["--end", "2025-12-31"],
        ),
        (
            "2026-01-01",
            "5000:10",
            "1000:10",
            vec!["--frequency", "fortnightly"],
        ),
        (
            "2026-01-01",
            "5000:10",
            "1000:10",
            vec!["--currency", "BAD"],
        ),
    ] {
        let mut command = ledger.command()?;
        command
            .args([
                "--json",
                "--company",
                "acme",
                "schedule",
                "create",
                "--description",
                "Invalid",
                "--start",
                start,
                "--debit",
                debit,
                "--credit",
                credit,
            ])
            .args(extra);
        assert!(
            !command.output()?.status.success(),
            "accepted {start} {debit} {credit}"
        );
    }
    for id in [
        "bad-id",
        "rule_01h455vb4pex5vsknk084sn02q",
        "sched_01h455vb4pex5vsknk084sn02q",
    ] {
        assert!(
            !ledger
                .command()?
                .args(["--json", "--company", "acme", "schedule", "cancel", id])
                .output()?
                .status
                .success()
        );
    }
    assert!(
        !ledger
            .command()?
            .args(["--json", "--company", "missing", "schedule", "list"])
            .output()?
            .status
            .success()
    );
    assert_eq!(
        ledger.run("acme", &["schedule", "list", "--all"])?,
        json!([])
    );
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    Ok(())
}

#[test]
fn currency_precision_is_preserved_in_scheduled_postings() -> TestResult {
    let ledger = Ledger::new()?;
    for (currency, debit, credit, expected, display) in [
        ("JPY", "5000:50", "1000:50", 50, "50"),
        ("BHD", "5000:1.234", "1000:1.234", 1234, "1.234"),
    ] {
        let template = ledger.create(
            "acme",
            "2026-01-01",
            (debit, credit),
            &["--currency", currency],
        )?;
        assert_eq!(template["currency"], currency);
        for line in template["entries"].as_array().ok_or("schedule entries")? {
            assert_eq!(line["amount"], expected);
            assert_eq!(line["amount_display"], display);
        }
        let posted = ledger.run("acme", &["schedule", "run", "--through", "2026-01-01"])?;
        let id = posted[0]["transaction_id"]
            .as_i64()
            .ok_or("missing transaction ID")?;
        let (txn, entries) = transactions::get_transaction(
            &beankeeper_cli::db::companies::Company::bind(ledger.db()?.read(), "acme".parse()?)?,
            id.try_into()?,
        )?;
        assert_eq!(txn.currency.code(), currency);
        assert!(
            entries
                .iter()
                .all(|entry| entry.amount.minor_units() == expected)
        );
    }
    Ok(())
}

#[test]
fn output_formats_quiet_and_command_scoped_company_work() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("acme", "2026-01-01", ("5000:10", "1000:10"), &[])?;
    let scoped = envelope(
        &ledger
            .command()?
            .args(["schedule", "list", "--company", "acme", "--json", "--quiet"])
            .output()?,
    )?;
    assert_eq!(scoped["meta"]["command"], "schedule.list");
    assert_eq!(scoped["meta"]["company"], "acme");
    assert_eq!(scoped["data"].as_array().map(Vec::len), Some(1));
    let csv = ledger
        .command()?
        .args(["--company", "acme", "--format", "csv", "schedule", "list"])
        .output()?;
    assert!(csv.status.success());
    let csv = String::from_utf8(csv.stdout)?;
    assert!(csv.starts_with("id,description,currency,frequency,next_due,status\n"));
    assert!(csv.contains("Rent,USD,once,2026-01-01,pending"));
    let table = ledger
        .command()?
        .args(["--company", "acme", "schedule", "list"])
        .output()?;
    assert!(table.status.success());
    assert!(String::from_utf8(table.stdout)?.contains("Rent"));
    let quiet = ledger
        .command()?
        .args([
            "--company",
            "acme",
            "--quiet",
            "schedule",
            "run",
            "--through",
            "2026-01-01",
            "--dry-run",
        ])
        .output()?;
    assert!(quiet.status.success());
    assert_eq!(quiet.stdout, [] as [u8; 0]);
    assert_eq!(quiet.stderr, [] as [u8; 0]);
    assert_eq!(ledger.count("transactions")?, 0);
    Ok(())
}

#[test]
fn pending_schedule_prevents_account_deletion() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("acme", "2026-01-01", ("5000:10", "1000:10"), &[])?;
    let audits = ledger.count("audit_log")?;
    let blocked = ledger
        .command()?
        .args([
            "--json",
            "--company",
            "acme",
            "account",
            "delete",
            "5000",
            "--force",
        ])
        .output()?;
    assert_eq!(blocked.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("schedule"));
    assert!(
        accounts::get_account(
            &beankeeper_cli::db::companies::Company::bind(ledger.db()?.read(), "acme".parse()?)?,
            "5000"
        )
        .is_ok()
    );
    assert_eq!(ledger.count("audit_log")?, audits);
    Ok(())
}

#[test]
fn oversized_runs_require_override_and_previews_show_the_count() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create(
        "acme",
        "2020-01-01",
        ("5000:10", "1000:10"),
        &["--frequency", "daily", "--end", "2022-09-27"],
    )?;
    let pending = ledger.run("acme", &["schedule", "list", "--all"])?;
    let audits = ledger.count("audit_log")?;
    for format in ["table", "csv", "json"] {
        let preview = ledger
            .command()?
            .args([
                "--company",
                "acme",
                "--format",
                format,
                "schedule",
                "run",
                "--through",
                "2022-09-27",
                "--dry-run",
            ])
            .output()?;
        assert!(preview.status.success());
        let message = String::from_utf8(preview.stderr.clone())?;
        assert!(message.contains("Preview: 1001 due occurrences"));
        assert!(message.contains("no transactions posted or schedules advanced"));
        if format == "json" {
            let rows = data(&preview)?;
            let rows = rows.as_array().ok_or("expected occurrence array")?;
            assert_eq!(rows.len(), 1_001);
            assert!(rows.iter().all(|row| row["transaction_id"].is_null()));
        }
    }
    let blocked = ledger
        .command()?
        .args([
            "--json",
            "--company",
            "acme",
            "schedule",
            "run",
            "--through",
            "2022-09-27",
        ])
        .output()?;
    assert_eq!(blocked.status.code(), Some(3));
    assert_eq!(blocked.stdout, [] as [u8; 0]);
    let error: Value = serde_json::from_slice(&blocked.stderr)?;
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "VALIDATION");
    let message = error["error"]["message"]
        .as_str()
        .ok_or("missing error message")?;
    for hint in [
        "1000 occurrences",
        "--dry-run",
        "--through",
        "--allow-large-run",
    ] {
        assert!(message.contains(hint));
    }
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("entries")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    assert_eq!(ledger.run("acme", &["schedule", "list", "--all"])?, pending);

    let posted = ledger.run(
        "acme",
        &[
            "schedule",
            "run",
            "--through",
            "2022-09-27",
            "--allow-large-run",
        ],
    )?;
    let rows = posted.as_array().ok_or("expected posting array")?;
    assert_eq!(rows.len(), 1_001);
    assert!(rows.iter().all(|row| row["transaction_id"].is_i64()));
    assert_eq!(ledger.count("transactions")?, 1_001);
    assert_eq!(ledger.count("entries")?, 2_002);
    assert_eq!(ledger.run("acme", &["schedule", "list"])?, json!([]));
    assert_eq!(
        ledger.run("acme", &["schedule", "run", "--through", "2022-09-27"])?,
        json!([])
    );
    ledger.run("acme", &["verify"])?;
    Ok(())
}

#[test]
fn dry_run_counts_include_empty_runs_and_respect_quiet_in_all_formats() -> TestResult {
    let ledger = Ledger::new()?;
    for format in ["table", "csv", "json"] {
        for quiet in [false, true] {
            let mut command = ledger.command()?;
            command.args([
                "--company",
                "acme",
                "--format",
                format,
                "schedule",
                "run",
                "--through",
                "2026-01-01",
                "--dry-run",
            ]);
            if quiet {
                command.arg("--quiet");
            }
            let output = command.output()?;
            assert!(output.status.success());
            if quiet {
                assert_eq!(output.stderr, [] as [u8; 0]);
                if format != "json" {
                    assert_eq!(output.stdout, [] as [u8; 0]);
                }
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("Preview: 0 due occurrences")
                );
            }
            if format == "json" {
                assert_eq!(data(&output)?, json!([]));
            }
        }
    }
    Ok(())
}

#[test]
fn closed_period_run_leaves_the_schedule_due_until_explicit_reopen() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create(
        "acme",
        "2026-01-31",
        ("5000:12.50", "1000:12.50"),
        &["--frequency", "monthly"],
    )?;
    let before = ledger.run("acme", &["schedule", "list"])?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    let history = ledger.count("audit_log")?;
    let output = ledger
        .command()?
        .args([
            "--json",
            "--company",
            "acme",
            "schedule",
            "run",
            "--through",
            "2026-01-31",
        ])
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("closed period"));
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("audit_log")?, history);
    assert_eq!(ledger.run("acme", &["schedule", "list"])?, before);
    ledger.run(
        "acme",
        &["period", "reopen", "--reason", "Post missing rent"],
    )?;
    ledger.run("acme", &["schedule", "run", "--through", "2026-01-31"])?;
    assert_eq!(ledger.count("transactions")?, 1);
    ledger.run("acme", &["verify"])?;
    Ok(())
}
