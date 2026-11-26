//! Regression tests for issue #17: transaction metadata is always stored as a
//! JSON object, `txn reconcile` tolerates any stored metadata, and the
//! structured `--meta key=value` flags on `txn post` / `txn list` work.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

/// Runs `bk --db <db> <args>` and returns (success, stdout, stderr).
fn run_bk(db: &str, args: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
    cmd.arg("--db").arg(db).args(args);
    let out = cmd.output().unwrap_or_else(|e| panic!("run bk: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs `bk` and asserts success, returning stdout.
fn ok(db: &str, args: &[&str]) -> String {
    let (success, stdout, stderr) = run_bk(db, args);
    assert!(success, "bk {args:?} failed: {stderr}");
    stdout
}

fn parse(stdout: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
}

/// Fresh DB with companies `acme` and `beta`, each with Cash (1000) and
/// Equity (3000).
fn setup() -> (TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .unwrap_or_else(|| panic!("utf-8 path"))
        .to_string();
    ok(&db, &["init"]);
    for slug in ["acme", "beta"] {
        ok(&db, &["company", "create", slug, slug]);
        ok(
            &db,
            &[
                "--company",
                slug,
                "account",
                "create",
                "1000",
                "Cash",
                "--type",
                "asset",
            ],
        );
        ok(
            &db,
            &[
                "--company",
                slug,
                "account",
                "create",
                "3000",
                "Equity",
                "--type",
                "equity",
            ],
        );
    }
    (dir, db)
}

/// Posts a balanced 1000/3000 transaction with extra args; returns its ID.
fn post(db: &str, company: &str, desc: &str, extra: &[&str]) -> i64 {
    let mut args = vec![
        "--json",
        "--company",
        company,
        "txn",
        "post",
        "-d",
        desc,
        "--date",
        "2026-01-01",
        "--debit",
        "1000:100",
        "--credit",
        "3000:100",
    ];
    args.extend_from_slice(extra);
    let v = parse(&ok(db, &args));
    v["data"]["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("no id in {v}"))
}

/// Returns the stored metadata of a transaction, parsed as JSON (`Null` if none).
fn stored_metadata(db: &str, company: &str, id: i64) -> Value {
    let v = parse(&ok(
        db,
        &[
            "--json",
            "--company",
            company,
            "txn",
            "list",
            "--limit",
            "1000",
        ],
    ));
    let txns = v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("data is not an array: {v}"));
    let txn = txns
        .iter()
        .find(|t| t["id"].as_i64() == Some(id))
        .unwrap_or_else(|| panic!("txn {id} not listed"));
    match txn["metadata"].as_str() {
        None => Value::Null,
        Some(s) => serde_json::from_str(s).unwrap_or_else(|e| panic!("not JSON ({e}): {s}")),
    }
}

fn list_ids(db: &str, company: &str, filters: &[&str]) -> Vec<i64> {
    let mut args = vec!["--json", "--company", company, "txn", "list"];
    args.extend_from_slice(filters);
    let v = parse(&ok(db, &args));
    v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("data is not an array: {v}"))
        .iter()
        .filter_map(|t| t["id"].as_i64())
        .collect()
}

#[test]
fn reconcile_succeeds_with_plain_text_metadata() {
    let (_dir, db) = setup();

    // The exact repro from the issue: free-text metadata.
    post(&db, "acme", "Opening", &["-m", "open:1000"]);
    // A memo that mentions the word "correlate" must not block linking.
    let partner = post(&db, "beta", "Loan out", &["-m", "will \"correlate\" later"]);
    let linked = post(
        &db,
        "acme",
        "Loan in",
        &["-m", "intercompany", "--correlate", &partner.to_string()],
    );

    let v = parse(&ok(&db, &["--json", "txn", "reconcile"]));
    assert_eq!(v["ok"], Value::Bool(true));
    assert_eq!(v["data"], Value::Array(vec![]), "pair is fully linked: {v}");

    // Both sides carry the link, and the memos survive.
    let partner_meta = stored_metadata(&db, "beta", partner);
    assert_eq!(partner_meta["correlate"], Value::from(linked));
    assert_eq!(
        partner_meta["memo"],
        Value::from("will \"correlate\" later")
    );
    let linked_meta = stored_metadata(&db, "acme", linked);
    assert_eq!(linked_meta["correlate"], Value::from(partner));
    assert_eq!(linked_meta["memo"], Value::from("intercompany"));
}

#[test]
fn reconcile_still_reports_orphans_alongside_plain_text() -> Result<(), Box<dyn std::error::Error>>
{
    let (_dir, db) = setup();
    post(&db, "acme", "Opening", &["-m", "open:1000"]);
    let orphan = post(&db, "acme", "Dangling", &[]);
    // Earlier writers accepted this metadata. Seed the historical row directly.
    let conn = rusqlite::Connection::open(&db)?;
    conn.execute(
        "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
        rusqlite::params![r#"{"correlate":999}"#, orphan],
    )?;
    drop(conn);

    let (success, stdout, _) = run_bk(&db, &["--json", "txn", "reconcile"]);
    assert!(!success, "orphans make reconcile exit non-zero");
    let v = parse(&stdout);
    let data = v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("data is not an array: {v}"));
    assert_eq!(data.len(), 1, "{v}");
    assert_eq!(data[0]["transaction_id"], Value::from(orphan));
    assert_eq!(data[0]["partner_id"], Value::from(999));
    Ok(())
}

#[test]
fn post_normalizes_metadata_to_json_object() {
    let (_dir, db) = setup();
    let cases: [(&str, Value); 5] = [
        ("open:1000", serde_json::json!({"memo": "open:1000"})),
        (r#"{"ref":"INV-1"}"#, serde_json::json!({"ref": "INV-1"})),
        ("[1,2]", serde_json::json!({"memo": [1, 2]})),
        ("00123", serde_json::json!({"memo": "00123"})),
        (r#""quoted""#, serde_json::json!({"memo": "quoted"})),
    ];
    for (input, expected) in cases {
        let id = post(&db, "acme", "t", &["-m", input]);
        assert_eq!(stored_metadata(&db, "acme", id), expected, "input {input}");
    }
    let none = post(&db, "acme", "t", &[]);
    assert_eq!(stored_metadata(&db, "acme", none), Value::Null);
}

#[test]
fn meta_flags_on_post_and_list() {
    let (_dir, db) = setup();
    let a = post(
        &db,
        "acme",
        "AWS",
        &[
            "-m",
            "Q1 cloud",
            "--meta",
            "vendor=AWS",
            "--meta",
            "project=apollo",
        ],
    );
    let b = post(
        &db,
        "acme",
        "GCP",
        &["-m", r#"{"qty":3,"paid":true}"#, "--meta", "vendor=GCP"],
    );
    post(&db, "acme", "Plain", &["-m", "vendor=AWS but as text"]);

    assert_eq!(
        stored_metadata(&db, "acme", a),
        serde_json::json!({"memo": "Q1 cloud", "vendor": "AWS", "project": "apollo"})
    );
    assert_eq!(
        stored_metadata(&db, "acme", b),
        serde_json::json!({"qty": 3, "paid": true, "vendor": "GCP"})
    );

    // Exact, case-sensitive string match; free text never matches.
    assert_eq!(list_ids(&db, "acme", &["--meta", "vendor=AWS"]), vec![a]);
    assert_eq!(
        list_ids(&db, "acme", &["--meta", "vendor=aws"]),
        Vec::<i64>::new()
    );
    // Repeated filters are AND-ed.
    assert_eq!(
        list_ids(
            &db,
            "acme",
            &["--meta", "vendor=AWS", "--meta", "project=apollo"]
        ),
        vec![a]
    );
    assert_eq!(
        list_ids(
            &db,
            "acme",
            &["--meta", "vendor=AWS", "--meta", "project=x"]
        ),
        Vec::<i64>::new()
    );
    // Numbers and booleans compare by their JSON text.
    assert_eq!(list_ids(&db, "acme", &["--meta", "qty=3"]), vec![b]);
    assert_eq!(list_ids(&db, "acme", &["--meta", "paid=true"]), vec![b]);
    // Free text is reachable under `memo`.
    assert_eq!(list_ids(&db, "acme", &["--meta", "memo=Q1 cloud"]), vec![a]);
    // Substring search still works alongside.
    assert_eq!(list_ids(&db, "acme", &["--metadata", "apollo"]), vec![a]);

    // Duplicate keys are a validation error (exit code 3).
    for extra in [
        &["--meta", "k=1", "--meta", "k=2"][..],
        &["-m", r#"{"k":1}"#, "--meta", "k=2"][..],
        &["-m", "text", "--meta", "memo=x"][..],
        &["--meta", "correlate=5"][..],
    ] {
        let mut args = vec![
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "dup",
            "--debit",
            "1000:1",
            "--credit",
            "3000:1",
        ];
        args.extend_from_slice(extra);
        let mut cmd = Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary: {e}"));
        let status = cmd
            .arg("--db")
            .arg(&db)
            .args(&args)
            .status()
            .unwrap_or_else(|e| panic!("run bk: {e}"));
        assert_eq!(status.code(), Some(3), "args {extra:?} should be rejected");
    }

    // Malformed --meta is a usage error.
    let (success, _, _) = run_bk(
        &db,
        &["--company", "acme", "txn", "list", "--meta", "novalue"],
    );
    assert!(!success);
}

#[test]
fn opening_a_v8_database_migrates_free_text_metadata() {
    let (_dir, db) = setup();
    post(&db, "acme", "seed", &[]);

    // Rewind to a v8-shaped database holding legacy free-text metadata.
    {
        let conn = rusqlite::Connection::open(&db).unwrap_or_else(|e| panic!("open: {e}"));
        conn.execute_batch(
            "DROP TRIGGER protect_cash_class_update;
             ALTER TABLE accounts DROP COLUMN cash_class;
             DROP TABLE fx_postings;
             DROP TABLE fx_accounts;
             DROP TRIGGER protect_statement_clearing_update;
             DROP TRIGGER protect_statement_clearing_delete;
             DROP TRIGGER protect_statement_clearing_replace;
             DROP TRIGGER protect_statement_undo;
             DROP VIEW statement_locked_clearings;
             DROP TABLE statement_reconciliation_clearings;
             DROP TABLE statement_reconciliations;
             DROP TRIGGER protect_transaction_link_undo;
             DROP TABLE transaction_links;
             DROP TRIGGER protect_bookkeeping_receipt_uniqueness;
             DROP TRIGGER protect_audit_related_posting;
             DROP VIEW active_transaction_links;
             DROP VIEW transaction_link_creations;
             DROP INDEX idx_transaction_company_id;
             DROP VIEW hold_adjustments;
             DROP VIEW posting_reversals;
             DROP VIEW posted_transactions;
             DROP TRIGGER protect_hold_settlement;
             DROP TRIGGER require_hold_actor;
             DROP TRIGGER protect_hold_fields;
             DROP INDEX idx_hold_settlements;
             ALTER TABLE transactions DROP COLUMN amount_delta;
             ALTER TABLE transactions DROP COLUMN settles_hold;
             ALTER TABLE transactions DROP COLUMN posting_status;
             DROP VIEW audit_receipt_postings;
             DROP TRIGGER protect_archived_account_delete;
             DROP TRIGGER protect_archived_account_insert;
             DROP TRIGGER protect_account_archive;
             ALTER TABLE accounts DROP COLUMN archived;
             DROP TABLE standing_settings;
             DROP TABLE setup_statement_batches;
             DROP TABLE setup_requests;
             DROP TABLE setup_book_profiles;
             DROP TABLE setup_sessions;
             DROP TABLE period_locks;
             DROP TRIGGER protect_period_post;
             DROP TABLE txn_proposals;
             DROP TABLE agent_spend_limits;
             DROP TABLE ledger_policy;
             DROP TABLE schedule_entries;
             DROP TABLE schedules;
             DROP TRIGGER protect_rule_deactivation;
             DROP TABLE categorization_rules;
             DROP VIEW bookkeeping_audit;
             DROP TRIGGER protect_transaction_identity;
             DROP TABLE agent_revocations;
             DROP TABLE agent_tokens;
             DROP TABLE identity_root;
             ALTER TABLE transactions DROP COLUMN agent_uri;
             ALTER TABLE transactions DROP COLUMN token_jti;
             DROP TRIGGER protect_transactions_insert;
             DROP TRIGGER protect_transactions_update;
             DROP TRIGGER protect_transactions_delete;
             DROP TRIGGER protect_entries_insert;
             DROP TRIGGER protect_entries_update;
             DROP TRIGGER protect_entries_delete;
             DROP TRIGGER protect_audit_insert;
             DROP TRIGGER protect_audit_update;
             DROP TRIGGER protect_audit_delete;
             CREATE TABLE audit_log_v8 AS
                 SELECT id, changed_at, actor, company_slug, entity, entity_id, action, before, after
                 FROM audit_log WHERE action <> 'create';
             DROP TABLE audit_log;
             ALTER TABLE audit_log_v8 RENAME TO audit_log;
             ALTER TABLE transactions DROP COLUMN posted_by;
             DROP TABLE lot_reliefs;
             DROP TABLE investment_lots;
             DROP TABLE security_prices;
             DROP TABLE securities;
             DELETE FROM schema_version WHERE version >= 9;",
        )
        .unwrap_or_else(|e| panic!("rewind: {e}"));
        seed_legacy_metadata(&conn).unwrap_or_else(|e| panic!("seed legacy metadata: {e}"));
    }

    // Reconcile verifies a backup before upgrading the legacy schema.
    let v = parse(&ok(&db, &["--json", "txn", "reconcile"]));
    assert_eq!(v["data"], Value::Array(vec![]));

    let conn = rusqlite::Connection::open(&db).unwrap_or_else(|e| panic!("open: {e}"));
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("version: {e}"));
    assert!(version >= 9);
    let mut stmt = conn
        .prepare("SELECT metadata FROM transactions WHERE description LIKE 'legacy%' ORDER BY id")
        .unwrap_or_else(|e| panic!("prepare: {e}"));
    let metas: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap_or_else(|e| panic!("query: {e}"))
        .filter_map(Result::ok)
        .collect();
    assert_eq!(metas, vec![r#"{"memo":"open:1000"}"#, r#"{"memo":"42"}"#]);
}

fn seed_legacy_metadata(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "INSERT INTO transactions (company_slug, description, metadata, date) VALUES
             ('acme', 'legacy', 'open:1000', '2026-01-01'),
             ('acme', 'legacy-num', '42', '2026-01-01');
         INSERT INTO entries (transaction_id, company_slug, account_code, direction, amount)
             SELECT id, company_slug, '1000', 'debit', 100
             FROM transactions WHERE description IN ('legacy', 'legacy-num');
         INSERT INTO entries (transaction_id, company_slug, account_code, direction, amount)
             SELECT id, company_slug, '3000', 'credit', 100
             FROM transactions WHERE description IN ('legacy', 'legacy-num');",
    )
}
