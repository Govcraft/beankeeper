//! Posted annotations are editable without changing the financial history.
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Books {
    _dir: TempDir,
    path: PathBuf,
    id: String,
}

impl Books {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let mut books = Self {
            path: dir.path().join("books.db"),
            _dir: dir,
            id: String::new(),
        };
        books.json(&["init"])?;
        books.json(&["company", "create", "acme", "Acme"])?;
        books.json(&["company", "create", "other", "Other"])?;
        books.json(&[
            "account",
            "create",
            "1100",
            "Receivables",
            "--type",
            "asset",
        ])?;
        books.json(&["account", "create", "4000", "Revenue", "--type", "revenue"])?;
        books.id = books.json(&["txn", "post", "-d", "Expected commission", "--date", "2026-01-01", "--reference", "CHARGE", "--debit", "1100:100", "--credit", "4000:100", "-m", r#"{"customer":"Carrier","nested":{"keep":true},"n":123456789012345678901234567890,"null":null,"applies_to":"old-link"}"#])?["data"]["id"].to_string();
        Ok(books)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(Command::cargo_bin("bk")?
            .arg("--db")
            .arg(&self.path)
            .args([
                "--company",
                "acme",
                "--actor",
                "annotation-test",
                "--no-color",
            ])
            .env_remove("BK_AGENT_TOKEN")
            .args(args)
            .output()?)
    }

    fn json(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut flags = vec!["--json"];
        flags.extend_from_slice(args);
        let output = self.run(&flags)?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn failure(&self, args: &[&str], code: &str, command: &str) -> TestResult {
        let mut flags = vec!["--json"];
        flags.extend_from_slice(args);
        let output = self.run(&flags)?;
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["error"]["code"], code, "{error}");
        assert_eq!(error["meta"]["command"], command);
        Ok(())
    }

    fn show(&self) -> Result<Value, Box<dyn Error>> {
        Ok(self.json(&["txn", "show", &self.id])?["data"][0].clone())
    }

    fn audit(&self, action: &str) -> Result<Value, Box<dyn Error>> {
        Ok(self.json(&["audit", "log", "--action", action])?["data"].clone())
    }
}

fn metadata(transaction: &Value) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(
        transaction["metadata"].as_str().ok_or("missing metadata")?,
    )?)
}

#[test]
fn set_and_unset_preserve_financial_fields_and_record_whole_objects() -> TestResult {
    let books = Books::new()?;
    let before = books.show()?;
    let set = books.json(&[
        "txn",
        "meta",
        "set",
        &books.id,
        "status=disputed",
        "dispute_id=D=42",
        "empty=",
        "numeric=123",
        "bool=true",
    ])?;
    assert_eq!(set["meta"]["command"], "txn.meta.set");
    assert_eq!(set["data"]["changed"], true);
    let mut after = books.show()?;
    let raw = after["metadata"].as_str().ok_or("missing metadata")?;
    assert!(raw.contains("123456789012345678901234567890"), "{raw}");
    let tags = metadata(&after)?;
    assert_eq!(tags["status"], "disputed");
    assert_eq!(tags["dispute_id"], "D=42");
    assert_eq!(tags["empty"], "");
    assert_eq!(tags["numeric"], "123");
    assert_eq!(tags["bool"], "true");
    assert_eq!(tags["nested"], json!({"keep":true}));
    assert_eq!(tags["applies_to"], "old-link");
    let rows = books.audit("metadata_set")?;
    assert_eq!(rows.as_array().map(Vec::len), Some(1));
    assert_eq!(rows[0]["entity"], "transaction");
    assert_eq!(rows[0]["entity_id"], books.id);
    assert_eq!(rows[0]["actor"], "annotation-test");
    assert_eq!(
        serde_json::from_str::<Value>(rows[0]["before"].as_str().ok_or("before")?)?,
        metadata(&before)?
    );
    assert_eq!(
        serde_json::from_str::<Value>(rows[0]["after"].as_str().ok_or("after")?)?,
        tags
    );
    after["metadata"] = before["metadata"].clone();
    assert_eq!(after, before);

    let replace = books.json(&[
        "txn",
        "meta",
        "set",
        "--reference",
        " CHARGE ",
        "status=resolved",
    ])?;
    assert_eq!(replace["data"]["id"].to_string(), books.id);
    assert_eq!(metadata(&books.show()?)?["status"], "resolved");
    let unset = books.json(&[
        "txn",
        "meta",
        "unset",
        "--reference",
        "CHARGE",
        "status",
        "dispute_id",
        "null",
    ])?;
    assert_eq!(unset["meta"]["command"], "txn.meta.unset");
    assert_eq!(unset["data"]["changed"], true);
    let tags = metadata(&books.show()?)?;
    assert!(tags.get("status").is_none());
    assert!(tags.get("dispute_id").is_none());
    assert!(tags.get("null").is_none());
    assert_eq!(tags["customer"], "Carrier");
    let unset_rows = books.audit("metadata_unset")?;
    assert_eq!(unset_rows.as_array().map(Vec::len), Some(1));
    assert_eq!(
        serde_json::from_str::<Value>(unset_rows[0]["after"].as_str().ok_or("after")?)?,
        tags
    );
    books.json(&["verify"])?;
    Ok(())
}

