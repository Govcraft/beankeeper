use super::*;

#[test]
fn inventory_tools_share_cli_storage_and_apply_the_read_only_ceiling() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    for (code, name, kind) in [
        ("1300", "Inventory", "asset"),
        ("2100", "Receiving clearing", "liability"),
        ("5000", "Cost of goods sold", "expense"),
        ("5100", "Inventory adjustment", "expense"),
    ] {
        bk(&path)?
            .args([
                "--company",
                "acme",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ])
            .assert()
            .success();
    }
    let mut serve = bk(&path)?;
    serve.args(["mcp", "serve"]);
    let mut writer = Client::spawn(serve)?;
    writer.initialize()?;
    let vendor = writer.success(
        "counterparty_create",
        &json!({"company":"acme","name":"Synthetic supplier","role":"vendor"}),
    )?;
    let item = writer.success(
        "inventory_register",
        &json!({
            "company":"acme",
            "terms":{
                "sku":"widget-1","name":"Widget","unit":"piece","currency":"USD",
                "stock_account":"1300","cogs_account":"5000",
                "receiving_clearing":"2100","adjustment_account":"5100"
            }
        }),
    )?;
    let receive = json!({
        "company":"acme",
        "request":{"item":item["id"],"quantity":"3","date":"2026-01-02","reference":"delivered",
                   "movement":{"kind":"receipt","vendor":vendor["party"]["id"],"total_minor":101}}
    });
    let received = writer.success("inventory_move", &receive)?;
    assert_eq!(received, writer.success("inventory_move", &receive)?);
    let issued = writer.success(
        "inventory_move",
        &json!({
            "company":"acme",
            "request":{"item":item["id"],"quantity":"1","date":"2026-01-03","reference":"shipped",
                       "movement":{"kind":"issue"}}
        }),
    )?;
    assert_eq!(issued["cost_minor"], 34);

    let output = bk(&path)?
        .args([
            "--json",
            "--company",
            "acme",
            "inventory",
            "valuation",
            "--as-of",
            "2026-01-03",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    let mut serve_read_only = bk(&path)?;
    serve_read_only.args(["mcp", "serve", "--read-only"]);
    let mut reader = Client::spawn(serve_read_only)?;
    reader.initialize()?;
    assert_eq!(
        reader.success(
            "inventory_valuation",
            &json!({"company":"acme","as_of":"2026-01-03"})
        )?,
        cli["data"]
    );
    assert_eq!(cli["data"]["positions"][0]["value_minor"], 67);
    assert_read_only_inventory_catalog(&mut reader)
}

fn assert_read_only_inventory_catalog(reader: &mut Client) -> TestResult {
    let response = reader.request("tools/list", &json!({}))?;
    let advertised: BTreeSet<_> = response["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("inventory_"))
        .collect();
    assert_eq!(
        advertised,
        BTreeSet::from([
            "inventory_list",
            "inventory_show",
            "inventory_movement",
            "inventory_movements",
            "inventory_valuation",
        ])
    );
    Ok(())
}
