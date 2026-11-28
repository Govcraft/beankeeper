//! Preview finalized payroll data through the CLI and validate control totals.

use std::{error::Error, fs, path::Path, process::Command};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};

fn bk(db: &Path) -> Result<Command, Box<dyn Error>> {
    let mut command = Command::cargo_bin("bk")?;
    command
        .env_remove("BK_AGENT_TOKEN")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .args(["--json", "--db"])
        .arg(db);
    Ok(command)
}

fn run(command: &mut Command) -> Result<Value, Box<dyn Error>> {
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn payroll() -> Value {
    json!({
        "run": {
            "key": {"provider":"adapter", "organization":"org-1", "run":"run-1"},
            "revision": 1, "kind": "regular",
            "period": {"start":"2026-01-01","end":"2026-01-14"}, "pay_date":"2026-01-16",
            "currency": "USD", "total_minor": 10_000,
            "components":[
                {"component":"net_pay", "amount_minor":8_000},
                {"component":"gross_wages", "amount_minor":10_000},
                {"component":"tax_withholdings", "amount_minor":2_000}
            ]
        },
        "accounts":[
            {"component":"gross_wages","code":"5000"},
            {"component":"net_pay","code":"2000"},
            {"component":"tax_withholdings","code":"2100"}
        ]
    })
}

#[test]
fn payroll_preview_reconciles_both_sides_to_the_provider_total() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::tempdir()?;
    let db = tmp.path().join("books.db");
    let source = tmp.path().join("export.json");
    run(bk(&db)?.args(["init"]))?;
    run(bk(&db)?.args(["company", "create", "acme", "Acme"]))?;
    for (code, kind) in [
        ("1000", "asset"),
        ("5000", "expense"),
        ("2000", "liability"),
        ("2100", "liability"),
    ] {
        run(bk(&db)?.args([
            "--company",
            "acme",
            "account",
            "create",
            code,
            code,
            "--type",
            kind,
        ]))?;
    }
    let mut export = payroll();
    fs::write(&source, serde_json::to_vec(&export)?)?;
    let preview = |db: &Path| -> Result<Command, Box<dyn Error>> {
        let mut command = bk(db)?;
        command
            .args(["--company", "acme", "payroll", "preview", "--file"])
            .arg(&source);
        Ok(command)
    };
    let result = run(&mut preview(&db)?)?;
    assert_eq!(result["data"]["entries"][0]["amount"], 10_000);
    assert_eq!(result["data"]["entries"][1]["amount_display"], "80.00");
    assert_eq!(result["data"]["entries"][2]["amount_display"], "20.00");
    let transactions = run(bk(&db)?.args(["--company", "acme", "txn", "list"]))?;
    assert_eq!(transactions["data"], json!([]));

    let digest = result["data"]["digest"]
        .as_str()
        .ok_or("missing preview digest")?;
    let import = || -> Result<Command, Box<dyn Error>> {
        let mut command = bk(&db)?;
        command
            .args([
                "--company",
                "acme",
                "payroll",
                "import",
                "--digest",
                digest,
                "--file",
            ])
            .arg(&source);
        Ok(command)
    };
    let original = run(&mut import()?)?;
    let retry = run(&mut import()?)?;
    assert_eq!(original["data"]["status"], "posted");
    assert_eq!(original["data"]["existing"], false);
    assert_eq!(retry["data"]["existing"], true);
    assert_eq!(original["data"]["payroll_id"], retry["data"]["payroll_id"]);
    assert_eq!(
        original["data"]["transaction_id"],
        retry["data"]["transaction_id"]
    );
    run(bk(&db)?.arg("verify"))?;

    check_funding(&db, &original["data"])?;

    export["run"]["total_minor"] = json!(9999);
    fs::write(&source, serde_json::to_vec(&export)?)?;
    let failure = preview(&db)?.output()?;
    assert!(!failure.status.success());
    let failure: Value = serde_json::from_slice(&failure.stdout)?;
    assert_eq!(failure["error"]["code"], "VALIDATION");
    assert!(
        failure["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("9999 minor units"))
    );
    Ok(())
}

fn check_funding(db: &Path, payroll: &Value) -> Result<(), Box<dyn Error>> {
    let id = payroll["payroll_id"].as_str().ok_or("missing payroll ID")?;
    let show = || -> Result<Value, Box<dyn Error>> {
        run(bk(db)?.args(["--company", "acme", "payroll", "show", "--payroll", id]))
    };
    let before = show()?;
    let liability_id = before["data"]["liabilities"][0]["entry_id"].to_string();
    let cash = run(bk(db)?.args([
        "--company",
        "acme",
        "txn",
        "post",
        "-d",
        "Provider withdrawal",
        "--debit",
        "2000:25",
        "--credit",
        "1000:25",
    ]))?;
    let payment_id = cash["data"]["id"]
        .as_i64()
        .ok_or("missing withdrawal ID")?
        .to_string();
    let details = run(bk(db)?.args(["--company", "acme", "txn", "show", &payment_id]))?;
    let payment_entry = details["data"][0]["entries"]
        .as_array()
        .ok_or("missing funding entries")?
        .iter()
        .find(|entry| entry["account_code"] == "2000")
        .ok_or("missing liability line")?["id"]
        .to_string();
    let allocation = |amount: &str| -> Result<Command, Box<dyn Error>> {
        let mut command = bk(db)?;
        command.args([
            "--company",
            "acme",
            "payroll",
            "fund",
            "--payroll",
            id,
            "--liability-entry",
            &liability_id,
            "--payment-transaction",
            &payment_id,
            "--payment-entry",
            &payment_entry,
            "--amount-minor",
            amount,
        ]);
        Ok(command)
    };
    assert!(!allocation("2501")?.output()?.status.success());
    let first = run(&mut allocation("2500")?)?;
    assert_eq!(
        run(&mut allocation("2500")?)?["data"]["id"],
        first["data"]["id"]
    );
    let after = show()?;
    assert_eq!(after["data"]["liabilities"][0]["outstanding"], 5500);
    assert_eq!(
        after["data"]["allocations"].as_array().map(Vec::len),
        Some(1)
    );
    run(bk(db)?.arg("verify"))?;
    Ok(())
}
