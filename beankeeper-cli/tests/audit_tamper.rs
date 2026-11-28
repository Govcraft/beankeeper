//! Storage protections, creation attribution, and independent tamper detection.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db, PostEntryParams, PostResult, PostTransactionParams};
use beankeeper_cli::identity::{Admin, Granted, Reconcile};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
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
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "4000",
                    name: "Revenue",
                    account_type: beankeeper::types::AccountType::Revenue,
                    default_tax_category: None,
                },
            )
        })?;
        Ok(Self { dir, path, db })
    }

    fn post(&self, reference: &str) -> Result<PostResult, beankeeper_cli::error::CliError> {
        self.post_in(
            &"acme".parse()?,
            beankeeper::types::Currency::USD,
            reference,
        )
    }

    fn post_in(
        &self,
        company: &db::companies::CompanySlug,
        currency: beankeeper::types::Currency,
        reference: &str,
    ) -> Result<PostResult, beankeeper_cli::error::CliError> {
        let entries = [
            entry("1000", beankeeper::types::DebitOrCredit::Debit),
            entry("4000", beankeeper::types::DebitOrCredit::Credit),
        ];
        {
            let params = PostTransactionParams {
                company_slug: company,
                description: "Sale",
                metadata: None,
                currency,
                date: "2026-01-01"
                    .parse()
                    .unwrap_or_else(|error| panic!("fixture date: {error}")),
                entries: &entries,
                correlate: None,
                reference: Some(reference),
                on_conflict: db::ConflictStrategy::Skip,
                actor: &Actor::new("posting-agent".parse()?),
            };
            beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Post>::with_db(
                &self.db,
                params.actor,
                |grant| db::post_transaction(grant, &params),
            )
        }
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut cmd = Command::cargo_bin("bk")?;
        cmd.arg("--db")
            .arg(&self.path)
            .args(["--json", "--company", "acme"])
            .env_remove("BEANKEEPER_PASSPHRASE_CMD");
        Ok(cmd)
    }

    fn verify(&self) -> Result<Output, Box<dyn Error>> {
        Ok(self.command()?.arg("verify").output()?)
    }

    fn count(&self, table: &str) -> Result<i64, rusqlite::Error> {
        self.db
            .read()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
    }
}

fn entry(code: &str, direction: beankeeper::types::DebitOrCredit) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction,
        amount: 100_i64
            .try_into()
            .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
        memo: None,
        tax_category: None,
    }
}

fn success(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn failure(output: &Output, message: &str) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "DATABASE");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains(message)),
        "{error}"
    );
    Ok(())
}

