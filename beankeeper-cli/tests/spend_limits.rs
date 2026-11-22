//! Agent spend controls through the real CLI and persisted ledger.

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use agent_uri_attestation::{AttestationClaims, Issuer, SigningKey};
use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, IssueRequest};
use rusqlite::params;
use secrecy::SecretString;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
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
        let path = dir.path().join("limits.db");
        let passphrase_file = dir.path().join("passphrase");
        std::fs::write(&passphrase_file, "spend-limit-test-passphrase")?;
        let passphrase: SecretString = "spend-limit-test-passphrase".into();
        let db = Db::open(&path, Some(&passphrase))?;
        for company in ["acme", "other"] {
            db::create_company(
                db.conn(),
                &Actor::new("fixture"),
                &company.parse()?,
                company,
                None,
            )?;
            for (code, kind) in [
                ("1000", beankeeper::types::AccountType::Asset),
                ("2000", beankeeper::types::AccountType::Liability),
                ("5000", beankeeper::types::AccountType::Expense),
                ("5100", beankeeper::types::AccountType::Expense),
            ] {
                db::create_account(
                    db.conn(),
                    &Actor::new("fixture"),
                    &CreateAccountParams {
                        company_slug: &company.parse()?,
                        code,
                        name: code,
                        account_type: kind,
                        default_tax_category: None,
                    },
                )?;
            }
        }
        identity::init_root(db.conn(), &Actor::new("owner"))?;
        Ok(Self {
            dir,
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
            .args(["--json", "--company", company, "--actor", "limit-test"]);
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
                name: "spender",
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

    fn issue_for_uri(&self, uri: &str, grants: &[&str]) -> Result<Agent, Box<dyn Error>> {
        let registered = self.issue(grants)?;
        let second_identity = identity::verify(self.db.conn(), &registered.token)?;
        let (root, raw): (String, Vec<u8>) = self.db.conn().query_row(
            "SELECT trust_root, private_key FROM identity_root",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let agent_key = SigningKey::generate().verifying_key();
        let claims = AttestationClaims::builder()
            .jti(second_identity.jti())
            .agent_uri(uri)
            .agent_key(&agent_key)
            .capabilities(grants.iter().map(ToString::to_string).collect::<Vec<_>>())
            .issuer(&root)
            .ttl(Duration::from_secs(3600))
            .build()?;
        // Reissue a second registered token for the same agent, as on key rotation.
        self.db.conn().execute(
            "UPDATE agent_tokens SET agent_uri = ?1, agent_key = ?2 WHERE jti = ?3",
            params![claims.agent_uri, claims.agent_key, claims.jti],
        )?;
        let seed: &[u8; 32] = raw.as_slice().try_into()?;
        let token = Issuer::new(
            root,
            SigningKey::from_bytes(seed)?,
            Duration::from_secs(3600),
        )
        .issue_claims(&claims)?;
        Ok(Agent {
            token,
            uri: uri.to_owned(),
        })
    }

    fn count(&self, table: &str) -> Result<i64, Box<dyn Error>> {
        Ok(self
            .db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }

    fn post(&self, agent: &Agent, amount: &str, date: &str) -> Result<Output, Box<dyn Error>> {
        self.call(
            "acme",
            &[
                "txn",
                "post",
                "-d",
                "Expense",
                "--debit",
                &format!("5000:{amount}"),
                "--credit",
                &format!("1000:{amount}"),
                "--date",
                date,
            ],
            Some(&agent.token),
        )
    }
}

fn forbidden(output: &Output) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, [] as [u8; 0]);
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "FORBIDDEN");
    Ok(())
}

#[test]
fn max_post_includes_every_debit_and_denials_leave_no_ledger_changes() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = ledger.issue(&["ledger/post", "ledger/read"])?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &agent.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "500",
        ],
        None,
    )?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let allowed = ledger.post(&agent, "500", "2026-01-01")?;
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let before = (
        ledger.count("transactions")?,
        ledger.count("entries")?,
        ledger.count("audit_log")?,
    );
    let denied = ledger.post(&agent, "500.01", "2026-01-01")?;
    forbidden(&denied)?;
    assert!(String::from_utf8_lossy(&denied.stderr).contains("propose"));
    forbidden(&ledger.call(
        "acme",
        &[
            "txn", "post", "-d", "Split", "--debit", "5000:300", "--debit", "5100:201", "--credit",
            "1000:501",
        ],
        Some(&agent.token),
    )?)?;
    assert_eq!(
        before,
        (
            ledger.count("transactions")?,
            ledger.count("entries")?,
            ledger.count("audit_log")?
        )
    );
    ledger.run("acme", &["verify"], Some(&agent.token))?;
    Ok(())
}

