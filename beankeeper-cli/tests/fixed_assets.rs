//! Fixed asset workflow tests through the public CLI.

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

type TestError = Box<dyn std::error::Error>;

struct Ledger {
    dir: TempDir,
}

impl Ledger {
    fn run(&self, args: &[&str]) -> Result<Output, TestError> {
        Ok(Command::cargo_bin("bk")?
            .arg("--db")
            .arg(self.dir.path().join("books.db"))
            .args(["--company", "acme", "--json"])
            .args(args)
            .output()?)
    }

    fn data(&self, args: &[&str]) -> Result<Value, TestError> {
        let out = self.run(args)?;
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let envelope: Value = serde_json::from_slice(&out.stdout)?;
        Ok(envelope["data"].clone())
    }

    fn new() -> Result<Self, TestError> {
        let ledger = Self {
            dir: tempfile::tempdir()?,
        };
        ledger.data(&["init"])?;
        ledger.data(&["company", "create", "acme", "Acme"])?;
        for (code, name, ty) in [
            ("1000", "Cash", "asset"),
            ("1200", "Equipment", "asset"),
            ("1210", "Accumulated depreciation", "asset"),
            ("5100", "Depreciation", "expense"),
            ("5900", "Disposal loss", "expense"),
            ("4900", "Disposal gain", "revenue"),
            ("3000", "Equity", "equity"),
        ] {
            ledger.data(&["account", "create", code, name, "--type", ty])?;
        }
        Ok(ledger)
    }

    fn path(&self, name: &str) -> impl AsRef<Path> {
        self.dir.path().join(name)
    }
}

const REGISTER: &[&str] = &[
    "fixed-asset",
    "register",
    "--name",
    "Lathe",
    "--currency",
    "USD",
    "--acquired-on",
    "2026-01-15",
    "--in-service",
    "2026-01-15",
    "--cost-minor",
    "105",
    "--salvage-minor",
    "5",
    "--life-months",
    "3",
    "--cost-account",
    "1200",
    "--accumulated-account",
    "1210",
    "--expense-account",
    "5100",
    "--offset-account",
    "1000",
    "--reference",
    "lathe-purchase",
];

#[test]
fn fixed_asset_lifecycle_reconciles_and_restores() -> Result<(), TestError> {
    let ledger = Ledger::new()?;
    let issued = ledger.data(REGISTER)?;
    let id = issued["asset"]["id"].as_str().ok_or("fixed asset id")?;
    assert!(id.starts_with("asset_"));
    assert_eq!(ledger.data(REGISTER)?["asset"]["id"], id);
    let projected = ledger.data(&["fixed-asset", "preview", id, "--through", "2026-03-31"])?;
    assert_eq!(projected[0]["amount"], 33);
    assert_eq!(projected[0]["date"], "2026-02-28");
    assert_eq!(projected[1]["amount"], 33);
    let posted = ledger.data(&["fixed-asset", "depreciate", id, "--through", "2026-02-28"])?;
    assert!(posted[0]["transaction_id"].is_number());
    assert_eq!(
        ledger.data(&["fixed-asset", "depreciate", id, "--through", "2026-02-28"])?,
        serde_json::json!([])
    );
    let archive = ledger.path("before-disposal.db");
    let archive = archive.as_ref().to_str().ok_or("backup path")?;
    ledger.data(&["backup", "--output", archive])?;
    let sale = [
        "fixed-asset",
        "dispose",
        id,
        "--date",
        "2026-03-12",
        "--proceeds-minor",
        "75",
        "--proceeds-account",
        "1000",
        "--gain-account",
        "4900",
        "--loss-account",
        "5900",
    ];
    let sold = ledger.data(&sale)?;
    assert_eq!(sold["book_value"], 0);
    assert_eq!(sold["disposal_gain_loss"], 3);
    assert_eq!(ledger.data(&sale)?, sold);
    let past = ledger.data(&["fixed-asset", "show", id, "--as-of", "2026-02-28"])?;
    assert_eq!(past["book_value"], 72);
    let register = ledger.data(&["fixed-asset", "list", "--as-of", "2026-04-01"])?;
    assert_eq!(register["rollforward"]["disposed_book_value_minor"], 72);
    assert_eq!(register["rollforward"]["ending_book_value_minor"], 0);
    for control in register["control_accounts"]
        .as_array()
        .ok_or("control accounts")?
    {
        assert_eq!(control["difference_minor"], 0);
    }
    assert_eq!(ledger.data(&["verify"])?["status"], "healthy");
    let reversed = ledger.run(&["txn", "reverse", &posted[0]["transaction_id"].to_string()])?;
    assert!(!reversed.status.success());
    let failure = format!(
        "{}{}",
        String::from_utf8_lossy(&reversed.stdout),
        String::from_utf8_lossy(&reversed.stderr)
    );
    assert!(
        failure.contains("fixed asset movements cannot be reversed"),
        "{failure}"
    );
    ledger.data(&["restore", "--input", archive])?;
    let restored = ledger.data(&["fixed-asset", "show", id, "--as-of", "2026-04-01"])?;
    assert_eq!(restored["book_value"], 72);
    assert_eq!(restored["movements"].as_array().map(Vec::len), Some(1));
    assert_eq!(ledger.data(&["verify"])?["status"], "healthy");
    Ok(())
}

#[test]
fn existing_opening_acquisition_and_locked_charge_are_atomic() -> Result<(), TestError> {
    let ledger = Ledger::new()?;
    let journal = ledger.data(&[
        "txn",
        "post",
        "-d",
        "Opening equipment",
        "--date",
        "2026-01-01",
        "--currency",
        "USD",
        "--debit",
        "1200:105",
        "--credit",
        "1210:5",
        "--credit",
        "3000:100",
    ])?;
    let id = journal["id"].to_string();
    let issued = ledger.data(&[
        "fixed-asset",
        "register",
        "--name",
        "Opening asset",
        "--currency",
        "USD",
        "--acquired-on",
        "2026-01-01",
        "--in-service",
        "2026-02-01",
        "--cost-minor",
        "10500",
        "--opening-depreciation-minor",
        "500",
        "--life-months",
        "2",
        "--cost-account",
        "1200",
        "--accumulated-account",
        "1210",
        "--expense-account",
        "5100",
        "--acquisition-transaction",
        &id,
    ])?;
    let asset = issued["asset"]["id"].as_str().ok_or("fixed asset id")?;
    assert_eq!(issued["book_value"], 10000);
    ledger.data(&["period", "close", "--through", "2026-02-28"])?;
    let denied = ledger.run(&[
        "fixed-asset",
        "depreciate",
        asset,
        "--through",
        "2026-03-31",
    ])?;
    assert!(!denied.status.success());
    let error: Value = serde_json::from_slice(&denied.stderr)?;
    assert_eq!(error["error"]["code"], "VALIDATION");
    let view = ledger.data(&["fixed-asset", "show", asset, "--as-of", "2026-03-31"])?;
    assert_eq!(view["book_value"], 10000);
    assert_eq!(view["movements"], serde_json::json!([]));
    Ok(())
}
