use std::{fs, path::Path};

use beankeeper_cli::db::Db;
use serde_json::{Value, json};

use super::{Client, TestResult, bk, identity_ledger, proposal_identity, tempdir, tool_names};

fn fixture_permissions(path: &Path, mode: u32) -> TestResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[test]
fn protected_tax_tools_take_host_artifact_names_and_keep_w9_out_of_mcp_results() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    let passphrase = dir.path().join("passphrase");
    let read = identity_ledger(&path, &passphrase)?;
    let db = Db::open_existing(&path, Some(&"mcp-test-passphrase".into()))?;
    let owner = proposal_identity(
        &db,
        "tax_preparer",
        &["ledger/read", "ledger/admin", "ledger/tax-details"],
    )?;
    drop(db);
    let root = dir.path().join("artifacts");
    let company = root.join("startup").join("acme");
    fs::create_dir_all(&company)?;
    for folder in [&root, &root.join("startup"), &company] {
        fixture_permissions(folder, 0o700)?;
    }
    let input = company.join("w9.json");
    fs::write(
        &input,
        json!({"legal_name":"Restricted MCP Fixture","taxpayer_id":"000-10-0001"}).to_string(),
    )?;
    fixture_permissions(&input, 0o600)?;

    let session = |identity: &Value| -> Result<Client, Box<dyn std::error::Error>> {
        let mut command = bk(&path)?;
        command
            .arg("--passphrase-file")
            .arg(&passphrase)
            .env("BEANKEEPER_CONTRACTOR_FILES", &root)
            .env(
                "BK_AGENT_TOKEN",
                identity["token"].as_str().ok_or("missing token")?,
            )
            .args(["mcp", "serve"]);
        let mut client = Client::spawn(command)?;
        client.initialize()?;
        Ok(client)
    };
    let mut preparer = session(&owner)?;
    let party = preparer.success(
        "counterparty_create",
        &json!({"company":"acme","name":"MCP Fixture","role":"vendor"}),
    )?;
    let profile = preparer.success("contractor_tax_w9", &json!({
        "company":"acme","party":party["party"]["id"],"classification":"individual","input":"w9.json"
    }))?;
    assert_eq!(profile["w9_status"], "verified");
    assert!(!profile.to_string().contains("Restricted MCP Fixture"));
    assert!(!profile.to_string().contains("000100001"));
    let escaped = preparer.call("contractor_tax_w9", &json!({
        "company":"acme","party":party["party"]["id"],"classification":"individual","input":"../w9.json"
    }))?;
    assert_eq!(escaped["result"]["isError"], true, "{escaped}");

    let export = preparer.success(
        "contractor_tax_export",
        &json!({"company":"acme","year":2025,"output":"worksheet.json"}),
    )?;
    assert_eq!(export["written"], "worksheet.json");
    assert!(company.join("worksheet.json").is_file());

    let mut reader = session(&read)?;
    let worksheet = reader.success(
        "contractor_tax_worksheet",
        &json!({"company":"acme","year":2025}),
    )?;
    assert_eq!(worksheet["currency"], "USD");
    let listed = reader.request("tools/list", &json!({}))?;
    let allowed = tool_names(&listed)?;
    assert!(allowed.contains("contractor_tax_worksheet"));
    assert!(!allowed.contains("contractor_tax_w9"));
    assert!(!allowed.contains("contractor_tax_export"));
    Ok(())
}
