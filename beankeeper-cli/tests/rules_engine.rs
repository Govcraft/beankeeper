//! Persisted categorization, import integration, and audit guarantees (issue #6).

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{Actor, Db, accounts, audit, companies, rules};
use beankeeper_cli::identity::{Granted, Post};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
    path: PathBuf,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("rules.db");
        let db = Db::open(&path, None)?;
        for company in ["acme", "other"] {
            companies::create_company(
                db.conn(),
                &Actor::new("fixture"),
                &company.parse()?,
                company,
                None,
            )?;
            for (code, kind, tax) in [
                ("1000", beankeeper::types::AccountType::Asset, None),
                (
                    "4000",
                    beankeeper::types::AccountType::Revenue,
                    Some("sales"),
                ),
                (
                    "5000",
                    beankeeper::types::AccountType::Expense,
                    Some("dining"),
                ),
                (
                    "9000",
                    beankeeper::types::AccountType::Expense,
                    Some("fallback"),
                ),
            ] {
                accounts::create_account(
                    db.conn(),
                    &Actor::new("fixture"),
                    &CreateAccountParams {
                        company_slug: &company.parse()?,
                        code,
                        name: code,
                        account_type: kind,
                        default_tax_category: tax,
                    },
                )?;
            }
        }
        Ok(Self { dir, path })
    }

    fn command(&self, company: &str) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .args(["--json", "--company", company, "--actor", "rules-test"])
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .env_remove("BEANKEEPER_CURRENCY")
            .env_remove("BK_AGENT_TOKEN");
        Ok(command)
    }

    fn create(
        &self,
        pattern: &str,
        account: &str,
        priority: &str,
    ) -> Result<Value, Box<dyn Error>> {
        data(
            &self
                .command("acme")?
                .args([
                    "rule",
                    "create",
                    "--contains",
                    pattern,
                    "--account",
                    account,
                    "--priority",
                    priority,
                ])
                .output()?,
        )
    }

    fn import(
        &self,
        company: &str,
        name: &str,
        content: &str,
        flags: &[&str],
    ) -> Result<Output, Box<dyn Error>> {
        let file = self.dir.path().join(name);
        std::fs::write(&file, content)?;
        Ok(self
            .command(company)?
            .args(["txn", "import", "--file"])
            .arg(file)
            .args(flags)
            .output()?)
    }

    fn count(&self, table: &str) -> Result<i64, Box<dyn Error>> {
        let db = Db::open_existing(&self.path, None)?;
        Ok(db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }
}

const BANK: &[&str] = &["--account", "1000", "--suspense", "9000"];

fn data(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(envelope["ok"], true);
    Ok(envelope["data"].clone())
}

#[test]
fn cli_rules_persist_are_company_scoped_and_validate_input() -> TestResult {
    let ledger = Ledger::new()?;
    let created = ledger.create("  Starbucks  ", "5000", "12")?;
    assert_eq!(created[0]["contains"], "starbucks");
    assert_eq!(created[0]["priority"], 12);
    let id = created[0]["id"].as_str().ok_or("missing ID")?;
    assert!(id.starts_with("rule_"));
    let list = data(&ledger.command("acme")?.args(["rule", "list"]).output()?)?;
    assert_eq!(list, created);
    assert_eq!(
        data(&ledger.command("other")?.args(["rule", "list"]).output()?)?,
        json!([])
    );
    let wrong_company = ledger
        .command("other")?
        .args(["rule", "delete", id, "--force"])
        .output()?;
    assert_eq!(wrong_company.status.code(), Some(5));
    for (pattern, account) in [("  ", "5000"), ("coffee", "missing")] {
        assert!(
            !ledger
                .command("acme")?
                .args([
                    "rule",
                    "create",
                    "--contains",
                    pattern,
                    "--account",
                    account
                ])
                .output()?
                .status
                .success()
        );
    }
    assert!(
        !ledger
            .command("acme")?
            .args(["rule", "delete", "invalid", "--force"])
            .output()?
            .status
            .success()
    );
    assert_eq!(ledger.count("categorization_rules")?, 1);
    let blocked = ledger
        .command("acme")?
        .args(["account", "delete", "5000", "--force"])
        .output()?;
    assert_eq!(blocked.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("categorization rules"));
    data(
        &ledger
            .command("acme")?
            .args(["rule", "delete", id, "--force"])
            .output()?,
    )?;
    assert_eq!(ledger.count("categorization_rules")?, 0);
    let log = data(
        &ledger
            .command("acme")?
            .args(["audit", "log", "--company", "acme", "--entity", "rule"])
            .output()?,
    )?;
    let text = log.to_string();
    assert!(
        text.contains("rule_delete") && text.contains("rules-test") && text.contains("starbucks")
    );
    data(&ledger.command("acme")?.args(["verify"]).output()?)?;
    data(
        &ledger
            .command("acme")?
            .args(["account", "delete", "5000", "--force"])
            .output()?,
    )?;
    Ok(())
}

