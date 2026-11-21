//! Pending transaction review, attribution, references and proposal visibility.

use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db, transactions};
use beankeeper_cli::identity::{self, IssueRequest};
use secrecy::SecretString;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    _dir: TempDir,
    path: PathBuf,
    passphrase_file: PathBuf,
    db: Db,
}

struct Agent {
    token: String,
    uri: String,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("proposals.db");
        let passphrase_file = dir.path().join("passphrase");
        std::fs::write(&passphrase_file, "proposal-test-passphrase")?;
        let passphrase: SecretString = "proposal-test-passphrase".into();
        let db = Db::open(&path, Some(&passphrase))?;
        for company in ["acme", "other"] {
            db::create_company(db.conn(), company, company, None)?;
            for (code, kind) in [("1000", "asset"), ("5000", "expense")] {
                db::create_account(db.conn(), company, code, code, kind, Some("default-tax"))?;
            }
        }
        identity::init_root(db.conn(), &Actor::new("owner"))?;
        Ok(Self {
            _dir: dir,
            path,
            passphrase_file,
            db,
        })
    }

    fn call(
        &self,
        company: &str,
        args: &[&str],
        token: Option<&str>,
    ) -> Result<Output, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(&self.path)
            .arg("--passphrase-file")
            .arg(&self.passphrase_file)
            .args(["--json", "--company", company, "--actor", "reviewer"]);
        for variable in [
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
        ] {
            command.env_remove(variable);
        }
        if let Some(token) = token {
            command.args(["--agent-token", token]);
        }
        Ok(command.args(args).output()?)
    }

    fn run(
        &self,
        company: &str,
        args: &[&str],
        token: Option<&str>,
    ) -> Result<Value, Box<dyn Error>> {
        let output = self.call(company, args, token)?;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(envelope["ok"], true);
        Ok(envelope["data"].clone())
    }

    fn issue(&self, grants: &[&str]) -> Result<Agent, Box<dyn Error>> {
        let issued = identity::issue(
            self.db.conn(),
            &Actor::new("owner"),
            &IssueRequest {
                name: "review",
                capabilities: &grants.iter().map(ToString::to_string).collect::<Vec<_>>(),
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )?;
        Ok(Agent {
            token: issued["token"].as_str().ok_or("missing token")?.to_owned(),
            uri: issued["agent_uri"]
                .as_str()
                .ok_or("missing URI")?
                .to_owned(),
        })
    }

    fn propose(&self, agent: &Agent, reference: &str) -> Result<Value, Box<dyn Error>> {
        self.run(
            "acme",
            &[
                "txn",
                "propose",
                "-d",
                "Office supplies",
                "--debit",
                "5000:500.01:Receipt",
                "--credit",
                "1000:500.01",
                "--date",
                "2026-01-01",
                "--reference",
                reference,
                "--meta",
                "vendor=Acme",
                "--tax",
                "5000=office",
            ],
            Some(&agent.token),
        )
    }

    fn count(&self, table: &str) -> Result<i64, Box<dyn Error>> {
        Ok(self
            .db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }
}

fn id(proposal: &Value) -> Result<&str, Box<dyn Error>> {
    proposal["id"]
        .as_str()
        .ok_or_else(|| "missing proposal ID".into())
}

fn error_code(output: &Output, expected: &str, exit: i32) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, [] as [u8; 0]);
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], expected);
    Ok(())
}

fn assert_approved_details(
    ledger: &Ledger,
    proposal: &Value,
    resolved: &Value,
    approver: &Agent,
) -> TestResult {
    let transaction_id = resolved["transaction_id"]
        .as_i64()
        .ok_or("missing transaction ID")?;
    let (txn, entries) = transactions::get_transaction(ledger.db.conn(), "acme", transaction_id)?;
    assert_eq!(txn.agent_uri.as_deref(), Some(approver.uri.as_str()));
    assert_eq!(
        txn.token_jti.as_deref(),
        Some(identity::verify(ledger.db.conn(), &approver.token)?.jti())
    );
    assert_eq!(txn.posted_by.as_deref(), Some("reviewer"));
    assert_eq!(txn.description, "Office supplies");
    assert_eq!(txn.date, "2026-01-01");
    assert_eq!(txn.reference.as_deref(), Some("receipt-1"));
    let metadata: Value = serde_json::from_str(txn.metadata.as_deref().ok_or("missing metadata")?)?;
    assert_eq!(metadata["proposal_id"], id(proposal)?);
    assert_eq!(metadata["vendor"], "Acme");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.amount == 50001));
    let debit = entries
        .iter()
        .find(|entry| entry.direction.is_debit())
        .ok_or("missing debit")?;
    assert_eq!(debit.memo.as_deref(), Some("Receipt"));
    assert_eq!(debit.tax_category.as_deref(), Some("office"));
    Ok(())
}