#[test]
fn sql_blocks_financial_edits_deletes_and_replace_even_without_recursive_triggers() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("original")?;
    let conn = Connection::open(&ledger.path)?;
    conn.pragma_update(None, "recursive_triggers", false)?;
    for (table, assignments) in [
        (
            "transactions",
            vec![
                "id = 2",
                "company_slug = 'other'",
                "description = 'changed'",
                "date = '2026-02-01'",
                "currency = 'MXN'",
                "posted_at = 'changed'",
                "reference = 'changed'",
                "posted_by = 'imposter'",
            ],
        ),
        (
            "entries",
            vec![
                "id = 9",
                "transaction_id = 2",
                "company_slug = 'other'",
                "account_code = '4000'",
                "direction = 'credit'",
                "amount = 200",
                "memo = 'changed'",
                "tax_category = 'changed'",
            ],
        ),
        (
            "audit_log",
            vec!["actor = 'imposter'", "after = NULL", "prev_hash = row_hash"],
        ),
    ] {
        for assignment in assignments {
            let sql = format!("UPDATE {table} SET {assignment} WHERE id = 1");
            assert!(conn.execute(&sql, []).is_err(), "{sql}");
        }
        assert!(conn.execute(&format!("DELETE FROM {table}"), []).is_err());
        assert!(
            conn.execute(
                &format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table}"),
                []
            )
            .is_err()
        );
    }
    assert!(conn.execute("INSERT OR REPLACE INTO transactions (company_slug, description, currency, date, reference)
        VALUES ('acme', 'replacement', 'USD', '2026-01-01', 'original')", []).is_err());
    success(&ledger.verify()?)?;
    Ok(())
}

#[test]
fn posts_and_reversals_record_creators_and_skip_retries_without_new_audit_rows() -> TestResult {
    let ledger = Ledger::new()?;
    assert_eq!(ledger.post("original")?, PostResult::Created(1.try_into()?));
    assert_eq!(ledger.post("original")?, PostResult::Skipped(1.try_into()?));
    let result = ledger
        .command()?
        .args([
            "--actor",
            "human",
            "txn",
            "reverse",
            "1",
            "--date",
            "2026-02-01",
        ])
        .output()?;
    success(&result)?;
    let output = ledger
        .command()?
        .args([
            "audit",
            "log",
            "--entity",
            "transaction",
            "--action",
            "create",
        ])
        .output()?;
    let rows = success(&output)?;
    assert_eq!(
        rows["data"].as_array().ok_or("missing audit rows")?.len(),
        2
    );
    assert_eq!(rows["data"][0]["actor"], "human");
    assert_eq!(rows["data"][1]["actor"], "posting-agent");
    assert_eq!(
        rows["data"][0]["row_hash"]
            .as_str()
            .ok_or("missing hash")?
            .len(),
        64
    );
    let output = ledger.command()?.args(["txn", "show", "2"]).output()?;
    assert_eq!(success(&output)?["data"][0]["posted_by"], "human");
    success(&ledger.verify()?)?;
    Ok(())
}

#[test]
fn creation_audit_failure_rolls_back_posting_and_chain_sequence() -> TestResult {
    let ledger = Ledger::new()?;
    let creations = ledger.count("audit_log")?;
    ledger.raw()?.execute_batch(
        "CREATE TRIGGER fail_creation BEFORE INSERT ON audit_log
        WHEN NEW.action = 'create' BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
    )?;
    assert!(ledger.post("original").is_err());
    for table in ["transactions", "entries"] {
        assert_eq!(ledger.count(table)?, 0);
    }
    assert_eq!(ledger.count("audit_log")?, creations);
    ledger.raw()?.execute_batch("DROP TRIGGER fail_creation;")?;
    assert_eq!(ledger.post("original")?, PostResult::Created(1.try_into()?));
    success(&ledger.verify()?)?;
    Ok(())
}

#[test]
fn verify_reports_first_modified_hash_link_for_each_canonical_field() -> TestResult {
    for assignment in [
        "id = 100",
        "changed_at = 'changed'",
        "actor = 'imposter'",
        "company_slug = NULL",
        "entity = 'budget'",
        "entity_id = 'other'",
        "action = 'metadata_set'",
        "before = '{}'",
        "after = '{}'",
        "prev_hash = row_hash",
        "row_hash = prev_hash",
    ] {
        let ledger = Ledger::new()?;
        ledger.post("first")?;
        ledger.post("second")?;
        ledger
            .raw()?
            .execute_batch("DROP TRIGGER protect_audit_update;")?;
        ledger.raw()?.execute(
            &format!("UPDATE audit_log SET {assignment} WHERE id = 1"),
            [],
        )?;
        failure(
            &ledger.verify()?,
            "audit_hash_chain failed: audit hash chain broken at row #",
        )?;
    }
    Ok(())
}

#[test]
fn verify_detects_deleted_prefix_middle_tail_and_entire_chain() -> TestResult {
    for (condition, expected) in [
        ("id = 1", "broken at row #2"),
        ("id = 2", "broken at row #3"),
        (
            "id = (SELECT MAX(id) FROM audit_log)",
            "tail missing after row #5",
        ),
        ("1 = 1", "tail missing after row #0"),
    ] {
        let ledger = Ledger::new()?;
        for reference in ["first", "second", "third"] {
            ledger.post(reference)?;
        }
        ledger
            .raw()?
            .execute_batch("DROP TRIGGER protect_audit_delete;")?;
        ledger
            .raw()?
            .execute(&format!("DELETE FROM audit_log WHERE {condition}"), [])?;
        failure(&ledger.verify()?, expected)?;
    }
    Ok(())
}

#[test]
fn verify_rechecks_balance_positive_amounts_currency_and_entry_ownership() -> TestResult {
    for (sql, expected) in [
        (
            "UPDATE entries SET amount = 101 WHERE id = 1",
            "debits do not equal credits",
        ),
        (
            "UPDATE entries SET amount = 0 WHERE id = 1",
            "entry #1: entry amount must be positive",
        ),
        (
            "UPDATE entries SET direction = 'invalid' WHERE id = 1",
            "entry #1: invalid entry direction",
        ),
        (
            "DELETE FROM entries WHERE id = 1",
            "must have at least two entries",
        ),
        (
            "UPDATE transactions SET currency = 'ZZZ' WHERE id = 1",
            "invalid transaction currency",
        ),
        (
            "UPDATE entries SET company_slug = 'other' WHERE id = 1",
            "entry #1: entry belongs to another company",
        ),
    ] {
        let ledger = Ledger::new()?;
        ledger.post("first")?;
        let conn = Connection::open(&ledger.path)?;
        conn.execute_batch(
            "DROP TRIGGER protect_entries_update; DROP TRIGGER protect_entries_delete;
            DROP TRIGGER protect_transactions_update; PRAGMA ignore_check_constraints = ON; PRAGMA foreign_keys = OFF;",
        )?;
        conn.execute(sql, [])?;
        failure(
            &ledger.verify()?,
            &format!("accounting failed: transaction #1 in company 'acme': {expected}"),
        )?;
    }
    Ok(())
}

#[test]
fn verify_detects_balanced_rewrites_via_creation_snapshot() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("first")?;
    ledger.raw()?.execute_batch(
        "DROP TRIGGER protect_entries_update;
        UPDATE entries SET amount = 200;",
    )?;
    failure(
        &ledger.verify()?,
        "does not match its creation audit record",
    )
}

