//! Storage protections, creation attribution, and independent tamper detection.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db, PostEntryParams, PostResult, PostTransactionParams};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
    path: PathBuf,
    db: Db,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let db = Db::open(&path, None)?;
        db::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
        db::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "1000",
                name: "Cash",
                account_type: "asset",
                default_tax_category: None,
            },
        )?;
        db::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code: "4000",
                name: "Revenue",
                account_type: "revenue",
                default_tax_category: None,
            },
        )?;
        Ok(Self { dir, path, db })
    }

    fn post(&self, reference: &str) -> Result<PostResult, beankeeper_cli::error::CliError> {
        let entries = [entry("1000", "debit"), entry("4000", "credit")];
        db::post_transaction(
            self.db.conn(),
            &PostTransactionParams {
                company_slug: "acme",
                description: "Sale",
                metadata: None,
                currency: "USD",
                date: "2026-01-01",
                entries: &entries,
                correlate: None,
                reference: Some(reference),
                on_conflict: db::ConflictStrategy::Skip,
                actor: &Actor::new("posting-agent"),
            },
        )
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
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
    }
}

fn entry(code: &str, direction: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: direction.into(),
        amount: 100,
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
    assert_eq!(ledger.post("original")?, PostResult::Created(1));
    assert_eq!(ledger.post("original")?, PostResult::Skipped(1));
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
    ledger.db.conn().execute_batch(
        "CREATE TRIGGER fail_creation BEFORE INSERT ON audit_log
        WHEN NEW.action = 'create' BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
    )?;
    assert!(ledger.post("original").is_err());
    for table in ["transactions", "entries"] {
        assert_eq!(ledger.count(table)?, 0);
    }
    assert_eq!(ledger.count("audit_log")?, creations);
    ledger
        .db
        .conn()
        .execute_batch("DROP TRIGGER fail_creation;")?;
    assert_eq!(ledger.post("original")?, PostResult::Created(1));
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
        "action = 'correlate'",
        "before = '{}'",
        "after = '{}'",
        "prev_hash = row_hash",
        "row_hash = prev_hash",
    ] {
        let ledger = Ledger::new()?;
        ledger.post("first")?;
        ledger.post("second")?;
        ledger
            .db
            .conn()
            .execute_batch("DROP TRIGGER protect_audit_update;")?;
        ledger.db.conn().execute(
            &format!("UPDATE audit_log SET {assignment} WHERE id = 1"),
            [],
        )?;
        failure(&ledger.verify()?, "audit hash chain broken at row #")?;
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
            .db
            .conn()
            .execute_batch("DROP TRIGGER protect_audit_delete;")?;
        ledger
            .db
            .conn()
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
            "entry amount must be positive",
        ),
        (
            "UPDATE entries SET direction = 'invalid' WHERE id = 1",
            "invalid entry direction",
        ),
        ("DELETE FROM entries WHERE id = 1", "at least two entries"),
        (
            "UPDATE transactions SET currency = 'ZZZ' WHERE id = 1",
            "invalid transaction currency",
        ),
        (
            "UPDATE entries SET company_slug = 'other' WHERE id = 1",
            "entry belongs to another company",
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
        failure(&ledger.verify()?, expected)?;
    }
    Ok(())
}

#[test]
fn verify_detects_balanced_rewrites_via_creation_snapshot() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.post("first")?;
    ledger.db.conn().execute_batch(
        "DROP TRIGGER protect_entries_update;
        UPDATE entries SET amount = 200;",
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
    db::set_entry_status(
        ledger.db.conn(),
        &Actor::new("clearer"),
        "acme",
        1,
        1,
        db::EntryStatus::Cleared,
    )?;
    ledger.db.conn().execute(
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
        .conn()
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
            .conn()
            .query_row("SELECT MAX(id) - 1 FROM audit_log", [], |row| row.get(0))?;
    let audit_count = ledger.count("audit_log")?;
    ledger.db.conn().execute_batch(
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
        ledger.db.conn().execute_batch(tamper)?;
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
        entry("1000", "debit"),
        entry("1000", "debit"),
        entry("4000", "credit"),
        entry("4000", "credit"),
    ];
    for entry in &mut entries {
        entry.amount = i64::MAX;
    }
    db::post_transaction(
        ledger.db.conn(),
        &PostTransactionParams {
            company_slug: "acme",
            description: "Large balance",
            metadata: None,
            currency: "USD",
            date: "2026-01-01",
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: db::ConflictStrategy::Error,
            actor: &Actor::new("large-poster"),
        },
    )?;
    success(&ledger.verify()?)?;
    Ok(())
}
