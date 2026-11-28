//! Verify that MCP and CLI share aggregate payroll review and funding.

use std::{fs, path::Path};

use serde_json::{Value, json};
use tempfile::tempdir;

use super::{Client, TestResult, bk, seed};

fn prepare(path: &Path) -> TestResult {
    seed(path)?;
    for (code, kind) in [("5000", "expense"), ("2000", "liability")] {
        let output = bk(path)?
            .args([
                "--company",
                "acme",
                "account",
                "create",
                code,
                code,
                "--type",
                kind,
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn aggregate() -> String {
    json!({
        "run": {
            "key": {"provider":"adapter", "organization":"org-1", "run":"aggregate-mcp-1"},
            "revision":1, "kind":"regular", "currency":"USD", "total_minor":10000,
            "period":{"start":"2026-01-01", "end":"2026-01-14"}, "pay_date":"2026-01-16",
            "components":[{"component":"gross_wages","amount_minor":10000}, {"component":"net_pay","amount_minor":10000}]
        },
        "accounts":[{"component":"gross_wages","code":"5000"}, {"component":"net_pay","code":"2000"}]
    }).to_string()
}

#[test]
fn payroll_tool_catalog_supports_reviewed_import_and_funding() -> TestResult {
    let tmp = tempdir()?;
    let books = tmp.path().join("payroll-mcp.db");
    prepare(&books)?;
    let file = tmp.path().join("aggregate.json");
    let export = aggregate();
    fs::write(&file, &export)?;
    let mut command = bk(&books)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let args = json!({"company":"acme","export_json":export});
    let preview = client.success("payroll_preview", &args)?;
    let cli = bk(&books)?
        .args([
            "--json",
            "--company",
            "acme",
            "payroll",
            "preview",
            "--file",
        ])
        .arg(file)
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli: Value = serde_json::from_slice(&cli.stdout)?;
    assert_eq!(cli["data"], preview);
    let mut import = args;
    import["digest"] = preview["digest"].clone();
    let payroll = client.success("payroll_import", &import)?;
    assert_eq!(payroll["status"], "posted");
    let again = client.success("payroll_import", &import)?;
    assert_eq!(again["existing"], true);
    assert_eq!(payroll["transaction_id"], again["transaction_id"]);
    fund(&mut client, &payroll["payroll_id"])
}

fn fund(client: &mut Client, id: &Value) -> TestResult {
    let selector = json!({"company":"acme", "payroll":id});
    let before = client.success("payroll_show", &selector)?;
    let payment = client.success(
        "txn_post",
        &json!({"company":"acme", "date":"2026-01-17", "currency":"USD",
        "description":"Aggregate withdrawal", "debits":[{"account":"2000","amount":"25"}],
        "credits":[{"account":"1000","amount":"25"}]}),
    )?;
    let txn = client.success("txn_show", &json!({"company":"acme","id":payment["id"]}))?;
    let line = txn[0]["entries"]
        .as_array()
        .ok_or("missing withdrawal lines")?
        .iter()
        .find(|entry| entry["account_code"] == "2000")
        .ok_or("missing debit")?;
    let mut funding = selector.clone();
    funding["liability_entry"] = before["liabilities"][0]["entry_id"].clone();
    funding["payment_transaction"] = payment["id"].clone();
    funding["payment_entry"] = line["id"].clone();
    funding["amount_minor"] = json!(2500);
    client.success("payroll_fund", &funding)?;
    let result = client.success("payroll_show", &selector)?;
    assert_ne!(before["liabilities"], result["liabilities"]);
    Ok(())
}
