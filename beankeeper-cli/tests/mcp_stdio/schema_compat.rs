use super::{
    Client, TestResult, assert_parameter_rejection, bk, identity_client, identity_ledger, payload,
    posting, proposal_identity, tempdir,
};
use assert_cmd::prelude::*;
use beankeeper_cli::db::Db;
use serde_json::{Value, json};
use std::error::Error;

#[test]
fn tools_list_inlines_structured_input_schemas() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("schema.db");
    bk(&path)?.arg("init").assert().success();
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let listed = client.request("tools/list", &json!({}))?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    for tool in tools {
        let name = tool["name"].as_str().ok_or("missing tool name")?;
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_inline_schema(name, &tool["inputSchema"]);
    }

    let properties = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| &tool["inputSchema"]["properties"])
            .ok_or_else(|| format!("missing {name}"))
    };
    let setup = properties("ledger_setup_book_initialize")?;
    let post = properties("txn_post")?;
    let statement = properties("ledger_setup_statement_import")?;
    let policy = properties("bookkeeping_policy_set")?;
    assert_eq!(
        json!({
            "target_type": setup["target"]["type"],
            "target_required": setup["target"]["required"],
            "company_type": setup["target"]["properties"]["company"]["type"],
            "generation_type": setup["target"]["properties"]["generation"]["type"],
            "settings_type": setup["settings"]["type"],
            "settings_required": setup["settings"]["required"],
            "csv_type": statement["csv"]["type"],
            "debits_type": post["debits"]["type"],
            "debits_min_items": post["debits"]["minItems"],
            "line_type": post["debits"]["items"]["type"],
            "line_required": post["debits"]["items"]["required"],
            "amount_type": post["debits"]["items"]["properties"]["amount"]["type"],
            "meta_type": post["meta"]["type"],
            "policy_type": policy["policy"]["type"],
        }),
        json!({
            "target_type": "object",
            "target_required": ["company", "reference"],
            "company_type": "string",
            "generation_type": ["integer", "null"],
            "settings_type": "object",
            "settings_required": ["purpose", "base_currency", "fiscal_year_start"],
            "csv_type": ["object", "null"],
            "debits_type": "array",
            "debits_min_items": 1,
            "line_type": "object",
            "line_required": ["account", "amount"],
            "amount_type": "string",
            "meta_type": "object",
            "policy_type": "object",
        })
    );
    Ok(())
}

fn assert_inline_schema(tool: &str, schema: &Value) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    for keyword in ["$ref", "$defs"] {
        assert!(!schema.contains_key(keyword), "{tool}: {schema:?}");
    }
    let has_type = |keyword: &str| {
        schema.get("type").is_some_and(|value| {
            value == keyword
                || value
                    .as_array()
                    .is_some_and(|types| types.iter().any(|kind| kind == keyword))
        })
    };
    if schema.contains_key("properties") || schema.contains_key("additionalProperties") {
        assert!(has_type("object"), "{tool}: {schema:?}");
    }
    if schema.contains_key("items") || schema.contains_key("prefixItems") {
        assert!(has_type("array"), "{tool}: {schema:?}");
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for property in properties.values() {
            assert_inline_schema(tool, property);
        }
    }
    for keyword in ["items", "additionalProperties", "not", "if", "then", "else"] {
        if let Some(child) = schema.get(keyword) {
            assert_inline_schema(tool, child);
        }
    }
    for keyword in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        if let Some(children) = schema.get(keyword).and_then(Value::as_array) {
            for child in children {
                assert_inline_schema(tool, child);
            }
        }
    }
}

#[test]
fn stringified_setup_and_posting_match_native_arguments() -> TestResult {
    assert_eq!(onboard_and_post(true)?, onboard_and_post(false)?);
    Ok(())
}

fn onboard_and_post(stringify: bool) -> Result<[Value; 3], Box<dyn Error>> {
    let dir = tempdir()?;
    let path = dir.path().join("onboarding.db");
    let passfile = dir.path().join("passphrase");
    identity_ledger(&path, &passfile)?;
    let passphrase = secrecy::SecretString::from("mcp-test-passphrase");
    let db = Db::open_existing(&path, Some(&passphrase))?;
    let bookkeeper = proposal_identity(
        &db,
        "bookkeeper",
        &["ledger/read", "ledger/chart", "ledger/post"],
    )?;
    let token = bookkeeper["token"]
        .as_str()
        .ok_or("missing bookkeeper token")?;
    let mut client = identity_client(&path, &passfile, Some(token), false)?;
    let wire = |value: Value| {
        if stringify {
            Value::String(value.to_string())
        } else {
            value
        }
    };

    let profile = client.success("ledger_setup_book_initialize", &json!({
        "target":wire(json!({"company":"acme", "reference":"initialize"})),
        "settings":wire(json!({"purpose":"personal", "base_currency":"USD", "fiscal_year_start":"01-01"}))
    }))?;
    assert_eq!(profile["profile"]["company"], "acme");
    let account = client.success(
        "ledger_setup_account_add",
        &json!({
            "target":wire(json!({"company":"acme", "reference":"savings"})),
            "code":"1100", "name":" Savings ", "account_type":"asset"
        }),
    )?;
    assert_eq!(account["name"], "Savings");
    let mut post = posting("structured-sale");
    let description = r#"{"literal":"description"}"#;
    post["description"] = json!(description);
    post["debits"][0]["account"] = json!("1100");
    for field in ["debits", "credits", "meta"] {
        post[field] = wire(post[field].take());
    }
    let mut posted = client.success("txn_post", &post)?;
    assert_eq!(posted["created"], true);
    let id = posted
        .as_object_mut()
        .and_then(|post| post.remove("id"))
        .ok_or("missing transaction ID")?;
    let stored = client.success("txn_show", &json!({"company":"acme", "id":id}))?;
    let transaction = &stored[0];
    assert_eq!(transaction["description"], description);
    assert_eq!(transaction["entries"][0]["account_code"], "1100");
    assert_eq!(transaction["entries"][0]["amount"], 1250);
    assert_eq!(transaction["entries"][0]["memo"], "bank receipt");
    let metadata = transaction["metadata"].as_str().ok_or("missing metadata")?;
    assert_eq!(
        serde_json::from_str::<Value>(metadata)?,
        json!({"invoice":"one"})
    );
    Ok([profile, account, posted])
}

#[test]
fn stringified_posting_keeps_input_and_ledger_validation() -> TestResult {
    let dir = tempdir()?;
    let path = dir.path().join("validation.db");
    super::seed(&path)?;
    let mut command = bk(&path)?;
    command.args(["mcp", "serve"]);
    let mut client = Client::spawn(command)?;
    client.initialize()?;
    let mut post = posting("invalid");
    for (debits, detail) in [
        ("[", "EOF"),
        ("[]", "a non-empty array"),
        ("null", "expected a JSON object or array"),
        (r#"[{"account":"1000","amount":"oops"}]"#, "invalid amount"),
        (
            r#"[{"account":"1000","amount":"1","extra":true}]"#,
            "unknown field",
        ),
    ] {
        post["debits"] = json!(debits);
        assert_parameter_rejection(&client.call("txn_post", &post)?, detail);
    }
    post["debits"] = json!(r#"[{"account":"1000","amount":"1"}]"#);
    let response = client.call("txn_post", &post)?;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(payload(&response)?["code"], "VALIDATION");
    let transactions = client.success("txn_list", &json!({"company":"acme"}))?;
    assert_eq!(transactions, json!([]));
    Ok(())
}
