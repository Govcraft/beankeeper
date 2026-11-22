//! Phase 1 identity lifecycle and attribution through the real CLI.

#[path = "support/raw_ledger.rs"]
mod raw_ledger;

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

use agent_uri::AgentUri;
use agent_uri_attestation::{AttestationClaims, Denylist, Issuer, SigningKey, Verifier};
use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, Admin, Granted};
use chrono::Utc;
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn Error>>;

struct Ledger {
    dir: TempDir,
    path: PathBuf,
    passphrase_file: PathBuf,
    db: Db,
}

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let passphrase_file = dir.path().join("passphrase");
        std::fs::write(&passphrase_file, "test-only-passphrase")?;
        let passphrase: SecretString = "test-only-passphrase".into();
        let db = Db::open(&path, Some(&passphrase))?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, "Acme", None)
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "4000",
                    name: "Revenue",
                    account_type: beankeeper::types::AccountType::Revenue,
                    default_tax_category: None,
                },
            )
        })?;
        Ok(Self {
            dir,
            path,
            passphrase_file,
            db,
        })
    }

    fn raw(&self) -> rusqlite::Result<rusqlite::Connection> {
        raw_ledger::open(&self.path, Some("test-only-passphrase"))
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        let mut cmd = Command::cargo_bin("bk")?;
        cmd.arg("--db")
            .arg(&self.path)
            .arg("--passphrase-file")
            .arg(&self.passphrase_file)
            .args(["--json", "--company", "acme", "--actor", "host-process"]);
        for var in [
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
        ] {
            cmd.env_remove(var);
        }
        Ok(cmd)
    }

    fn run(&self, args: &[&str], token: Option<&str>) -> Result<Value, Box<dyn Error>> {
        let mut cmd = self.command()?;
        if let Some(token) = token {
            cmd.args(["--agent-token", token]);
        }
        success(&cmd.args(args).output()?)
    }

    fn issue(&self) -> Result<Value, Box<dyn Error>> {
        self.run(&["key", "init"], None)?;
        Ok(self.run(
            &["agent", "issue", "--name", "claude", "--generate-key"],
            None,
        )?["data"]
            .clone())
    }

    fn post(&self, reference: &str, token: Option<&str>) -> Result<Value, Box<dyn Error>> {
        self.run(
            &[
                "txn",
                "post",
                "-d",
                "Sale",
                "--date",
                "2026-01-01",
                "--debit",
                "1000:10",
                "--credit",
                "4000:10",
                "--reference",
                reference,
            ],
            token,
        )
    }

    fn count(&self, table: &str) -> Result<i64, rusqlite::Error> {
        self.db
            .read()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
    }
}

fn success(output: &Output) -> Result<Value, Box<dyn Error>> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, Box<dyn Error>> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}").into())
}