#[test]
fn matching_is_literal_lowercase_and_uses_priority_then_id() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("coffee", "4000", "-1")?;
    ledger.create("coffee", "5000", "10")?;
    ledger.create("coffee", "9000", "10")?;
    ledger.create("CAFÉ 10%_", "5000", "0")?;
    let db = Db::open_existing(&ledger.path, None)?;
    let rules = rules::list_rules(&beankeeper_cli::db::companies::Company::bind(
        db.read(),
        "acme".parse()?,
    )?)?;
    assert_eq!(rules[0].priority, 10);
    assert!(rules[0].id.to_string() < rules[1].id.to_string());
    assert_eq!(
        rules::matching_rule(&rules, "COFFEE shop").map(|r| &r.id),
        Some(&rules[0].id)
    );
    assert!(rules::matching_rule(&rules, "café 100X").is_none());
    assert!(rules::matching_rule(&rules, "CAFÉ 10%_ receipt").is_some());
    assert!(rules::matching_rule(&rules, "unmatched").is_none());
    assert!("".parse::<rules::DescriptionPattern>().is_err());
    assert!(
        "account_01h455vb4pex5vsknk084sn02q"
            .parse::<rules::RuleId>()
            .is_err()
    );
    Ok(())
}

#[test]
fn csv_categorizes_both_directions_preserves_fallback_and_isolates_companies() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("starbucks", "5000", "0")?;
    ledger.create("deposit", "4000", "0")?;
    let missing_fallback = ledger.import(
        "acme",
        "bank.csv",
        "date,description,amount\n2026-03-15,Starbucks,-12.34\n",
        &["--account", "1000", "--suspense", "missing"],
    )?;
    assert_eq!(missing_fallback.status.code(), Some(3));
    assert_eq!(ledger.count("transactions")?, 0);
    let csv = "date,description,amount\n2026-03-15,STARBUCKS,-12.34\n2026-03-16,Deposit,20\n2026-03-17,Unmatched,-5\n";
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", csv, BANK)?)?["imported"],
        3
    );
    assert_eq!(
        data(&ledger.import("other", "bank.csv", csv, BANK)?)?["imported"],
        3
    );
    let db = Db::open_existing(&ledger.path, None)?;
    let mut stmt = db.conn().prepare("SELECT t.description, e.account_code, e.direction, e.amount, e.tax_category FROM entries e JOIN transactions t ON t.id = e.transaction_id WHERE t.company_slug = 'acme' AND e.account_code != '1000' ORDER BY t.id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        rows,
        vec![
            (
                "STARBUCKS".into(),
                "5000".into(),
                "debit".into(),
                1234,
                "dining".into()
            ),
            (
                "Deposit".into(),
                "4000".into(),
                "credit".into(),
                2000,
                "sales".into()
            ),
            (
                "Unmatched".into(),
                "9000".into(),
                "debit".into(),
                500,
                "fallback".into()
            ),
        ]
    );
    let other_fallback: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM entries WHERE company_slug = 'other' AND account_code = '9000'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(other_fallback, 3);
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.read())?;
    Ok(())
}

#[test]
fn csv_references_survive_rule_changes_and_dry_runs_do_not_mutate() -> TestResult {
    let ledger = Ledger::new()?;
    let csv = "date,description,amount\n2026-03-15,Coffee,-12.34\n";
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", csv, BANK)?)?["imported"],
        1
    );
    ledger.create("Coffee", "5000", "0")?;
    let audit_count = ledger.count("audit_log")?;
    assert_eq!(
        data(&ledger.import(
            "acme",
            "bank.csv",
            csv,
            &["--account", "1000", "--suspense", "9000", "--dry-run"]
        )?)?["skipped"],
        1
    );
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", csv, BANK)?)?["skipped"],
        1
    );
    let extra = format!("{csv}2026-03-16,Coffee,-1\n");
    assert_eq!(
        data(&ledger.import(
            "acme",
            "bank.csv",
            &extra,
            &["--account", "1000", "--suspense", "9000", "--dry-run"]
        )?)?["imported"],
        1
    );
    assert_eq!(ledger.count("transactions")?, 1);
    assert_eq!(ledger.count("audit_log")?, audit_count);
    let id = ledger.create("Coffee", "4000", "5")?[0]["id"]
        .as_str()
        .ok_or("missing ID")?
        .to_string();
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", &extra, BANK)?)?["imported"],
        1
    );
    data(
        &ledger
            .command("acme")?
            .args(["rule", "delete", &id, "--force"])
            .output()?,
    )?;
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", &extra, BANK)?)?["skipped"],
        2
    );
    Ok(())
}

