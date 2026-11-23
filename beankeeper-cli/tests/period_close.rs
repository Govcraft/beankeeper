//! Inclusive company cutoffs, audited reopening, and atomic posting failures.

use beankeeper_cli::db::CreateAccountParams;
use beankeeper_cli::identity::{Admin, Granted};
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::verification::VerifiedLedger;
use beankeeper_cli::db::{self, Actor, Db, audit, periods, transactions};
use beankeeper_cli::error::CliError;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
    path: PathBuf,
    db: Db,
}

impl Ledger {
    fn raw(&self) -> rusqlite::Result<rusqlite::Connection> {
        let connection = rusqlite::Connection::open(&self.path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let db = Db::open(&path, None)?;
        for company in ["acme", "other"] {
            Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                db::create_company(grant, &company.parse()?, &company.parse()?, None)
            })?;
            for (code, kind) in [
                ("1000", beankeeper::types::AccountType::Asset),
                ("4000", beankeeper::types::AccountType::Revenue),
            ] {
                Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                    db::create_account(
                        grant,
                        &CreateAccountParams {
                            company_slug: &company.parse()?,
                            code,
                            name: code,
                            account_type: kind,
                            default_tax_category: None,
                        },
                    )
                })?;
            }
        }
        Ok(Self { dir, path, db })
    }

    fn call(&self, company: &str, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let mut cmd = Command::cargo_bin("bk")?;
        cmd.arg("--db")
            .arg(&self.path)
            .args(["--company", company, "--actor", "owner"]);
        for variable in [
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
            "BEANKEEPER_PASSPHRASE_CMD",
        ] {
            cmd.env_remove(variable);
        }
        Ok(cmd.args(args).output()?)
    }

    fn run(&self, company: &str, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut flags = vec!["--json"];
        flags.extend_from_slice(args);
        let output = self.call(company, &flags)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].clone())
    }

    fn state(&self) -> Result<(i64, i64, i64), CliError> {
        Ok(self.db.read().query_row(
            "SELECT (SELECT COUNT(*) FROM transactions), (SELECT COUNT(*) FROM entries),
             (SELECT COUNT(*) FROM audit_log)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }
}

fn post_args(date: &str) -> Vec<&str> {
    vec![
        "txn",
        "post",
        "--description",
        "Sale",
        "--date",
        date,
        "--debit",
        "1000:1.00",
        "--credit",
        "4000:1.00",
    ]
}

fn rejected(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("closed period"));
}

#[test]
fn close_blocks_inclusive_dates_but_keeps_company_and_later_dates_open() -> TestResult {
    let ledger = Ledger::new()?;
    assert_eq!(
        ledger.run("acme", &["period", "list"])?,
        serde_json::json!([])
    );
    let closed = ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    assert_eq!(closed[0]["closed_through"], "2026-01-31");
    assert_eq!(closed[0]["actor"], "owner");
    let state = ledger.state()?;
    for date in ["2025-12-01", "2026-01-31", "2026-1-31"] {
        rejected(&ledger.call("acme", &post_args(date))?);
        assert_eq!(ledger.state()?, state);
    }
    ledger.run("acme", &post_args("2026-02-01"))?;
    ledger.run("other", &post_args("2026-01-31"))?;
    ledger.run("acme", &["verify"])?;
    let table = ledger.call("acme", &["period", "list"])?;
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains("2026-01-31"));
    let csv = ledger.call("acme", &["--format", "csv", "period", "list"])?;
    assert!(csv.status.success());
    assert_eq!(String::from_utf8_lossy(&csv.stdout).lines().count(), 2);
    Ok(())
}

#[test]
fn reopen_requires_a_reason_records_history_and_allows_backdated_posts() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    let before = ledger.state()?;
    for args in [
        vec!["period", "close", "--through", "2026-01-30"],
        vec!["period", "close", "--through", "2026-01-31"],
        vec!["period", "reopen", "--reason", "  "],
        vec!["period", "close", "--through", "2026-02-30"],
        vec!["period", "close", "--through", "2026-2-28"],
    ] {
        assert_eq!(ledger.call("acme", &args)?.status.code(), Some(3));
        assert_eq!(ledger.state()?, before);
    }
    assert_eq!(
        ledger.call("acme", &["period", "reopen"])?.status.code(),
        Some(2)
    );
    ledger.run("acme", &["period", "close", "--through", "2026-02-28"])?;
    let reopened = ledger.run(
        "acme",
        &[
            "period",
            "reopen",
            "--reason",
            "  Correct missing receipt  ",
        ],
    )?;
    assert!(reopened[0]["closed_through"].is_null());
    assert_eq!(reopened[0]["reason"], "Correct missing receipt");
    assert_eq!(ledger.run("acme", &["period", "list"])?, reopened);
    ledger.run("acme", &post_args("2026-01-31"))?;
    assert_eq!(
        ledger
            .call("acme", &["period", "reopen", "--reason", "Again"])?
            .status
            .code(),
        Some(3)
    );
    let history = ledger.run(
        "acme",
        &[
            "audit",
            "log",
            "--entity",
            "period",
            "--action",
            "period_reopen",
        ],
    )?;
    assert_eq!(history.as_array().map(Vec::len), Some(1));
    assert_eq!(history[0]["actor"], "owner");
    let snapshot: Value = serde_json::from_str(history[0]["after"].as_str().ok_or("snapshot")?)?;
    assert_eq!(snapshot["reason"], "Correct missing receipt");
    assert_eq!(ledger.state()?.0, 1);
    ledger.run("acme", &["verify"])?;
    // A new close after reopening may choose an earlier cutoff.
    ledger.run("acme", &["period", "close", "--through", "2025-12-31"])?;
    Ok(())
}