#[test]
fn ui_tokens_preserve_scope_and_require_explicit_navigation_authority() -> TestResult {
    use beankeeper_cli::capability::Capability;
    use beankeeper_cli::error::CliError;

    let ledger = Ledger::new()?;
    let legacy = ledger.issue()?;
    let issue = |caps| {
        ledger.run(
            &[
                "agent",
                "issue",
                "--name",
                "desktop",
                "--generate-key",
                "--caps",
                caps,
            ],
            None,
        )
    };
    let navigation = issue("ui/navigate")?["data"].clone();
    let narrow = issue("ui/navigate/panel")?["data"].clone();
    let administrator = issue("ledger/admin")?;
    let owner = Actor::new("owner".parse()?).with_identity(identity::verify(
        ledger.db.read(),
        text(&administrator["data"], "token")?,
    )?);
    let uri = AgentUri::parse(text(&navigation, "agent_uri")?)?;
    assert_eq!(uri.capability_path().as_str(), "ui");
    assert_eq!(navigation["capabilities"], json!(["ui/navigate"]));
    let identity = identity::verify(ledger.db.read(), text(&navigation, "token")?)?;
    assert!(identity.allows(Capability::Navigate)?);
    assert!(!identity.allows(Capability::Read)?);
    assert!(!identity.allows(Capability::Post)?);
    let legacy_identity = identity::verify(ledger.db.read(), text(&legacy, "token")?)?;
    assert_eq!(
        AgentUri::parse(legacy_identity.uri())?
            .capability_path()
            .as_str(),
        "ledger"
    );
    assert!(legacy_identity.allows(Capability::Post)?);
    assert!(!legacy_identity.allows(Capability::Navigate)?);
    let authorize = |token, capability| identity::authorize(ledger.db.read(), token, capability);
    for enforce in [false, true] {
        identity::Granted::<identity::Admin>::with_db(&ledger.db, &owner, |grant| {
            db::audit::set_policy(grant, enforce)
        })?;
        assert!(authorize(Some(text(&navigation, "token")?), Capability::Navigate)?.is_some());
        for issued in [&legacy, &narrow] {
            assert!(matches!(
                authorize(Some(text(issued, "token")?), Capability::Navigate),
                Err(CliError::Forbidden(_))
            ));
        }
        assert!(matches!(
            authorize(None, Capability::Navigate),
            Err(CliError::Unauthorized(_))
        ));
    }
    assert!(matches!(
        authorize(Some(text(&navigation, "token")?), Capability::Read),
        Err(CliError::Forbidden(_))
    ));
    identity::Granted::<identity::Admin>::with_db(&ledger.db, &owner, |grant| {
        db::audit::set_policy(grant, false)
    })?;
    let before = ledger.count("agent_tokens")?;
    for caps in [
        "ui",
        "ledger",
        "ledger/read,ui/navigate",
        "outside/post",
        "ui/navigate,",
    ] {
        let output = ledger
            .command()?
            .args([
                "agent",
                "issue",
                "--name",
                "desktop",
                "--generate-key",
                "--caps",
                caps,
            ])
            .output()?;
        assert!(!output.status.success(), "accepted {caps}");
    }
    assert_eq!(ledger.count("agent_tokens")?, before);
    Granted::<Admin>::with_db(&ledger.db, &Actor::new("owner".parse()?), |grant| {
        identity::revoke(grant, identity.jti())
    })?;
    assert!(matches!(
        identity::verify(ledger.db.read(), text(&navigation, "token")?),
        Err(CliError::Unauthorized(_))
    ));
    ledger.run(&["verify"], None)?;
    Ok(())
}

fn unauthorized(output: &Output) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(value["error"]["code"], "UNAUTHORIZED");
    assert!(
        !value["meta"]
            .as_object()
            .ok_or("missing meta")?
            .contains_key("agent")
    );
    Ok(())
}