#[test]
fn daily_limit_uses_transaction_date_and_pending_proposals_do_not_consume_it() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = ledger.issue(&["ledger/post", "ledger/propose", "ledger/read"])?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &agent.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--daily-limit",
            "500",
        ],
        None,
    )?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    ledger.run(
        "acme",
        &[
            "txn",
            "propose",
            "-d",
            "Pending",
            "--debit",
            "5000:10000",
            "--credit",
            "1000:10000",
            "--date",
            "2026-01-01",
        ],
        Some(&agent.token),
    )?;
    assert!(ledger.post(&agent, "400", "2026-1-1")?.status.success());
    let stored_date: String =
        ledger
            .db
            .conn()
            .query_row("SELECT date FROM transactions", [], |row| row.get(0))?;
    assert_eq!(stored_date, "2026-01-01");
    forbidden(&ledger.post(&agent, "400", "2026-01-01")?)?;
    assert!(ledger.post(&agent, "100", "2026-01-01")?.status.success());
    forbidden(&ledger.post(&agent, "0.01", "2026-01-01")?)?;
    assert!(ledger.post(&agent, "400", "2026-01-02")?.status.success());
    assert_eq!(ledger.count("transactions")?, 3);
    ledger.run("acme", &["verify"], Some(&agent.token))?;
    Ok(())
}

#[test]
fn limits_fail_closed_on_other_currencies_but_stay_scoped_to_agent_and_company() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = ledger.issue(&["ledger/post"])?;
    let unlimited = ledger.issue(&["ledger/post"])?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &agent.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "10",
        ],
        None,
    )?;
    // Amount limits are independent of whether capability enforcement is enabled.
    forbidden(&ledger.post(&agent, "11", "2026-01-01")?)?;
    forbidden(&ledger.call(
        "acme",
        &[
            "txn",
            "post",
            "-d",
            "Foreign",
            "--debit",
            "5000:1",
            "--credit",
            "1000:1",
            "--currency",
            "EUR",
        ],
        Some(&agent.token),
    )?)?;
    ledger.run(
        "other",
        &[
            "txn",
            "post",
            "-d",
            "Other company",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
        ],
        Some(&agent.token),
    )?;
    assert!(
        ledger
            .post(&unlimited, "100000", "2026-01-01")?
            .status
            .success()
    );
    ledger.run(
        "acme",
        &["agent", "limit", "clear", &agent.uri, "--company", "acme"],
        None,
    )?;
    assert!(
        ledger
            .post(&agent, "100000", "2026-01-01")?
            .status
            .success()
    );
    ledger.run("acme", &["verify"], None)?;
    Ok(())
}

#[test]
fn setting_and_clearing_limits_require_admin_when_enforcement_is_on() -> TestResult {
    let ledger = Ledger::new()?;
    let post = ledger.issue(&["ledger/post"])?;
    let admin = ledger.issue(&["ledger/admin", "ledger/read"])?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let set = [
        "agent",
        "limit",
        "set",
        &post.uri,
        "--company",
        "acme",
        "--currency",
        "JPY",
        "--max-post",
        "10",
        "--daily-limit",
        "20",
    ];
    forbidden(&ledger.call("acme", &set, Some(&post.token))?)?;
    ledger.run("acme", &set, Some(&admin.token))?;
    let before = ledger.count("audit_log")?;
    forbidden(&ledger.call(
        "acme",
        &["agent", "limit", "clear", &post.uri],
        Some(&post.token),
    )?)?;
    assert_eq!(ledger.count("audit_log")?, before);
    assert!(
        ledger
            .call(
                "acme",
                &[
                    "txn",
                    "post",
                    "-d",
                    "JPY",
                    "--currency",
                    "JPY",
                    "--debit",
                    "5000:10",
                    "--credit",
                    "1000:10"
                ],
                Some(&post.token)
            )?
            .status
            .success()
    );
    forbidden(&ledger.call(
        "acme",
        &[
            "txn",
            "post",
            "-d",
            "JPY",
            "--currency",
            "JPY",
            "--debit",
            "5000:11",
            "--credit",
            "1000:11",
        ],
        Some(&post.token),
    )?)?;
    ledger.run(
        "acme",
        &["agent", "limit", "clear", &post.uri],
        Some(&admin.token),
    )?;
    assert!(
        ledger
            .call(
                "acme",
                &[
                    "txn",
                    "post",
                    "-d",
                    "JPY",
                    "--currency",
                    "JPY",
                    "--debit",
                    "5000:100",
                    "--credit",
                    "1000:100"
                ],
                Some(&post.token)
            )?
            .status
            .success()
    );
    ledger.run("acme", &["verify"], Some(&admin.token))?;
    Ok(())
}

#[test]
fn multiple_live_tokens_for_one_uri_share_the_daily_bucket() -> TestResult {
    let ledger = Ledger::new()?;
    let first = ledger.issue(&["ledger/post"])?;
    let second = ledger.issue_for_uri(&first.uri, &["ledger/post"])?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &first.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--daily-limit",
            "500",
        ],
        None,
    )?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    assert!(ledger.post(&first, "400", "2026-01-01")?.status.success());
    forbidden(&ledger.post(&second, "400", "2026-01-01")?)?;
    assert!(ledger.post(&second, "100", "2026-01-01")?.status.success());
    forbidden(&ledger.post(&first, "0.01", "2026-01-01")?)?;
    assert_eq!(ledger.count("transactions")?, 2);
    Ok(())
}