#[test]
fn self_posting_rules_fail_csv_atomically_and_do_not_rewrite_json() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("Coffee", "1000", "0")?;
    let csv = "date,description,amount\n2026-03-15,Valid,-1\n2026-03-16,Coffee,-12.34\n";
    let output = ledger.import("acme", "bank.csv", csv, BANK)?;
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("selects bank account"));
    assert_eq!(ledger.count("transactions")?, 0);
    let json = json!([{"date":"2026-03-15", "description":"Coffee", "entries":[
        {"account_code":"5000", "direction":"debit", "amount":"12.34"},
        {"account_code":"1000", "direction":"credit", "amount":"12.34"}
    ]}])
    .to_string();
    assert_eq!(
        data(&ledger.import("acme", "journals.json", &json, &[])?)?["imported"],
        1
    );
    Ok(())
}

fn ofx() -> &'static str {
    "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n\
    <OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS><DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>\
    <BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS><STMTRS><CURDEF>USD\
    <BANKACCTFROM><BANKID>123<ACCTID>456<ACCTTYPE>CHECKING</BANKACCTFROM>\
    <BANKTRANLIST><DTSTART>20260101<DTEND>20260131\
    <STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260101<TRNAMT>-12.34<FITID>COFFEE<NAME>STARBUCKS<MEMO>Coffee</STMTTRN>\
    <STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260102<TRNAMT>20<FITID>PAYMENT<NAME>Customer<MEMO>Deposit</STMTTRN>\
    <STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260103<TRNAMT>-5<FITID>UNKNOWN<NAME>Unmatched</STMTTRN>\
    </BANKTRANLIST><LEDGERBAL><BALAMT>2.66<DTASOF>20260131</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>"
}

#[test]
fn ofx_matches_name_and_memo_applies_tax_defaults_and_preserves_references() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("starbucks", "5000", "0")?;
    ledger.create("deposit", "4000", "0")?;
    let count = ledger.count("audit_log")?;
    let dry = data(&ledger.import(
        "acme",
        "bank.ofx",
        ofx(),
        &["--account", "1000", "--suspense", "9000", "--dry-run"],
    )?)?;
    assert_eq!(dry["imported"], 3);
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("audit_log")?, count);
    assert_eq!(
        data(&ledger.import("acme", "bank.ofx", ofx(), BANK)?)?["imported"],
        3
    );
    ledger.create("starbucks", "4000", "10")?;
    assert_eq!(
        data(&ledger.import("acme", "bank.ofx", ofx(), BANK)?)?["skipped"],
        3
    );
    let db = Db::open_existing(&ledger.path, None)?;
    let mut stmt = db.conn().prepare("SELECT e.account_code, e.direction, e.amount, e.tax_category FROM entries e JOIN transactions t ON t.id = e.transaction_id WHERE e.account_code != '1000' ORDER BY t.id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        rows,
        vec![
            ("5000".into(), "debit".into(), 1234, "dining".into()),
            ("4000".into(), "credit".into(), 2000, "sales".into()),
            ("9000".into(), "debit".into(), 500, "fallback".into())
        ]
    );
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.read())?;
    Ok(())
}

#[test]
fn ofx_dry_run_and_posting_both_reject_self_posting_rules() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.create("starbucks", "1000", "0")?;
    for flags in [
        vec!["--account", "1000", "--suspense", "9000", "--dry-run"],
        BANK.to_vec(),
    ] {
        let result = data(&ledger.import("acme", "bank.ofx", ofx(), &flags)?)?;
        assert_eq!(result["errors"], 1);
        assert_eq!(result["imported"], 2);
        assert!(result.to_string().contains("selects bank account"));
    }
    assert_eq!(ledger.count("transactions")?, 2);
    Ok(())
}