#[test]
fn loan_pay_preserves_verified_agent_attribution_and_rejects_invalid_tokens() -> TestResult {
    let ledger = Ledger::new()?;
    Granted::<Admin>::with_db(&ledger.db, &Actor::new("fixture".parse()?), |grant| {
        db::create_account(
            grant,
            &CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "2000",
                name: "Loan",
                account_type: beankeeper::types::AccountType::Liability,
                default_tax_category: None,
            },
        )
    })?;
    Granted::<Admin>::with_db(&ledger.db, &Actor::new("fixture".parse()?), |grant| {
        db::create_account(
            grant,
            &CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "5000",
                name: "Interest",
                account_type: beankeeper::types::AccountType::Expense,
                default_tax_category: None,
            },
        )
    })?;
    let issued = ledger.issue()?;
    let token = text(&issued, "token")?;
    let args = [
        "loan",
        "pay",
        "--principal",
        "1000",
        "--rate",
        "6",
        "--months",
        "12",
        "--payment-number",
        "1",
        "--liability",
        "2000",
        "--interest-account",
        "5000",
        "--paying-account",
        "1000",
        "--date",
        "2026-11-20",
        "--reference",
        "LOAN-1",
    ];
    let invalid = ledger
        .command()?
        .args(["--agent-token", "invalid"])
        .args(args)
        .output()?;
    unauthorized(&invalid)?;
    assert_eq!(ledger.count("transactions")?, 0);
    let result = ledger.run(&args, Some(token))?;
    assert_eq!(result["meta"]["command"], "loan.pay");
    assert_eq!(result["meta"]["agent"], issued["agent_uri"]);
    let id = result["data"]["id"].to_string();
    let shown = ledger.run(&["txn", "show", &id], None)?;
    assert_eq!(shown["data"][0]["agent_uri"], issued["agent_uri"]);
    assert_eq!(shown["data"][0]["token_jti"], issued["token_jti"]);
    assert_eq!(shown["data"][0]["posted_by"], "host-process");
    let (uri, jti): (String, String) = ledger.db.read().query_row(
        "SELECT agent_uri, token_jti FROM audit_log WHERE entity = 'transaction' AND action = 'create'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(uri, text(&issued, "agent_uri")?);
    assert_eq!(jti, text(&issued, "token_jti")?);
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn key_and_token_lifecycle_binds_supplied_key_and_never_lists_secrets() -> TestResult {
    let ledger = Ledger::new()?;
    let creations = ledger.count("audit_log")?;
    let root = ledger.run(&["key", "init"], None)?["data"].clone();
    assert!(text(&root, "trust_root")?.ends_with(".bk.local"));
    let key = SigningKey::generate();
    let issued = ledger.run(
        &[
            "agent",
            "issue",
            "--name",
            "bookkeeper",
            "--public-key",
            &key.verifying_key().to_base64(),
            "--caps",
            "ledger/read,ledger/reverse,ledger/admin",
            "--ttl",
            "2h",
        ],
        None,
    )?["data"]
        .clone();
    assert!(issued.get("private_key").is_none());
    let token = text(&issued, "token")?;
    let uri = AgentUri::parse(text(&issued, "agent_uri")?)?;
    let mut verifier = Verifier::new().with_revocation(Denylist::new());
    verifier.add_trusted_root(
        text(&root, "trust_root")?,
        agent_uri_attestation::VerifyingKey::from_base64(text(&root, "public_key")?)?,
    );
    let claims = verifier.verify_for_uri(token, &uri)?;
    assert_eq!(claims.agent_verifying_key()?, key.verifying_key());
    assert_eq!(identity::verify(ledger.db.read(), token)?.jti(), claims.jti);
    let listed = ledger.run(&["agent", "list"], None)?;
    assert_eq!(listed["data"][0]["token_jti"], issued["token_jti"]);
    assert!(!listed.to_string().contains(token));
    assert!(!listed.to_string().contains("private_key"));
    let repeated = ledger.command()?.args(["key", "init"]).output()?;
    assert_eq!(repeated.status.code(), Some(3));
    assert_eq!(ledger.count("identity_root")?, 1);
    assert_eq!(ledger.count("audit_log")?, creations + 2);
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn tokenless_behavior_and_explicit_flag_over_environment_remain_compatible() -> TestResult {
    let ledger = Ledger::new()?;
    let token = ledger.issue()?;
    let uri = text(&token, "agent_uri")?;
    let posted = ledger.post("named", None)?;
    assert!(posted["meta"].get("agent").is_none());
    let original = ledger.run(&["txn", "show", "1"], None)?;
    assert_eq!(original["data"][0]["posted_by"], "host-process");
    assert!(original["data"][0]["agent_uri"].is_null());
    let env = ledger
        .command()?
        .env("BK_AGENT_TOKEN", text(&token, "token")?)
        .args(["txn", "show", "1"])
        .output()?;
    assert_eq!(success(&env)?["meta"]["agent"], uri);
    let flag = ledger
        .command()?
        .env("BK_AGENT_TOKEN", "invalid-env-token")
        .args(["--agent-token", text(&token, "token")?, "agent", "list"])
        .output()?;
    assert_eq!(success(&flag)?["meta"]["agent"], uri);
    let debug = format!(
        "{:?}",
        <beankeeper_cli::cli::Cli as clap::Parser>::try_parse_from([
            "bk",
            "--agent-token",
            "do-not-print",
            "verify"
        ])?
    );
    assert!(!debug.contains("do-not-print"));
    Ok(())
}

#[test]
fn identity_metadata_preserves_pagination_and_currency_displays() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let token = text(&issued, "token")?;
    let uri = text(&issued, "agent_uri")?;
    ledger.post("named", None)?;
    ledger.post("agent-first", Some(token))?;
    ledger.post("agent-second", Some(token))?;

    let mut previous_id = Value::Null;
    for offset in [0, 1] {
        let page = ledger.run(
            &[
                "audit",
                "log",
                "--agent",
                uri,
                "--entity",
                "transaction",
                "--action",
                "create",
                "--limit",
                "1",
                "--offset",
                &offset.to_string(),
            ],
            Some(token),
        )?;
        assert_eq!(page["meta"]["agent"], uri);
        assert_eq!(
            page["meta"]["pagination"],
            json!({"limit":1,"offset":offset,"returned":1,"total":2,"has_more":offset == 0})
        );
        assert_eq!(page["data"].as_array().map(Vec::len), Some(1));
        let row = &page["data"][0];
        assert_ne!(row["id"], previous_id);
        previous_id = row["id"].clone();
        assert_eq!(row["agent_uri"], uri);
        assert_eq!(row["token_jti"], issued["token_jti"]);
        assert!(row["after"].is_string());
        assert_eq!(row["after_display"]["entries"][0]["amount"], 1000);
        assert_eq!(
            row["after_display"]["entries"][0]["amount_display"],
            "10.00"
        );
    }

    let txns = ledger.run(
        &["txn", "list", "--limit", "1", "--offset", "1"],
        Some(token),
    )?;
    assert_eq!(txns["meta"]["agent"], uri);
    assert_eq!(
        txns["meta"]["pagination"],
        json!({"limit":1,"offset":1,"returned":1,"total":3,"has_more":true})
    );
    assert_eq!(txns["data"][0]["entries"][0]["amount_display"], "10.00");

    let report = ledger.run(
        &["report", "trial-balance", "--currency", "USD"],
        Some(token),
    )?;
    assert_eq!(report["meta"]["agent"], uri);
    assert_eq!(report["meta"]["currency"], "USD");
    assert_eq!(report["data"]["total_debits_display"], "30.00");
    Ok(())
}

#[test]
fn bad_foreign_expired_future_and_revoked_tokens_fail_before_writes() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let token = text(&issued, "token")?;
    let other = Ledger::new()?;
    other.run(&["key", "init"], None)?;
    let expired = dated_token(&ledger, &issued, -7200)?;
    let future = dated_token(&ledger, &issued, 7200)?;
    let mut tampered = token.to_owned();
    tampered.replace_range(15..16, if &token[15..16] == "A" { "B" } else { "A" });
    for invalid in ["", "invalid", &expired, &future, &tampered] {
        unauthorized(
            &ledger
                .command()?
                .args([
                    "--agent-token",
                    invalid,
                    "txn",
                    "post",
                    "-d",
                    "Bad",
                    "--debit",
                    "1000:10",
                    "--credit",
                    "4000:10",
                ])
                .output()?,
        )?;
    }
    unauthorized(
        &other
            .command()?
            .args(["--agent-token", token, "company", "create", "bad", "Bad"])
            .output()?,
    )?;
    assert_eq!(other.count("companies")?, 1);
    assert_eq!(ledger.count("transactions")?, 0);
    let before = ledger.count("audit_log")?;
    let revoked = ledger.run(&["agent", "revoke", text(&issued, "token_jti")?], None)?;
    assert_eq!(revoked["data"]["revoked"], 1);
    assert_eq!(
        ledger.run(&["agent", "revoke", text(&issued, "agent_uri")?], None)?["data"]["revoked"],
        0
    );
    unauthorized(
        &ledger
            .command()?
            .env("BK_AGENT_TOKEN", token)
            .arg("verify")
            .output()?,
    )?;
    assert_eq!(ledger.count("audit_log")?, before + 1);
    // Reads still work without a token after revocation.
    ledger.run(&["verify"], None)?;
    Ok(())
}