#[test]
fn csv_and_json_import_lines_into_closed_dates_are_rejected_atomically() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    let before = ledger.state()?;
    let csv = ledger.dir.path().join("bank.csv");
    std::fs::write(
        &csv,
        "date,description,amount\n2026-01-31,Closed sale,1.00\n",
    )?;
    rejected(&ledger.call(
        "acme",
        &[
            "txn",
            "import",
            "--file",
            csv.to_str().ok_or("path")?,
            "--account",
            "1000",
            "--suspense",
            "4000",
        ],
    )?);
    assert_eq!(ledger.state()?, before);
    let json = ledger.dir.path().join("bulk.json");
    std::fs::write(
        &json,
        r#"[{"date":"2026-01-31","description":"Closed sale","entries":[{"account_code":"1000","direction":"debit","amount":"1.00"},{"account_code":"4000","direction":"credit","amount":"1.00"}]}]"#,
    )?;
    rejected(&ledger.call(
        "acme",
        &["txn", "import", "--file", json.to_str().ok_or("path")?],
    )?);
    assert_eq!(ledger.state()?, before);
    Ok(())
}

#[test]
fn direct_posting_and_sql_cannot_bypass_a_closed_period() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    let entries = [
        transactions::PostEntryParams {
            account_code: "1000".into(),
            direction: beankeeper::types::DebitOrCredit::Debit,
            amount: 100_i64
                .try_into()
                .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
            memo: None,
            tax_category: None,
        },
        transactions::PostEntryParams {
            account_code: "4000".into(),
            direction: beankeeper::types::DebitOrCredit::Credit,
            amount: 100_i64
                .try_into()
                .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
            memo: None,
            tax_category: None,
        },
    ];
    let actor = Actor::new("direct".parse()?);
    let before = ledger.state()?;
    assert!(matches!(
        {
            let params = transactions::PostTransactionParams {
                company_slug: &"acme".parse()?,
                description: "Bypass",
                currency: beankeeper::types::Currency::USD,
                date: "2026-1-31"
                    .parse()
                    .unwrap_or_else(|error| panic!("fixture date: {error}")),
                entries: &entries,
                metadata: None,
                correlate: None,
                reference: None,
                on_conflict: transactions::ConflictStrategy::Error,
                actor: &actor,
            };
            beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Post>::with_db(
                &ledger.db,
                params.actor,
                |grant| transactions::post_transaction(grant, &params),
            )
        },
        Err(CliError::Validation(_))
    ));
    assert!(ledger.raw()?.execute("INSERT INTO transactions (company_slug, description, date) VALUES ('acme', 'bypass', '2026-01-31')", []).is_err());
    assert_eq!(ledger.state()?, before);
    Ok(())
}

#[test]
fn lock_mutations_require_audit_and_verify_detects_trigger_bypass() -> TestResult {
    for tamper in [
        "UPDATE period_locks SET closed_through = NULL, reason = 'bypass'",
        "DELETE FROM period_locks",
        "UPDATE period_locks SET actor = 'forged'",
        "INSERT OR REPLACE INTO period_locks SELECT * FROM period_locks",
    ] {
        let ledger = Ledger::new()?;
        ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
        assert!(ledger.raw()?.execute(tamper, []).is_err(), "{tamper}");
        VerifiedLedger::read(&ledger.db)?;
    }
    for tamper in [
        "UPDATE period_locks SET closed_through = NULL, reason = 'bypass'",
        "DELETE FROM period_locks",
    ] {
        let ledger = Ledger::new()?;
        ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
        ledger.raw()?.execute_batch(
            "DROP TRIGGER protect_period_update; DROP TRIGGER protect_period_delete;",
        )?;
        ledger.raw()?.execute(tamper, [])?;
        assert!(matches!(
            VerifiedLedger::read(&ledger.db),
            Err(CliError::Database(_))
        ));
        assert!(matches!(
            periods::ensure_open(ledger.db.read(), &"acme".parse()?, "2026-02-01".parse()?),
            Err(CliError::Database(_))
        ));
    }
    // An audit-row edit also remains detectable with periods present.
    let ledger = Ledger::new()?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    ledger.raw()?.execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE entity = 'period';")?;
    assert!(audit::verify_hash_chain(ledger.db.read()).is_err());
    Ok(())
}

#[test]
fn a_noop_retry_remains_allowed_after_close_and_reversals_use_their_own_date() -> TestResult {
    let ledger = Ledger::new()?;
    let mut args = post_args("2026-01-01");
    args.extend(["--reference", "retry-closed"]);
    let created = ledger.run("acme", &args)?;
    ledger.run("acme", &["period", "close", "--through", "2026-01-31"])?;
    let before = ledger.state()?;
    args.extend(["--on-conflict", "skip"]);
    assert_eq!(ledger.run("acme", &args)?["skipped"], true);
    assert_eq!(ledger.state()?, before);
    let id = created["id"].as_i64().ok_or("transaction ID")?.to_string();
    rejected(&ledger.call("acme", &["txn", "reverse", &id, "--date", "2026-01-31"])?);
    assert_eq!(ledger.state()?, before);
    ledger.run("acme", &["txn", "reverse", &id, "--date", "2026-02-01"])?;
    ledger.run("acme", &["verify"])?;
    Ok(())
}
