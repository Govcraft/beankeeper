use super::*;

#[test]
fn cash_forecast_source_links_share_cli_and_mcp() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    for args in [
        &[
            "account",
            "create",
            "2100",
            "Payables",
            "--type",
            "liability",
        ][..],
        &["account", "create", "5000", "Rent", "--type", "expense"],
        &[
            "schedule",
            "create",
            "-d",
            "Rent",
            "--start",
            "2026-02-08",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
        ],
    ] {
        bk(&path)?
            .args(["--company", "acme"])
            .args(args)
            .assert()
            .success();
    }
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let party = client.success(
        "counterparty_create",
        &json!({
            "company":"acme","name":"Rent vendor","role":"vendor",
        }),
    )?;
    let document = client.success("document_draft", &json!({
        "company":"acme",
        "terms":{"kind":"bill","number":"RENT","counterparty_id":party["party"]["id"],
            "control_account":"2100","due_date":"2026-02-08",
            "lines":[{"description":"Rent","quantity":"1","unit_price_minor":10000,"account":"5000"}]},
    }))?;
    let id = document["record"]["id"]
        .as_str()
        .ok_or("missing document ID")?;
    client.success(
        "document_issue",
        &json!({"company":"acme","id":id,"date":"2026-02-01"}),
    )?;
    let mut forecast = json!({
        "company":"acme","kind":"cash-forecast","as_of":"2026-02-01","through":"2026-02-20","cadence":"monthly",
    });
    let baseline = client.success("report", &forecast)?;
    let schedule = baseline["possible_overlaps"][0]["scheduled_source_id"]
        .as_str()
        .ok_or("missing scheduled source")?;
    let link = format!("{schedule}=open-item:{id}");
    forecast["link_sources"] = json!([link]);
    let linked = client.success("report", &forecast)?;
    assert_eq!(linked["events"].as_array().map(Vec::len), Some(1));
    assert_eq!(linked["linked_sources"][0]["covered"], -10_000);
    let output = bk(&path)?
        .args([
            "--company",
            "acme",
            "--json",
            "report",
            "cash-forecast",
            "--as-of",
            "2026-02-01",
            "--through",
            "2026-02-20",
            "--cadence",
            "monthly",
            "--link-source",
            &link,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(linked, cli["data"]);
    Ok(())
}