fn dated_token(ledger: &Ledger, issued: &Value, seconds: i64) -> Result<String, Box<dyn Error>> {
    let seed: [u8; 32] =
        ledger
            .db
            .read()
            .query_row("SELECT private_key FROM identity_root", [], |r| r.get(0))?;
    let root: String =
        ledger
            .db
            .read()
            .query_row("SELECT trust_root FROM identity_root", [], |r| r.get(0))?;
    let authority = Issuer::new(
        &root,
        SigningKey::from_bytes(&seed)?,
        std::time::Duration::from_secs(3600),
    );
    let claims = AttestationClaims::builder()
        .jti(text(issued, "token_jti")?)
        .agent_uri(text(issued, "agent_uri")?)
        .agent_key(&agent_uri_attestation::VerifyingKey::from_base64(text(
            issued,
            "agent_key",
        )?)?)
        .capabilities(vec!["ledger/post".into()])
        .issuer(&root)
        .ttl(std::time::Duration::from_secs(3600))
        .build_at(Utc::now() + chrono::Duration::seconds(seconds))?;
    Ok(authority.issue_claims(&claims)?)
}

#[test]
fn posts_imports_reversals_and_mutations_all_record_verified_claims() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let token = text(&issued, "token")?;
    ledger.post("sale", Some(token))?;
    let imported = ledger.dir.path().join("batch.json");
    std::fs::write(&imported, json!([{"date":"2026-01-02", "description":"Import", "reference":"json-row", "entries":[{"account_code":"1000", "direction":"debit", "amount":"12"},{"account_code":"4000", "direction":"credit", "amount":"12"}]}]).to_string())?;
    ledger.run(
        &[
            "txn",
            "import",
            "--file",
            imported.to_str().ok_or("path")?,
            "--input-format",
            "json",
        ],
        Some(token),
    )?;
    ledger.run(
        &["txn", "reverse", "1", "--date", "2026-01-03"],
        Some(token),
    )?;
    bank_imports(&ledger, token)?;
    mutation_commands(&ledger, token)?;
    let rows = ledger.run(
        &[
            "audit",
            "log",
            "--agent",
            text(&issued, "agent_uri")?,
            "--limit",
            "100",
        ],
        None,
    )?;
    let rows = rows["data"].as_array().ok_or("audit rows")?;
    for row in rows {
        assert_eq!(row["agent_uri"], issued["agent_uri"]);
        assert_eq!(row["token_jti"], issued["token_jti"]);
        assert_eq!(row["actor"], "host-process");
        assert_eq!(row["hash_version"], 4);
    }
    for action in [
        "create",
        "reverse",
        "status_change",
        "budget_set",
        "budget_delete",
        "account_delete",
        "company_delete",
        "attach",
        "token_issue",
        "token_revoke",
    ] {
        assert!(
            rows.iter().any(|r| r["action"] == action),
            "missing {action}"
        );
    }
    let txns = ledger.run(&["txn", "list"], Some(token))?;
    assert_eq!(txns["meta"]["agent"], issued["agent_uri"]);
    for txn in txns["data"].as_array().ok_or("transaction rows")? {
        assert_eq!(txn["agent_uri"], issued["agent_uri"]);
        assert_eq!(txn["token_jti"], issued["token_jti"]);
    }
    ledger.run(&["verify"], None)?;
    Ok(())
}

