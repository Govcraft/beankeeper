use super::*;

const READS: &[&str] = &[
    "project_list",
    "project_show",
    "project_time",
    "project_unbilled",
    "project_report",
];

const WRITES: &[&str] = &[
    "project_create",
    "project_archive",
    "project_record",
    "project_approve",
    "project_split",
    "project_budget",
    "project_draft_invoice",
    "project_cancel_invoice",
];

#[test]
fn project_catalog_preserves_read_only_ceiling() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve", "--read-only"]);
    let mut reader = Client::spawn(command)?;
    reader.initialize()?;
    let catalog = reader.request("tools/list", &json!({}))?;
    let names = tool_names(&catalog)?;
    for read in READS {
        assert!(names.contains(read), "missing {read}");
    }
    for write in WRITES {
        assert!(!names.contains(write), "unexpected {write}");
    }
    assert_eq!(
        reader.success("project_list", &json!({"company":"acme"}))?,
        json!([])
    );
    Ok(())
}

#[test]
fn project_billing_and_report_share_cli_and_mcp_results() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let customer = client.success(
        "counterparty_create",
        &json!({
            "company":"acme","name":"Bayside","role":"customer","payment_terms":30,
        }),
    )?;
    let project = client.success("project_create",&json!({
        "company":"acme","terms":{"code":"oak","name":"Oak","customer":customer["party"]["id"],"currency":"USD"},
    }))?;
    let id = &project["project"]["id"];
    let work = client.success("project_record",&json!({
        "company":"acme","work":{"project":id,"date":"2026-01-02","description":"Work","minutes":60,"rate_minor":8000},
    }))?;
    client.success(
        "project_approve",
        &json!({
            "company":"acme","id":work["work"]["id"],"date":"2026-01-03",
        }),
    )?;
    let dates = json!({"company":"acme","id":id,"from":"2026-01-01","through":"2026-01-31"});
    let source = client.success("project_unbilled", &dates)?;
    assert_eq!(source[0]["amount_minor"], 8000);
    let invoice = client.success("project_draft_invoice",&json!({
        "company":"acme","invoice":{"project":id,"number":"JOB-1","receivable_account":"1000",
            "revenue_account":"4000","from":"2026-01-01","through":"2026-01-10","sources":[source[0]["source"]]},
    }))?;
    client.success(
        "document_issue",
        &json!({
            "company":"acme","id":invoice["result"]["record"]["id"],"date":"2026-01-11",
        }),
    )?;
    assert_eq!(client.success("project_unbilled", &dates)?, json!([]));
    let report = client.success(
        "project_report",
        &json!({
            "company":"acme","month":"2026-01","currency":"USD","detail":true,
        }),
    )?;
    assert_eq!(report["projects"][0]["revenue_minor"], 8000);
    let output = bk(&path)?
        .args([
            "--company",
            "acme",
            "--json",
            "project",
            "report",
            "--month",
            "2026-01",
            "--currency",
            "USD",
            "--detail",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report, cli["data"]);
    client.success("verify", &json!({}))?;
    Ok(())
}
