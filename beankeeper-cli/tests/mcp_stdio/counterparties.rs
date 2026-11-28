use super::*;

#[test]
fn counterparties_use_the_shared_mcp_operations() -> TestResult {
    let directory = tempdir()?;
    let path = directory.path().join("books.db");
    seed(&path)?;
    let mut writable = bk(&path)?;
    writable.args(["mcp", "serve"]);
    let mut client = Client::spawn(writable)?;
    client.initialize()?;
    let created = client.success(
        "counterparty_create",
        &json!({
            "company":"acme", "name":"Clearwater", "role":"vendor",
            "email":"accounts@example.com", "payment_terms":30,
        }),
    )?;
    let id = created["party"]["id"].as_str().ok_or("missing party ID")?;
    let amended = client.success(
        "counterparty_amend",
        &json!({
            "company":"acme", "id":id, "patch":{"email":"clear", "payment_terms":{"set":45}}
        }),
    )?;
    assert_eq!(amended["party"]["payment_terms"], 45);
    assert!(amended["party"]["email"].is_null());

    let mut read_only = bk(&path)?;
    read_only.args(["mcp", "serve", "--read-only"]);
    let mut reader = Client::spawn(read_only)?;
    reader.initialize()?;
    assert_eq!(
        reader.success("counterparty_show", &json!({"company":"acme", "id":id}))?,
        amended["party"]
    );
    let listed = reader.request("tools/list", &json!({}))?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "counterparty_list"));
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "counterparty_create")
    );
    Ok(())
}
