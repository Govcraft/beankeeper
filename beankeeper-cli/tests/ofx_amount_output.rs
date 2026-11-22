//! OFX import outcomes retain the statement currency for decimal presentation.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{Actor, accounts, companies, connection::Db};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

fn statement(currency: &str, amount: &str) -> String {
    format!(
        "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n\
        <OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS>\
        <DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>\
        <BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>\
        <STMTRS><CURDEF>{currency}<BANKACCTFROM><BANKID>123<ACCTID>456\
        <ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20260101<DTEND>20260131\
        <STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260101<TRNAMT>{amount}<FITID>AMOUNT-TEST\
        <NAME>Sale</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>{amount}\
        <DTASOF>20260131</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>"
    )
}

fn import_command(db: &Path, file: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command
        .arg("--db")
        .arg(db)
        .args(["--company", "acme", "txn", "import", "--file"])
        .arg(file)
        .args(["--account", "1000", "--suspense", "4000"])
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY");
    Ok(command)
}

fn import(db: &Path, file: &Path, flags: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = import_command(db, file)?
        .arg("--json")
        .args(flags)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    Ok(envelope["data"].clone())
}

#[test]
fn ofx_dry_run_created_skipped_and_failed_amounts_use_statement_currency() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    let db = Db::open(&path, None)?;
    companies::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("4000", beankeeper::types::AccountType::Revenue),
    ] {
        accounts::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code,
                name: code,
                account_type: kind,
                default_tax_category: None,
            },
        )?;
    }
    for (currency, amount, minor) in [
        ("USD", "12.34", 1234),
        ("JPY", "123", 123),
        ("BHD", "1.234", 1234),
    ] {
        let file = dir.path().join(format!("{currency}.ofx"));
        std::fs::write(&file, statement(currency, amount))?;
        let verbose = import_command(&path, &file)?
            .args(["--dry-run", "--verbose"])
            .output()?;
        assert!(verbose.status.success());
        let text = String::from_utf8(verbose.stderr)?;
        assert!(
            text.lines()
                .any(|line| line.ends_with(&format!("+{amount}"))),
            "{text}"
        );
        for (flags, status) in [
            (vec!["--dry-run"], "imported"),
            (vec![], "imported"),
            (vec!["--on-conflict", "skip"], "skipped"),
            (vec!["--on-conflict", "error"], "error"),
        ] {
            let data = import(&path, &file, &flags)?;
            let transaction = &data["transactions"][0];
            assert_eq!(transaction["status"], status, "{data}");
            assert_eq!(transaction["currency"], currency, "{data}");
            assert_eq!(transaction["amount"], minor, "{data}");
            assert_eq!(transaction["amount_display"], amount, "{data}");
        }
    }
    Ok(())
}

#[test]
fn ofx_closed_date_is_reported_as_a_failed_line_without_posting() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    let db = Db::open(&path, None)?;
    companies::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("4000", beankeeper::types::AccountType::Revenue),
    ] {
        accounts::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code,
                name: code,
                account_type: kind,
                default_tax_category: None,
            },
        )?;
    }
    beankeeper_cli::db::audit::change_period(
        db.conn(),
        &beankeeper_cli::db::Actor::new("owner"),
        "acme",
        beankeeper_cli::db::periods::PeriodChange::Close("2026-01-31".parse()?),
    )?;
    let before: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
    let file = dir.path().join("closed.ofx");
    std::fs::write(&file, statement("USD", "12.34"))?;
    let data = import(&path, &file, &[])?;
    assert_eq!(data["transactions"][0]["status"], "error");
    assert!(
        data["transactions"][0]["reason"]
            .as_str()
            .is_some_and(|message| message.contains("closed period")),
        "{data}"
    );
    let state: (i64, i64) = db.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM transactions), (SELECT COUNT(*) FROM audit_log)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(state, (0, before));
    beankeeper_cli::commands::verify::execute_json(&db)?;
    Ok(())
}