#[test]
fn creation_verification_detects_rewritten_date_text() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("date-spelling")?;
    success(&ledger.verify()?)?;
    ledger.raw()?.execute_batch(
        "DROP TRIGGER protect_transactions_update;
        UPDATE transactions SET date = '2026-1-1' WHERE id = 1;",
    )?;
    failure(
        &ledger.verify()?,
        "does not match its creation audit record",
    )
}

#[test]
fn allowed_clearing_and_metadata_mutations_keep_verification_healthy() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("first")?;
    Granted::<Reconcile>::with_db(&ledger.db, &Actor::new("clearer".parse()?), |grant| {
        db::set_entry_status(
            grant,
            &"acme".parse()?,
            1.try_into()?,
            1.try_into()?,
            db::EntryStatus::Cleared,
        )
    })?;
    ledger.raw()?.execute(
        "UPDATE transactions SET metadata = ?1 WHERE id = 1",
        params![r#"{"note":"allowed"}"#],
    )?;
    success(&ledger.verify()?)?;
    Ok(())
}

#[test]
fn cli_post_and_every_import_format_honor_explicit_actor_and_environment() -> TestResult {
    let ledger = Ledger::new()?;
    let creations = ledger.count("audit_log")?;
    let post = ledger
        .command()?
        .env("BK_ACTOR", "environment-agent")
        .args([
            "txn", "post", "-d", "Sale", "--debit", "1000:1", "--credit", "4000:1",
        ])
        .output()?;
    success(&post)?;
    let json = r#"[{"date":"2026-01-01","description":"JSON sale","entries":[
        {"account_code":"1000","direction":"debit","amount":"1"},
        {"account_code":"4000","direction":"credit","amount":"1"}]}]"#;
    let csv = "date,description,amount\n2026-01-01,CSV sale,1\n";
    let ofx = "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n<OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS><DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1><BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS><STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>123<ACCTID>456<ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20260101<DTEND>20260131<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260101<TRNAMT>1.00<FITID>ACTOR-TEST<NAME>OFX sale</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>1.00<DTASOF>20260131</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>";
    for (format, content) in [("json", json), ("csv", csv), ("ofx", ofx)] {
        let file = ledger.dir.path().join(format!("import.{format}"));
        std::fs::write(&file, content)?;
        let output = ledger
            .command()?
            .env("BK_ACTOR", "ignored-env")
            .args([
                "--actor",
                &format!("{format}-agent"),
                "txn",
                "import",
                "--file",
            ])
            .arg(&file)
            .args(["--account", "1000", "--suspense", "4000"])
            .output()?;
        success(&output)?;
    }
    let mut stmt = ledger
        .db
        .read()
        .prepare("SELECT posted_by FROM transactions ORDER BY id")?;
    let posters = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        posters,
        ["environment-agent", "json-agent", "csv-agent", "ofx-agent"]
    );
    assert_eq!(ledger.count("audit_log")?, creations + 4);
    success(&ledger.verify()?)?;
    Ok(())
}