fn bank_imports(ledger: &Ledger, token: &str) -> TestResult {
    let csv = "date,description,amount\n2026-01-01,CSV sale,1\n";
    let ofx = "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\nENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n<OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS><DTSERVER>20260101<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1><BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS><STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>123<ACCTID>456<ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20260101<DTEND>20260131<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260101<TRNAMT>1.00<FITID>ACTOR-TEST<NAME>OFX sale</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>1.00<DTASOF>20260131</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>";
    for (format, content) in [("csv", csv), ("ofx", ofx)] {
        let path = ledger.dir.path().join(format!("import.{format}"));
        std::fs::write(&path, content)?;
        ledger.run(
            &[
                "txn",
                "import",
                "--file",
                path.to_str().ok_or("path")?,
                "--account",
                "1000",
                "--suspense",
                "4000",
            ],
            Some(token),
        )?;
    }
    Ok(())
}

fn mutation_commands(ledger: &Ledger, token: &str) -> TestResult {
    for args in [
        vec!["company", "create", "temporary", "Temporary"],
        vec!["company", "delete", "temporary", "--force"],
        vec!["account", "create", "5000", "Costs", "--type", "expense"],
        vec![
            "budget", "set", "5000", "--year", "2026", "--month", "1", "--amount", "100",
        ],
        vec![
            "budget", "delete", "5000", "--year", "2026", "--month", "1", "--force",
        ],
        vec!["account", "delete", "5000", "--force"],
        vec![
            "txn",
            "clear",
            "1",
            "--entry",
            "1",
            "--date",
            "2026-01-02",
            "--statement",
            "S1",
        ],
    ] {
        let result = ledger.run(&args, Some(token))?;
        assert!(result["meta"]["agent"].is_string(), "{result}");
    }
    let file = ledger.dir.path().join("receipt.txt");
    std::fs::write(&file, "Receipt")?;
    ledger.run(
        &[
            "txn",
            "attach",
            "1",
            file.to_str().ok_or("path")?,
            "--type",
            "receipt",
        ],
        Some(token),
    )?;
    let issued = ledger.run(
        &["agent", "issue", "--name", "secondary", "--generate-key"],
        Some(token),
    )?;
    ledger.run(
        &["agent", "revoke", text(&issued["data"], "agent_uri")?],
        Some(token),
    )?;
    Ok(())
}