fn check_import_limits(ledger: &Ledger, agent: &Agent) -> TestResult {
    let csv = ledger.dir.path().join("over-limit.csv");
    std::fs::write(
        &csv,
        "date,description,amount,reference\n2026-01-01,Purchase,-11,import-limit\n",
    )?;
    forbidden(&ledger.call(
        "acme",
        &[
            "txn",
            "import",
            "--file",
            csv.to_str().ok_or("CSV path")?,
            "--account",
            "1000",
            "--suspense",
            "5000",
        ],
        Some(&agent.token),
    )?)?;
    let ofx = ledger.dir.path().join("over-limit.ofx");
    std::fs::write(
        &ofx,
        "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n\
        <OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS>\
        <DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>\
        <BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>\
        <STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>123<ACCTID>456\
        <ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20260101<DTEND>20260131\
        <STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260101<TRNAMT>-11<FITID>OFX-LIMIT\
        <NAME>Purchase</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>0\
        <DTASOF>20260131</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>",
    )?;
    forbidden(&ledger.call(
        "acme",
        &[
            "txn",
            "import",
            "--file",
            ofx.to_str().ok_or("OFX path")?,
            "--account",
            "1000",
            "--suspense",
            "5000",
        ],
        Some(&agent.token),
    )?)?;
    Ok(())
}

#[test]
fn imports_loans_and_scheduled_postings_cannot_bypass_spend_limits() -> TestResult {
    let ledger = Ledger::new()?;
    let agent = ledger.issue(&["ledger/post", "ledger/read"])?;
    ledger.run(
        "acme",
        &[
            "agent",
            "limit",
            "set",
            &agent.uri,
            "--company",
            "acme",
            "--currency",
            "USD",
            "--max-post",
            "10",
        ],
        None,
    )?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    check_import_limits(&ledger, &agent)?;
    forbidden(&ledger.call(
        "acme",
        &[
            "loan",
            "pay",
            "--principal",
            "20",
            "--rate",
            "0",
            "--months",
            "1",
            "--payment-number",
            "1",
            "--liability",
            "2000",
            "--interest-account",
            "5000",
            "--paying-account",
            "1000",
            "--date",
            "2026-01-01",
            "--reference",
            "loan-limit",
        ],
        Some(&agent.token),
    )?)?;
    ledger.run(
        "acme",
        &[
            "schedule",
            "create",
            "-d",
            "Over limit",
            "--start",
            "2026-01-01",
            "--frequency",
            "once",
            "--debit",
            "5000:11",
            "--credit",
            "1000:11",
        ],
        Some(&agent.token),
    )?;
    let audits = ledger.count("audit_log")?;
    forbidden(&ledger.call(
        "acme",
        &["schedule", "run", "--through", "2026-01-01"],
        Some(&agent.token),
    )?)?;
    assert_eq!(ledger.count("transactions")?, 0);
    assert_eq!(ledger.count("entries")?, 0);
    assert_eq!(ledger.count("audit_log")?, audits);
    let next_due: String =
        ledger
            .db
            .conn()
            .query_row("SELECT next_due FROM schedules", [], |row| row.get(0))?;
    assert_eq!(next_due, "2026-01-01");
    ledger.run("acme", &["verify"], Some(&agent.token))?;
    Ok(())
}

#[test]
fn approve_only_token_can_resolve_proposals_created_by_another_token_for_its_uri() -> TestResult {
    let ledger = Ledger::new()?;
    let proposer = ledger.issue(&["ledger/propose"])?;
    let reviewer = ledger.issue_for_uri(&proposer.uri, &["ledger/approve"])?;
    let read = ledger.issue(&["ledger/read"])?;
    ledger.run("acme", &["policy", "set", "--enforce", "on"], None)?;
    let reviewer_identity = identity::verify(ledger.db.conn(), &reviewer.token)?;
    assert_ne!(
        reviewer_identity.jti(),
        identity::verify(ledger.db.conn(), &proposer.token)?.jti()
    );
    for operation in ["reject", "withdraw"] {
        let proposal = ledger.run(
            "acme",
            &[
                "txn", "propose", "-d", "Review", "--debit", "5000:10", "--credit", "1000:10",
            ],
            Some(&proposer.token),
        )?;
        let proposal_id = proposal["id"].as_str().ok_or("missing proposal ID")?;
        let resolved = ledger.run(
            "acme",
            &["txn", operation, proposal_id, "--reason", "Correct receipt"],
            Some(&reviewer.token),
        )?;
        assert_eq!(resolved["status"], "withdrawn");
        assert_eq!(resolved["resolved_by_uri"], proposer.uri);
        assert_eq!(resolved["resolved_by_jti"], reviewer_identity.jti());
    }
    assert_eq!(ledger.count("transactions")?, 0);
    ledger.run("acme", &["verify"], Some(&read.token))?;
    Ok(())
}