#[test]
fn posting_after_tail_removal_fails_without_hiding_evidence() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("first")?;
    ledger.post("second")?;
    let last_retained: i64 =
        ledger
            .db
            .read()
            .query_row("SELECT MAX(id) - 1 FROM audit_log", [], |row| row.get(0))?;
    let audit_count = ledger.count("audit_log")?;
    ledger.raw()?.execute_batch(
        "DROP TRIGGER protect_audit_delete;
        DELETE FROM audit_log WHERE id = (SELECT MAX(id) FROM audit_log);",
    )?;
    assert!(ledger.post("third").is_err());
    assert_eq!(ledger.count("transactions")?, 2);
    assert_eq!(ledger.count("audit_log")?, audit_count - 1);
    failure(
        &ledger.verify()?,
        &format!("tail missing after row #{last_retained}"),
    )
}

#[test]
fn verify_detects_deleted_posting_and_erased_poster_against_creation_audit() -> TestResult {
    for tamper in [
        "DROP TRIGGER protect_transactions_update; UPDATE transactions SET posted_by = NULL;",
        "DROP TRIGGER protect_entries_delete; DROP TRIGGER protect_transactions_delete;
         DELETE FROM entries; DELETE FROM transactions;",
    ] {
        let ledger = Ledger::new()?;
        ledger.post("first")?;
        ledger.raw()?.execute_batch(tamper)?;
        failure(&ledger.verify()?, "creation audit record")?;
    }
    Ok(())
}

#[test]
fn dry_runs_and_skipped_imports_leave_creation_history_unchanged() -> TestResult {
    let ledger = Ledger::new()?;
    let creations = ledger.count("audit_log")?;
    let file = ledger.dir.path().join("import.csv");
    std::fs::write(&file, "date,description,amount\n2026-01-01,Sale,1\n")?;
    for extra in [vec!["--dry-run"], vec![], vec!["--on-conflict", "skip"]] {
        let output = ledger
            .command()?
            .args(["txn", "import", "--file"])
            .arg(&file)
            .args(["--account", "1000", "--suspense", "4000"])
            .args(&extra)
            .output()?;
        success(&output)?;
        assert_eq!(
            ledger.count("audit_log")?,
            creations + i64::from(extra != ["--dry-run"])
        );
        success(&ledger.verify()?)?;
    }
    Ok(())
}

#[test]
fn verify_uses_wide_totals_for_large_balanced_postings() -> TestResult {
    let ledger = Ledger::new()?;
    let mut entries = vec![
        entry("1000", beankeeper::types::DebitOrCredit::Debit),
        entry("1000", beankeeper::types::DebitOrCredit::Debit),
        entry("4000", beankeeper::types::DebitOrCredit::Credit),
        entry("4000", beankeeper::types::DebitOrCredit::Credit),
    ];
    for entry in &mut entries {
        entry.amount = i64::MAX.try_into()?;
    }
    {
        let params = PostTransactionParams {
            company_slug: &"acme".parse()?,
            description: "Large balance",
            metadata: None,
            currency: beankeeper::types::Currency::USD,
            date: "2026-01-01"
                .parse()
                .unwrap_or_else(|error| panic!("fixture date: {error}")),
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: db::ConflictStrategy::Error,
            actor: &Actor::new("large-poster".parse()?),
        };
        beankeeper_cli::identity::Granted::<beankeeper_cli::identity::Post>::with_db(
            &ledger.db,
            params.actor,
            |grant| db::post_transaction(grant, &params),
        )
    }?;
    let data = success(&ledger.verify()?)?;
    let total = (i128::from(i64::MAX) * 2).to_string();
    let usd = &data["data"]["companies"][0]["currencies"][0];
    assert_eq!(usd["debit_minor"], total);
    assert_eq!(usd["credit_minor"], total);
    Ok(())
}

