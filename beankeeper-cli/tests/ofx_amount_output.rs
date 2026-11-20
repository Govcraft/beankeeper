//! OFX import outcomes retain the statement currency for decimal presentation.

use std::error::Error;
use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{accounts, companies, connection::Db};
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

fn import(db: &Path, file: &Path, flags: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("bk")?
        .arg("--db")
        .arg(db)
        .args(["--json", "--company", "acme", "txn", "import", "--file"])
        .arg(file)
        .args(["--account", "1000", "--suspense", "4000"])
        .args(flags)
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
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
    companies::create_company(db.conn(), "acme", "Acme", None)?;
    for (code, kind) in [("1000", "asset"), ("4000", "revenue")] {
        accounts::create_account(db.conn(), "acme", code, code, kind, None)?;
    }
    for (currency, amount, minor) in [
        ("USD", "12.34", 1234),
        ("JPY", "123", 123),
        ("BHD", "1.234", 1234),
    ] {
        let file = dir.path().join(format!("{currency}.ofx"));
        std::fs::write(&file, statement(currency, amount))?;
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