#[test]
fn no_ops_and_literal_keys_do_not_create_extra_audit_rows() -> TestResult {
    let books = Books::new()?;
    let before = books.show()?;
    for args in [
        vec!["txn", "meta", "set", &books.id, "customer=Carrier"],
        vec!["txn", "meta", "unset", &books.id, "absent"],
    ] {
        assert_eq!(books.json(&args)?["data"]["changed"], false);
    }
    assert_eq!(books.show()?, before);
    assert_eq!(books.audit("metadata_set")?, json!([]));
    assert_eq!(books.audit("metadata_unset")?, json!([]));
    let keys = ["a.b", "arr[0]", "x\\y", "日本語", "$"];
    for key in keys {
        let pair = format!("{key}=literal");
        books.json(&["txn", "meta", "set", &books.id, &pair])?;
        assert_eq!(metadata(&books.show()?)?[key], "literal", "{key}");
        assert_eq!(
            books.json(&["txn", "meta", "set", &books.id, &pair])?["data"]["changed"],
            false
        );
        books.json(&["txn", "meta", "unset", &books.id, key])?;
        assert!(metadata(&books.show()?)?.get(key).is_none());
    }
    let table = books.run(&["txn", "meta", "set", &books.id, "status=disputed"])?;
    assert!(table.status.success());
    assert!(String::from_utf8(table.stdout)?.contains("disputed"));
    let table = books.run(&["txn", "meta", "unset", &books.id, "status"])?;
    assert!(table.status.success());
    assert!(!String::from_utf8(table.stdout)?.contains("disputed"));
    Ok(())
}

#[test]
fn reserved_invalid_duplicate_and_cross_company_edits_are_rejected_atomically() -> TestResult {
    let books = Books::new()?;
    let before = books.show()?;
    for key in ["correlate", "reverses", "reversed_by", "applies_to"] {
        let pair = format!("{key}=oops");
        books.failure(
            &["txn", "meta", "set", &books.id, "status=disputed", &pair],
            "VALIDATION",
            "txn.meta.set",
        )?;
        books.failure(
            &["txn", "meta", "unset", &books.id, "customer", key],
            "VALIDATION",
            "txn.meta.unset",
        )?;
    }
    for args in [
        vec!["a=1", "a=2"],
        vec!["=x"],
        vec!["broken"],
        vec!["a\"b=x"],
    ] {
        let mut flags = vec!["txn", "meta", "set", &books.id];
        flags.extend(args);
        let output = books.run(&flags)?;
        assert!(!output.status.success());
    }
    books.failure(
        &["txn", "meta", "unset", &books.id, "customer", "customer"],
        "VALIDATION",
        "txn.meta.unset",
    )?;
    for selector in [
        vec!["999999"],
        vec!["--reference", "missing"],
        vec!["--company", "other", &books.id],
        vec!["--company", "other", "--reference", "CHARGE"],
    ] {
        for (operation, value) in [("set", "x=y"), ("unset", "customer")] {
            let mut args = vec!["txn", "meta", operation];
            args.extend_from_slice(&selector);
            args.push(value);
            books.failure(&args, "NOT_FOUND", &format!("txn.meta.{operation}"))?;
        }
    }
    for args in [
        vec![
            "txn",
            "meta",
            "set",
            &books.id,
            "--reference",
            "CHARGE",
            "x=y",
        ],
        vec!["txn", "meta", "set", &books.id],
        vec!["txn", "meta", "unset", &books.id],
    ] {
        assert!(!books.run(&args)?.status.success());
    }
    assert_eq!(books.show()?, before);
    assert_eq!(books.audit("metadata_set")?, json!([]));
    assert_eq!(books.audit("metadata_unset")?, json!([]));
    Ok(())
}