fn set_review_limits(ledger: &Ledger, proposer: &Agent, approver: &Agent) -> TestResult {
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &proposer.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "500",
            "--daily-limit",
            "500",
        ],
        None,
    )?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &approver.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "0.01",
            "--daily-limit",
            "0.01",
        ],
        None,
    )?;
    Ok(())
}

#[test]
fn approval_preserves_posting_details_attributes_approver_and_audits_proposer() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose", "ledger/post", "ledger/read"])?;
    let approver = ledger.issue(&["ledger/approve", "ledger/post", "ledger/read"])?;
    set_review_limits(&ledger, &proposer, &approver)?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let audits = ledger.count("audit_log")?;
    let proposal = ledger.propose(&proposer, "receipt-1")?;
    assert_eq!(proposal["status"], "pending");
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("entries")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    ledger.run("acme", &["verify"], Some(&proposer.token))?;
    let resolved = ledger.run(
        "acme",
        &["txn", "approve", id(&proposal)?],
        Some(&approver.token),
    )?;
    assert_eq!(resolved["status"], "approved");
    assert_approved_details(&ledger, &proposal, &resolved, &approver)?;
    let approval_snapshot: String = ledger.db.conn().query_row(
        "SELECT after FROM audit_log WHERE action = 'proposal_approve'",
        [],
        |row| row.get(0),
    )?;
    assert!(approval_snapshot.contains(&proposer.uri));
    assert!(approval_snapshot.contains(id(&proposal)?));
    error_code(
        &ledger.call(
            "acme",
            &["txn", "approve", id(&proposal)?],
            Some(&approver.token),
        )?,
        "VALIDATION",
        3,
    )?;
    assert_eq!(ledger.count("transactions")?, 1);
    // Over-limit approval counts against the proposer, never the approver.
    error_code(
        &ledger.call(
            "acme",
            &[
                "txn",
                "post",
                "-d",
                "More",
                "--debit",
                "5000:0.01",
                "--credit",
                "1000:0.01",
                "--date",
                "2026-01-01",
            ],
            Some(&proposer.token),
        )?,
        "FORBIDDEN",
        7,
    )?;
    ledger.run(
        "acme",
        &[
            "txn",
            "post",
            "-d",
            "Approver bucket",
            "--debit",
            "5000:0.01",
            "--credit",
            "1000:0.01",
            "--date",
            "2026-01-01",
        ],
        Some(&approver.token),
    )?;
    ledger.run("acme", &["verify"], Some(&approver.token))?;
    Ok(())
}

#[test]
fn propose_and_approve_grants_are_siblings_and_proposal_visibility_is_scoped() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose", "ledger/read"])?;
    let other = ledger.issue(&["ledger/propose", "ledger/read"])?;
    let post = ledger.issue(&["ledger/post"])?;
    let admin = ledger.issue(&["ledger/admin", "ledger/read"])?;
    let approver = ledger.issue(&["ledger/approve"])?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let one = ledger.propose(&proposer, "one")?;
    let two = ledger.propose(&other, "two")?;
    assert_eq!(
        ledger
            .run("acme", &["txn", "proposals"], Some(&proposer.token))?
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        ledger
            .run("acme", &["txn", "proposals"], Some(&admin.token))?
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(
        ledger
            .run("acme", &["txn", "proposals"], Some(&approver.token))?
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    error_code(
        &ledger.call(
            "acme",
            &["txn", "approve", id(&one)?],
            Some(&proposer.token),
        )?,
        "FORBIDDEN",
        7,
    )?;
    error_code(
        &ledger.call("acme", &["txn", "approve", id(&one)?], Some(&admin.token))?,
        "FORBIDDEN",
        7,
    )?;
    error_code(
        &ledger.call(
            "acme",
            &[
                "txn", "propose", "-d", "No grant", "--debit", "5000:1", "--credit", "1000:1",
            ],
            Some(&post.token),
        )?,
        "FORBIDDEN",
        7,
    )?;
    error_code(
        &ledger.call("acme", &["txn", "reject", id(&two)?], Some(&proposer.token))?,
        "FORBIDDEN",
        7,
    )?;
    let withdrawn = ledger.run(
        "acme",
        &["txn", "withdraw", id(&one)?, "--reason", "Correct receipt"],
        Some(&proposer.token),
    )?;
    assert_eq!(withdrawn["status"], "withdrawn");
    assert_eq!(withdrawn["reason"], "Correct receipt");
    let rejected = ledger.run(
        "acme",
        &["txn", "reject", id(&two)?, "--reason", "Not approved"],
        Some(&approver.token),
    )?;
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(
        ledger.run("acme", &["txn", "proposals"], Some(&admin.token))?,
        serde_json::json!([])
    );
    assert_ne!(
        ledger
            .run(
                "acme",
                &["txn", "proposals", "--include-rejected"],
                Some(&admin.token)
            )?
            .as_array()
            .ok_or("missing proposals")?
            .as_slice(),
        [] as [Value; 0]
    );
    assert_eq!(ledger.count("transactions")?, 0);
    ledger.run("acme", &["verify"], Some(&admin.token))?;
    Ok(())
}

#[test]
fn pending_references_are_reserved_until_withdrawal() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    let first = ledger.propose(&proposer, "reserved")?;
    let duplicate = ledger.call(
        "acme",
        &[
            "txn",
            "propose",
            "-d",
            "Duplicate",
            "--debit",
            "5000:1",
            "--credit",
            "1000:1",
            "--reference",
            "reserved",
        ],
        Some(&proposer.token),
    )?;
    error_code(&duplicate, "VALIDATION", 3)?;
    error_code(
        &ledger.call(
            "acme",
            &[
                "txn",
                "post",
                "-d",
                "Steal reference",
                "--debit",
                "5000:1",
                "--credit",
                "1000:1",
                "--reference",
                "reserved",
            ],
            None,
        )?,
        "VALIDATION",
        3,
    )?;
    assert_eq!(ledger.count("txn_proposals")?, 1);
    ledger.run(
        "acme",
        &["txn", "reject", id(&first)?],
        Some(&proposer.token),
    )?;
    ledger.run(
        "acme",
        &[
            "txn",
            "post",
            "-d",
            "Posted",
            "--debit",
            "5000:1",
            "--credit",
            "1000:1",
            "--reference",
            "reserved",
        ],
        None,
    )?;
    assert_eq!(ledger.count("transactions")?, 1);
    ledger.run("acme", &["verify"], None)?;
    Ok(())
}