#[test]
fn rule_mutations_roll_back_when_audit_recording_fails() -> TestResult {
    let db = Db::open_in_memory()?;
    companies::create_company(
        db.conn(),
        &Actor::new("fixture"),
        &"acme".parse()?,
        "Acme",
        None,
    )?;
    accounts::create_account(
        db.conn(),
        &Actor::new("fixture"),
        &CreateAccountParams {
            company_slug: &"acme".parse()?,
            code: "5000",
            name: "Dining",
            account_type: beankeeper::types::AccountType::Expense,
            default_tax_category: None,
        },
    )?;
    let actor = Actor::new("rules-test");
    db.conn().execute_batch("CREATE TRIGGER fail_rule_audit BEFORE INSERT ON audit_log WHEN NEW.entity = 'rule' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;")?;
    Granted::<Post>::with_db(&db, &actor, |grant| {
        assert!(rules::create_rule(grant, &"acme".parse()?, "coffee".parse()?, "5000", 0).is_err());
        Ok(())
    })?;
    assert!(
        rules::list_rules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?
        .is_empty()
    );
    db.conn().execute_batch("DROP TRIGGER fail_rule_audit;")?;
    let rule = Granted::<Post>::with_db(&db, &actor, |grant| {
        rules::create_rule(grant, &"acme".parse()?, "coffee".parse()?, "5000", 0)
    })?;
    db.conn().execute_batch("CREATE TRIGGER fail_rule_audit BEFORE INSERT ON audit_log WHEN NEW.entity = 'rule' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;")?;
    Granted::<Post>::with_db(&db, &actor, |grant| {
        assert!(
            audit::delete_rule(
                grant,
                &"acme".parse()?,
                &rule.id,
                beankeeper_cli::confirmation::Confirmed::forced(true).expect("forced confirmation")
            )
            .is_err()
        );
        Ok(())
    })?;
    assert_eq!(
        rules::list_rules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?
        .len(),
        1
    );
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn csv_skips_duplicates_before_evaluating_changed_rules() -> TestResult {
    let ledger = Ledger::new()?;
    let csv = "date,description,amount,reference\n2026-03-15,Coffee,-12.34,coffee-1\n";
    assert_eq!(
        data(&ledger.import("acme", "bank.csv", csv, BANK)?)?["imported"],
        1
    );
    ledger.create("coffee", "1000", "0")?;
    let overlap = format!("{csv}2026-03-16,Unmatched,-2,new-1\n");
    let result = data(&ledger.import("acme", "bank.csv", &overlap, BANK)?)?;
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["imported"], 1);
    assert_eq!(ledger.count("transactions")?, 2);
    let invalid_source = ledger.import(
        "acme",
        "bank.csv",
        "date,description,amount,reference\n2026-03-15,Coffee,0,coffee-1\n",
        BANK,
    )?;
    assert_eq!(invalid_source.status.code(), Some(3));
    Ok(())
}

#[test]
fn rule_output_formats_escape_values_and_respect_quiet() -> TestResult {
    let ledger = Ledger::new()?;
    let created = ledger.create("Coffee, shop", "5000", "0")?;
    let id = created[0]["id"].as_str().ok_or("missing ID")?;
    for format in ["csv", "table"] {
        let run = |quiet: bool| -> Result<Output, Box<dyn Error>> {
            let mut command = Command::cargo_bin("bk")?;
            command
                .arg("--db")
                .arg(&ledger.path)
                .args(["rule", "list", "--company", "acme", "--format", format])
                .env_remove("BEANKEEPER_PASSPHRASE_CMD")
                .env_remove("BK_AGENT_TOKEN");
            if quiet {
                command.arg("--quiet");
            }
            Ok(command.output()?)
        };
        let normal = run(false)?;
        assert!(normal.status.success());
        if format == "csv" {
            let mut reader = csv::Reader::from_reader(normal.stdout.as_slice());
            assert_eq!(
                reader.headers()?.iter().collect::<Vec<_>>(),
                ["id", "contains", "account_code", "priority"]
            );
            let rows = reader.records().collect::<Result<Vec<_>, _>>()?;
            assert_eq!(&rows[0][1], "coffee, shop");
        } else {
            assert!(String::from_utf8_lossy(&normal.stdout).contains("coffee, shop"));
        }
        let quiet = run(true)?;
        assert!(quiet.status.success());
        assert_eq!(quiet.stdout, b"");
    }
    let deleted = Command::cargo_bin("bk")?
        .arg("--db")
        .arg(&ledger.path)
        .args([
            "rule",
            "delete",
            id,
            "--company",
            "acme",
            "--format",
            "csv",
            "--force",
        ])
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BK_AGENT_TOKEN")
        .output()?;
    assert!(deleted.status.success());
    let mut reader = csv::Reader::from_reader(deleted.stdout.as_slice());
    assert_eq!(reader.headers()?.iter().collect::<Vec<_>>(), ["deleted"]);
    assert_eq!(&reader.records().next().ok_or("missing deletion")??[0], id);
    Ok(())
}
