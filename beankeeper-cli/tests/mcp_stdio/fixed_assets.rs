use super::*;

#[test]
fn fixed_assets_cli_and_mcp_share_read_and_write_results() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    for (code, name, kind) in [
        ("1230", "Equipment", "asset"),
        ("1231", "Accumulated depreciation", "asset"),
        ("5230", "Depreciation", "expense"),
    ] {
        client.success(
            "account_create",
            &json!({"company":"acme","code":code,"name":name,"account_type":kind}),
        )?;
    }
    let asset = client.success("fixed_asset_register", &json!({
        "company":"acme", "name":"Milling machine", "currency":"USD",
        "acquired_on":"2026-01-01", "in_service":"2026-01-01", "cost_minor":1000,
        "life_months":3, "cost_account":"1230","accumulated_account":"1231","expense_account":"5230",
        "offset_account":"1000","reference":"machine-1"
    }))?;
    let id = asset["asset"]["id"].as_str().ok_or("asset ID")?;
    let input = json!({"company":"acme","id":id,"date":"2026-01-31"});
    let preview = client.success("fixed_asset_preview", &input)?;
    assert_eq!(preview[0]["amount"], 333);
    let posting = client.success("fixed_asset_depreciate", &input)?;
    assert_eq!(posting[0]["amount"], 333);
    let mcp = client.success("fixed_asset_show", &input)?;
    assert_eq!(mcp["book_value"], 667);
    let cli = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "fixed-asset",
            "show",
            id,
            "--as-of",
            "2026-01-31",
        ])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let response: Value = serde_json::from_slice(&cli.stdout)?;
    assert_eq!(response["data"], mcp);

    let mut reader = bk(&path)?;
    reader.args(["mcp", "serve", "--read-only"]);
    let mut reader = Client::spawn(reader)?;
    reader.initialize()?;
    let names = tool_names(&reader.request("tools/list", &json!({}))?)?;
    assert!(names.contains("fixed_asset_show"));
    assert!(!names.contains("fixed_asset_depreciate"));
    Ok(())
}