#[test]
fn existing_posted_references_conflict_or_skip_without_creating_proposals() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    ledger.run(
        "acme",
        &[
            "txn",
            "post",
            "-d",
            "Posted",
            "--debit",
            "5000:1",
            "--credit",
            "1000:1",
            "--reference",
            "reserved",
        ],
        None,
    )?;
    error_code(
        &ledger.call(
            "acme",
            &[
                "txn",
                "propose",
                "-d",
                "Already posted",
                "--debit",
                "5000:1",
                "--credit",
                "1000:1",
                "--reference",
                "reserved",
            ],
            Some(&proposer.token),
        )?,
        "VALIDATION",
        3,
    )?;
    ledger.run(
        "acme",
        &[
            "txn",
            "propose",
            "-d",
            "Skip posted",
            "--debit",
            "5000:1",
            "--credit",
            "1000:1",
            "--reference",
            "reserved",
            "--on-conflict",
            "skip",
        ],
        Some(&proposer.token),
    )?;
    assert_eq!(ledger.count("txn_proposals")?, 0);
    assert_eq!(ledger.count("transactions")?, 1);
    ledger.run("acme", &["verify"], None)?;
    Ok(())
}

#[test]
fn proposal_ids_and_relationship_metadata_cannot_be_forged_or_edited() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    for extra in [
        ["--meta", "proposal_id=fake"],
        ["--metadata", "{\"proposal_id\":\"fake\"}"],
    ] {
        for operation in ["post", "propose"] {
            error_code(
                &ledger.call(
                    "acme",
                    &[
                        "txn", operation, "-d", "Forged", "--debit", "5000:1", "--credit",
                        "1000:1", extra[0], extra[1],
                    ],
                    Some(&proposer.token),
                )?,
                "VALIDATION",
                3,
            )?;
        }
    }
    assert_eq!(ledger.count("txn_proposals")?, 0);
    let proposal = ledger.propose(&proposer, "metadata-reserved")?;
    let resolved = ledger.run("acme", &["txn", "approve", id(&proposal)?], None)?;
    let txn_id = resolved["transaction_id"]
        .as_i64()
        .ok_or("missing transaction ID")?
        .to_string();
    error_code(
        &ledger.call(
            "acme",
            &["txn", "meta", "set", &txn_id, "proposal_id=other"],
            None,
        )?,
        "VALIDATION",
        3,
    )?;
    error_code(
        &ledger.call(
            "acme",
            &["txn", "meta", "unset", &txn_id, "proposal_id"],
            None,
        )?,
        "VALIDATION",
        3,
    )?;
    ledger.run("acme", &["verify"], None)?;
    Ok(())
}