#[test]
fn identity_is_immutable_and_hash_chain_detects_changes_after_trigger_bypass() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    ledger.post("sale", Some(text(&issued, "token")?))?;
    for field in ["agent_uri", "token_jti"] {
        assert!(
            ledger
                .raw()?
                .execute(&format!("UPDATE transactions SET {field} = 'forged'"), [])
                .is_err()
        );
        assert!(
            ledger
                .raw()?
                .execute(&format!("UPDATE audit_log SET {field} = 'forged'"), [])
                .is_err()
        );
    }
    ledger.raw()?.execute_batch(
        "DROP TRIGGER protect_transaction_identity; UPDATE transactions SET token_jti = 'forged'",
    )?;
    let failed = ledger.command()?.arg("verify").output()?;
    assert_eq!(failed.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("creation audit record"));
    let other = Ledger::new()?;
    other.issue()?;
    other.raw()?.execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET agent_uri = 'forged', token_jti = 'forged' WHERE id = 1")?;
    let failed = other.command()?.arg("verify").output()?;
    assert_eq!(failed.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("hash chain broken at row #1"));
    Ok(())
}

#[test]
fn invalid_issuance_and_audit_failure_leave_no_partial_registry_records() -> TestResult {
    let ledger = Ledger::new()?;
    let creations = ledger.count("audit_log")?;
    ledger.run(&["key", "init"], None)?;
    for args in [
        vec!["agent", "issue", "--name", "Bad Prefix!", "--generate-key"],
        vec![
            "agent",
            "issue",
            "--name",
            "valid",
            "--public-key",
            "invalid",
        ],
        vec![
            "agent",
            "issue",
            "--name",
            "valid",
            "--generate-key",
            "--caps",
            "outside/post",
        ],
        vec![
            "agent",
            "issue",
            "--name",
            "valid",
            "--generate-key",
            "--ttl",
            "0h",
        ],
    ] {
        assert_eq!(
            ledger.command()?.args(args).output()?.status.code(),
            Some(3)
        );
    }
    ledger.raw()?.execute_batch("CREATE TRIGGER reject_issue BEFORE INSERT ON audit_log WHEN NEW.action = 'token_issue' BEGIN SELECT RAISE(ABORT, 'test failure'); END")?;
    assert_eq!(
        ledger
            .command()?
            .args(["agent", "issue", "--name", "valid", "--generate-key"])
            .output()?
            .status
            .code(),
        Some(4)
    );
    assert_eq!(ledger.count("agent_tokens")?, 0);
    assert_eq!(ledger.count("audit_log")?, creations + 1);
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn ttl_validation_has_exact_positive_bounds_and_encryption_is_required_for_keys() -> TestResult {
    for input in [
        "",
        "0h",
        "31d",
        "8",
        "1w",
        "-1h",
        "18446744073709551615d",
        "é",
    ] {
        assert!(identity::parse_ttl(input).is_err(), "{input}");
    }
    assert_eq!(identity::parse_ttl("30d")?.as_secs(), 30 * 86400);
    let db = Db::open_in_memory()?;
    assert!(matches!(
        Granted::<Admin>::with_db(&db, &Actor::new("test".parse()?), identity::init_root),
        Err(beankeeper_cli::error::CliError::Validation(_))
    ));
    assert_eq!(
        db.read()
            .query_row("SELECT COUNT(*) FROM identity_root", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn token_preflight_reads_a_one_shot_passphrase_descriptor_only_once() -> TestResult {
    use std::io::Write;
    use std::process::Stdio;

    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let mut child = ledger
        .command()?
        .args([
            "--passphrase-fd",
            "0",
            "verify",
            "--agent-token",
            text(&issued, "token")?,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("missing stdin")?;
    input.write_all(b"test-only-passphrase\n")?;
    drop(input);
    let result = success(&child.wait_with_output()?)?;
    assert_eq!(result["meta"]["agent"], issued["agent_uri"]);
    Ok(())
}

#[test]
fn demo_honors_named_actor_and_init_rejects_tokens_before_overwriting() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let rejected = ledger
        .command()?
        .args(["--agent-token", text(&issued, "token")?, "init", "--force"])
        .output()?;
    assert_eq!(rejected.status.code(), Some(6));
    assert_eq!(ledger.count("identity_root")?, 1);
    ledger.run(&["verify"], None)?;
    let path = ledger.dir.path().join("demo.db");
    let output = Command::cargo_bin("bk")?
        .arg("--db")
        .arg(&path)
        .env_remove("BK_AGENT_TOKEN")
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .args(["--actor", "demo-host", "init", "--demo"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = Db::open_existing(&path, None)?;
    let wrong: i64 = db.read().query_row(
        "SELECT COUNT(*) FROM transactions WHERE posted_by IS NOT 'demo-host'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(wrong, 0);
    let (companies, accounts): (i64, i64) = db.read().query_row(
        "SELECT
            (SELECT COUNT(*) FROM companies c WHERE (
                SELECT COUNT(*) FROM audit_log a WHERE a.company_slug = c.slug
                AND a.entity = 'company' AND a.entity_id = c.slug
                AND a.action = 'create' AND a.actor = 'demo-host'
            ) = 1),
            (SELECT COUNT(*) FROM accounts acc WHERE (
                SELECT COUNT(*) FROM audit_log a WHERE a.company_slug = acc.company_slug
                AND a.entity = 'account' AND a.entity_id = acc.code
                AND a.action = 'create' AND a.actor = 'demo-host'
            ) = 1)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(companies, 3);
    let account_count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))?;
    assert!(account_count > 0);
    assert_eq!(accounts, account_count);
    let creations: i64 = db.read().query_row(
        "SELECT COUNT(*) FROM audit_log WHERE entity IN ('company', 'account') AND action = 'create'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(creations, companies + accounts);
    Ok(())
}

#[test]
fn metadata_annotations_record_verified_identity_without_changing_the_poster() -> TestResult {
    let ledger = Ledger::new()?;
    let issued = ledger.issue()?;
    let token = text(&issued, "token")?;
    let posted = ledger.post("annotate", None)?;
    let id = posted["data"]["id"].to_string();
    let edit = ledger.run(&["txn", "meta", "set", &id, "status=disputed"], Some(token))?;
    assert_eq!(edit["meta"]["agent"], issued["agent_uri"]);
    ledger.run(
        &["txn", "meta", "unset", "--reference", "annotate", "status"],
        Some(token),
    )?;
    for action in ["metadata_set", "metadata_unset"] {
        let rows = ledger.run(&["audit", "log", "--action", action], None)?;
        assert_eq!(rows["data"][0]["agent_uri"], issued["agent_uri"]);
        assert_eq!(rows["data"][0]["token_jti"], issued["token_jti"]);
    }
    let original = ledger.run(&["txn", "show", &id], None)?;
    assert_eq!(original["data"][0]["posted_by"], "host-process");
    assert!(original["data"][0]["agent_uri"].is_null());
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn typed_issuance_keeps_bearer_and_private_key_out_of_debug_and_audit() -> TestResult {
    use secrecy::ExposeSecret as _;

    let ledger = Ledger::new()?;
    let db = &ledger.db;
    let actor = Actor::new("owner".parse()?);
    Granted::<Admin>::with_db(db, &actor, identity::init_root)?;
    let issued = Granted::<Admin>::with_db(db, &actor, |grant| {
        identity::issue_credential(
            grant,
            &identity::IssueRequest {
                name: "typed_reader",
                capabilities: &["ledger/read".into()],
                ttl: std::time::Duration::from_mins(15),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    let secret = issued.token().expose_secret();
    let private_key = issued
        .generated_private_key()
        .ok_or("missing generated key")?;
    let preview = format!("{issued:?}");
    assert!(!preview.contains(secret));
    assert!(!preview.contains(private_key.expose_secret()));

    let verified = identity::verify(db.read(), secret)?;
    assert_eq!(verified.uri(), issued.agent_uri.as_str());
    assert_eq!(verified.jti(), issued.token_jti.as_str());
    assert_eq!(verified.capabilities(), issued.capabilities);
    assert!(issued.expires_at > issued.issued_at);

    let recorded: String = db.read().query_row(
        "SELECT after FROM audit_log WHERE action = 'token_issue' AND entity_id = ?1",
        [issued.token_jti.as_str()],
        |row| row.get(0),
    )?;
    let event: Value = serde_json::from_str(&recorded)?;
    assert_eq!(event["agent_uri"], issued.agent_uri.as_str());
    assert!(event.get("token").is_none());
    assert!(event.get("private_key").is_none());
    Ok(())
}