#[test]
fn tagging_a_charge_preserves_its_original_age_and_specific_payment_matching() -> TestResult {
    let books = Books::new()?;
    books.json(&[
        "txn",
        "post",
        "-d",
        "Payment",
        "--date",
        "2026-02-01",
        "--debit",
        "4000:40",
        "--credit",
        "1100:40",
        "--meta",
        "applies_to=CHARGE",
    ])?;
    let report_args = [
        "report",
        "aging",
        "--account",
        "1100",
        "--as-of",
        "2026-03-15",
        "--method",
        "specific",
        "--detail",
        "--filter-metadata",
        "status=disputed",
    ];
    let initial = books.json(&report_args)?;
    books.json(&[
        "txn",
        "meta",
        "set",
        "--reference",
        "CHARGE",
        "status=disputed",
    ])?;
    let aged = books.json(&report_args)?;
    assert_ne!(aged["data"], initial["data"]);
    assert_eq!(initial["data"]["totals"]["total_open"], 0);
    assert_eq!(aged["data"]["totals"]["total_open"], 6000);
    assert_eq!(aged["data"]["unmatched_links"], json!([]));
    let item = &aged["data"]["accounts"][0]["groups"][0]["open_items"][0];
    assert_eq!(item["transaction_id"].to_string(), books.id);
    assert_eq!(item["date"], "2026-01-01");
    assert_eq!(item["age_days"], 73);
    assert_eq!(item["original_amount"], 10000);
    assert_eq!(item["open_amount"], 6000);
    let list = books.json(&["txn", "list", "--filter-metadata", "status=disputed"])?;
    assert_eq!(list["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(list["data"][0]["id"].to_string(), books.id);
    assert_eq!(books.show()?["date"], "2026-01-01");
    Ok(())
}

#[test]
fn audit_append_failure_rolls_back_the_entire_metadata_batch() -> TestResult {
    let books = Books::new()?;
    let before = books.show()?;
    Connection::open(&books.path)?.execute_batch("CREATE TRIGGER fail_metadata_audit BEFORE INSERT ON audit_log WHEN NEW.action = 'metadata_set' BEGIN SELECT RAISE(ABORT, 'simulated audit failure'); END;")?;
    assert!(
        !books
            .run(&[
                "txn",
                "meta",
                "set",
                &books.id,
                "customer=Changed",
                "status=disputed"
            ])?
            .status
            .success()
    );
    assert_eq!(books.show()?, before);
    assert_eq!(books.audit("metadata_set")?, json!([]));
    books.json(&["verify"])?;
    Ok(())
}

#[test]
fn unsupported_upsert_is_rejected_by_post_and_all_import_parsers() -> TestResult {
    let books = Books::new()?;
    for args in [
        vec![
            "txn",
            "post",
            "-d",
            "No",
            "--debit",
            "1100:1",
            "--credit",
            "4000:1",
            "--on-conflict",
            "upsert",
        ],
        vec![
            "txn",
            "import",
            "--file",
            "unused.csv",
            "--on-conflict",
            "upsert",
        ],
        vec![
            "txn",
            "import",
            "--file",
            "unused.ofx",
            "--on-conflict",
            "upsert",
        ],
    ] {
        let output = books.run(&args)?;
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("upsert"), "{stderr}");
    }
    Ok(())
}

#[test]
fn initially_absent_metadata_empty_objects_and_environment_actor_are_supported() -> TestResult {
    let books = Books::new()?;
    let id = books.json(&[
        "txn", "post", "-d", "No tags", "--debit", "1100:10", "--credit", "4000:10",
    ])?["data"]["id"]
        .to_string();
    let noop = books.json(&["txn", "meta", "unset", &id, "missing"])?;
    assert_eq!(noop["data"]["metadata"], Value::Null);
    assert_eq!(noop["data"]["changed"], false);
    let output = Command::cargo_bin("bk")?
        .arg("--db")
        .arg(&books.path)
        .args([
            "--json",
            "--company",
            "acme",
            "txn",
            "meta",
            "set",
            &id,
            "status=disputed",
        ])
        .env_remove("BK_AGENT_TOKEN")
        .env("BK_ACTOR", "workflow")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = books.audit("metadata_set")?;
    assert_eq!(rows[0]["actor"], "workflow");
    assert_eq!(rows[0]["before"], Value::Null);
    let unset = books.json(&["txn", "meta", "unset", &id, "status"])?;
    assert_eq!(unset["data"]["metadata"], "{}");
    assert_eq!(books.audit("metadata_unset")?[0]["after"], "{}");
    assert_eq!(
        books.json(&["txn", "meta", "unset", &id, "status"])?["data"]["changed"],
        false
    );
    books.json(&["verify"])?;
    Ok(())
}

#[test]
fn post_rejects_forged_ledger_links_in_raw_metadata() -> TestResult {
    let books = Books::new()?;
    for key in ["correlate", "reverses", "reversed_by", "proposal_id"] {
        let metadata = format!(r#"{{"{key}":999}}"#);
        books.failure(
            &[
                "txn", "post", "-d", "Forgery", "--debit", "1100:5", "--credit", "4000:5", "-m",
                &metadata,
            ],
            "VALIDATION",
            "txn.post",
        )?;
    }
    Ok(())
}