const VERIFICATION_CHECKS: &[&str] = &[
    "schema_version",
    "audit_hash_chain",
    "ledger_policy",
    "period_locks",
    "proposal_resolutions",
    "spend_limits",
    "accounting",
    "creation_snapshots",
    "account_archives",
    "account_labels",
    "account_classifications",
    "counterparties",
    "bank_feed",
    "open_items",
    "transaction_links",
    "statement_batches",
    "statement_reconciliations",
    "payroll_runs",
    "fixed_assets",
    "foreign_keys",
    "sqlite_integrity",
    "row_counts",
];

#[test]
fn verify_returns_company_totals_counts_and_the_recomputed_audit_head() -> TestResult {
    use beankeeper::types::{AccountType, Currency};

    let ledger = Ledger::new()?;
    let other = "other".parse()?;
    let empty = "empty".parse()?;
    let actor = Actor::new("fixture".parse()?);
    for (company, name) in [(&other, "Other"), (&empty, "Empty")] {
        Granted::<Admin>::with_db(&ledger.db, &actor, |grant| {
            db::create_company(grant, company, &name.parse()?, None)
        })?;
    }
    for (code, name, account_type) in [
        ("1000", "Cash", AccountType::Asset),
        ("4000", "Revenue", AccountType::Revenue),
    ] {
        Granted::<Admin>::with_db(&ledger.db, &actor, |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &other,
                    code,
                    name,
                    account_type,
                    default_tax_category: None,
                },
            )
        })?;
    }

    ledger.post("sale-1")?;
    ledger.post("sale-2")?;
    ledger.post_in(&"acme".parse()?, Currency::JPY, "sale-jpy")?;
    ledger.post_in(&other, Currency::USD, "other-sale")?;

    let data = &success(&ledger.verify()?)?["data"];
    let counts = &data["row_counts"];
    let audit_count = ledger.count("audit_log")?;
    assert_eq!(
        counts,
        &json!({
            "companies": 3, "accounts": 4, "transactions": 4, "entries": 8, "audit_rows": audit_count,
        })
    );
    let total = |currency: &str, digits: u8, amount: &str| {
        json!({
            "currency": currency, "minor_unit_digits": digits,
            "debit_minor": amount, "credit_minor": amount, "balanced": true,
        })
    };
    assert_eq!(
        data["companies"],
        json!([
            {"company": "acme", "currencies": [total("JPY", 0, "100"), total("USD", 2, "200")]},
            {"company": "empty", "currencies": []},
            {"company": "other", "currencies": [total("USD", 2, "100")]},
        ])
    );
    let audit_end: (i64, String) = ledger.raw()?.query_row(
        "SELECT id, row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        data["audit_chain"],
        json!({
            "first_id": 1, "last_id": audit_end.0, "head_hash": audit_end.1,
            "length": audit_count, "intact": true,
        })
    );
    let checks = data["checks"].as_array().ok_or("verification checks")?;
    assert_eq!(
        checks,
        &VERIFICATION_CHECKS
            .iter()
            .map(|name| json!({"name": name, "outcome": "passed"}))
            .collect::<Vec<_>>()
    );
    assert_eq!(data["desktop_view"]["link"], json!({"view_id": "review"}));

    let text = Command::cargo_bin("bk")?
        .arg("--db")
        .arg(&ledger.path)
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .arg("verify")
        .output()?;
    assert!(text.status.success());
    let report = std::str::from_utf8(&text.stderr)?;
    assert!(report.contains("acme (USD): debits and credits each total 2.00"));
    assert!(report.contains("empty: no posted transactions"));
    assert!(report.contains(&audit_end.1));
    Ok(())
}

#[test]
fn empty_ledger_has_an_intact_chain_without_a_head() -> TestResult {
    let db = Db::open_in_memory()?;
    let data = serde_json::to_value(beankeeper_cli::db::verification::VerifiedLedger::read(&db)?)?;
    assert_eq!(data["companies"], json!([]));
    assert_eq!(
        data["row_counts"],
        json!({
            "companies": 0, "accounts": 0, "transactions": 0, "entries": 0, "audit_rows": 0,
        })
    );
    assert_eq!(
        data["audit_chain"],
        json!({
            "length": 0, "first_id": null, "last_id": null, "head_hash": null, "intact": true,
        })
    );
    Ok(())
}