#[test]
fn invalid_proposals_and_failed_approvals_do_not_leave_partial_history() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    error_code(
        &ledger.call(
            "acme",
            &[
                "txn",
                "propose",
                "-d",
                "Anonymous",
                "--debit",
                "5000:1",
                "--credit",
                "1000:1",
            ],
            None,
        )?,
        "UNAUTHORIZED",
        6,
    )?;
    for (debit, credit) in [("5000:1", "1000:2"), ("9999:1", "1000:1")] {
        assert!(
            !ledger
                .call(
                    "acme",
                    &[
                        "txn", "propose", "-d", "Invalid", "--debit", debit, "--credit", credit
                    ],
                    Some(&proposer.token)
                )?
                .status
                .success()
        );
    }
    assert_eq!(ledger.count("txn_proposals")?, 0);
    let proposal = ledger.propose(&proposer, "atomic")?;
    let audits = ledger.count("audit_log")?;
    ledger.db.conn().execute_batch("CREATE TRIGGER reject_test_post BEFORE INSERT ON transactions BEGIN SELECT RAISE(ABORT, 'simulated failure'); END;")?;
    assert!(
        !ledger
            .call("acme", &["txn", "approve", id(&proposal)?], None)?
            .status
            .success()
    );
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("entries")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    let status: String = ledger.db.conn().query_row(
        "SELECT status FROM txn_proposals WHERE id = ?1",
        [id(&proposal)?],
        |row| row.get(0),
    )?;
    assert_eq!(status, "pending");
    ledger
        .db
        .conn()
        .execute_batch("DROP TRIGGER reject_test_post;")?;
    ledger.run("acme", &["txn", "approve", id(&proposal)?], None)?;
    ledger.run("acme", &["verify"], None)?;
    Ok(())
}

#[test]
fn proposal_resolution_is_company_scoped_and_rejections_cannot_be_approved() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    let approver = ledger.issue(&["ledger/approve", "ledger/read"])?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let proposal = ledger.propose(&proposer, "company")?;
    error_code(
        &ledger.call(
            "other",
            &["txn", "approve", id(&proposal)?],
            Some(&approver.token),
        )?,
        "NOT_FOUND",
        5,
    )?;
    error_code(
        &ledger.call(
            "other",
            &["txn", "reject", id(&proposal)?],
            Some(&approver.token),
        )?,
        "NOT_FOUND",
        5,
    )?;
    ledger.run(
        "acme",
        &["txn", "reject", id(&proposal)?],
        Some(&approver.token),
    )?;
    error_code(
        &ledger.call(
            "acme",
            &["txn", "approve", id(&proposal)?],
            Some(&approver.token),
        )?,
        "VALIDATION",
        3,
    )?;
    assert_eq!(ledger.count("transactions")?, 0);
    ledger.run("acme", &["verify"], Some(&approver.token))?;
    Ok(())
}

#[test]
fn period_lock_blocks_approval_atomically_and_reopen_requires_admin() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    let approver = ledger.issue(&["ledger/approve", "ledger/read"])?;
    let admin = ledger.issue(&["ledger/admin", "ledger/read"])?;
    let proposal = ledger.propose(&proposer, "closed-receipt")?;
    let id = id(&proposal)?;
    ledger.run(
        "acme",
        &["policy", "set", "--enforce", "on"],
        Some(&admin.token),
    )?;
    let close = ["period", "close", "--through", "2026-01-31"];
    let reopen = ["period", "reopen", "--reason", "Missing receipt correction"];
    error_code(
        &ledger.call("acme", &close, Some(&approver.token))?,
        "FORBIDDEN",
        7,
    )?;
    ledger.run("acme", &close, Some(&admin.token))?;
    let history = ledger.count("audit_log")?;
    error_code(
        &ledger.call("acme", &["txn", "approve", id], Some(&approver.token))?,
        "VALIDATION",
        3,
    )?;
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("entries")?, 0);
    assert_eq!(ledger.count("audit_log")?, history);
    let pending = beankeeper_cli::db::proposals::get(ledger.db.conn(), "acme", id)?;
    assert_eq!(pending.state.code(), "pending");
    error_code(
        &ledger.call("acme", &reopen, Some(&approver.token))?,
        "FORBIDDEN",
        7,
    )?;
    error_code(&ledger.call("acme", &reopen, None)?, "UNAUTHORIZED", 6)?;
    ledger.run("acme", &reopen, Some(&admin.token))?;
    let reopened = ledger.run("acme", &["period", "list"], Some(&admin.token))?;
    assert_eq!(reopened[0]["reason"], "Missing receipt correction");
    ledger.run("acme", &["txn", "approve", id], Some(&approver.token))?;
    assert_eq!(ledger.count("transactions")?, 1);
    ledger.run("acme", &["verify"], Some(&approver.token))?;
    let attribution: (Option<String>, Option<String>) = ledger.db.conn().query_row(
        "SELECT agent_uri, token_jti FROM audit_log WHERE action = 'period_reopen'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(attribution.0.as_deref(), Some(admin.uri.as_str()));
    assert!(attribution.1.is_some());
    Ok(())
}
