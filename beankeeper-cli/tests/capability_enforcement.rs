//! Capability policy behavior through the real CLI and persisted ledger.

#[path = "support/raw_ledger.rs"]
mod raw_ledger;

use beankeeper_cli::db::CreateAccountParams;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use agent_uri_attestation::{AttestationClaims, Issuer, SigningKey};
use assert_cmd::cargo::CommandCargoExt;
use beankeeper_cli::db::{self, Actor, Db};
use beankeeper_cli::identity::{self, Admin, Granted, IssueRequest};
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

impl Ledger {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("books.db");
        let passphrase_file = dir.path().join("passphrase");
        std::fs::write(&passphrase_file, "capability-test-passphrase")?;
        let passphrase: SecretString = "capability-test-passphrase".into();
        let db = Db::open(&path, Some(&passphrase))?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        for (code, kind) in [
            ("1000", beankeeper::types::AccountType::Asset),
            ("4000", beankeeper::types::AccountType::Revenue),
            ("5000", beankeeper::types::AccountType::Expense),
        ] {
            Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
                db::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code,
                        name: code,
                        account_type: kind,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), identity::init_root)?;
        Ok(Self {
            dir,
            path,
            passphrase_file,
            db,
        })
    }

    fn raw(&self) -> rusqlite::Result<rusqlite::Connection> {
        raw_ledger::open(&self.path, Some("capability-test-passphrase"))
    }

    fn command(&self) -> Result<Command, Box<dyn Error>> {
        self.command_at(&self.path)
    }

    fn command_at(&self, path: &Path) -> Result<Command, Box<dyn Error>> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(path)
            .arg("--passphrase-file")
            .arg(&self.passphrase_file)
            .args(["--json", "--company", "acme", "--actor", "owner"]);
        for variable in [
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_CURRENCY",
        ] {
            command.env_remove(variable);
        }
        Ok(command)
    }

    fn call(&self, args: &[&str], token: Option<&str>) -> Result<Output, Box<dyn Error>> {
        let mut command = self.command()?;
        if let Some(token) = token {
            command.args(["--agent-token", token]);
        }
        Ok(command.args(args).output()?)
    }

    fn run(&self, args: &[&str], token: Option<&str>) -> Result<Value, Box<dyn Error>> {
        let output = self.call(args, token)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn issue(&self, grants: &[&str]) -> Result<String, Box<dyn Error>> {
        let issued = Granted::<Admin>::with_db(&self.db, &Actor::new("owner".parse()?), |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name: "tester",
                    capabilities: &grants.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        Ok(issued["token"].as_str().ok_or("missing token")?.to_owned())
    }

    fn enable(&self) -> TestResult {
        assert_eq!(
            self.run(&["policy", "set", "--enforce", "on"], None)?["data"]["enforce"],
            true
        );
        Ok(())
    }

    fn count(&self, table: &str) -> Result<i64, rusqlite::Error> {
        self.db
            .read()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
    }
}

fn denied(output: &Output, code: &str, exit: i32, capability: Option<&str>) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, Vec::<u8>::new());
    let error: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], code);
    if let Some(capability) = capability {
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(capability))
        );
        assert!(error["meta"]["agent"].is_string());
    }
    Ok(())
}

fn posting() -> [&'static str; 12] {
    [
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
        "sale",
    ]
}

#[test]
fn default_off_preserves_optional_tokens_and_unrestricted_named_actors() -> TestResult {
    let ledger = Ledger::new()?;
    assert_eq!(
        ledger.run(&["policy", "show"], None)?["data"]["enforce"],
        false
    );
    let read = ledger.issue(&["ledger/read"])?;
    ledger.run(&posting(), Some(&read))?;
    ledger.run(&["txn", "reverse", "1"], None)?;
    denied(
        &ledger.call(&["company", "list"], Some("invalid"))?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn read_grant_allows_reads_and_denies_all_mutations_without_side_effects() -> TestResult {
    let ledger = Ledger::new()?;
    let read = ledger.issue(&["ledger/read"])?;
    ledger.enable()?;
    for args in [
        vec!["company", "list"],
        vec!["account", "list"],
        vec!["txn", "list"],
        vec!["report", "trial-balance"],
        vec!["report", "schedule-c"],
        vec!["report", "1099"],
        vec!["report", "1099-nec"],
        vec!["verify"],
        vec!["audit", "log"],
        vec!["agent", "list"],
        vec!["policy", "show"],
    ] {
        ledger.run(&args, Some(&read))?;
    }
    let export = ledger.call(&["export", "--output-format", "json"], Some(&read))?;
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let before = (
        ledger.count("transactions")?,
        ledger.count("accounts")?,
        ledger.count("audit_log")?,
    );
    for (args, capability) in [
        (posting().to_vec(), "ledger/post"),
        (vec!["txn", "reverse", "1"], "ledger/reverse"),
        (
            vec!["txn", "clear", "1", "--entry", "1"],
            "ledger/reconcile",
        ),
        (vec!["txn", "reconcile"], "ledger/reconcile"),
        (
            vec!["budget", "set", "5000", "--year", "2026", "--annual", "100"],
            "ledger/budget",
        ),
        (
            vec!["account", "create", "2000", "Loan", "--type", "liability"],
            "ledger/admin",
        ),
        (vec!["policy", "set", "--enforce", "off"], "ledger/admin"),
    ] {
        denied(
            &ledger.call(&args, Some(&read))?,
            "FORBIDDEN",
            7,
            Some(capability),
        )?;
    }
    assert_eq!(
        before,
        (
            ledger.count("transactions")?,
            ledger.count("accounts")?,
            ledger.count("audit_log")?
        )
    );
    Ok(())
}

#[test]
fn sibling_grants_allow_only_their_own_operations_and_admin_can_disable_policy() -> TestResult {
    let ledger = Ledger::new()?;
    let post = ledger.issue(&["ledger/post"])?;
    let reverse = ledger.issue(&["ledger/reverse"])?;
    let budget = ledger.issue(&["ledger/budget"])?;
    let reconcile = ledger.issue(&["ledger/reconcile"])?;
    let admin = ledger.issue(&["ledger/admin"])?;
    ledger.enable()?;
    ledger.run(&posting(), Some(&post))?;
    denied(
        &ledger.call(&["txn", "reverse", "1"], Some(&post))?,
        "FORBIDDEN",
        7,
        Some("ledger/reverse"),
    )?;
    denied(
        &ledger.call(&["company", "list"], Some(&post))?,
        "FORBIDDEN",
        7,
        Some("ledger/read"),
    )?;
    for (args, capability) in [
        (posting().to_vec(), "ledger/post"),
        (vec!["txn", "reverse", "1"], "ledger/reverse"),
        (vec!["txn", "reconcile"], "ledger/reconcile"),
        (
            vec!["budget", "set", "5000", "--year", "2026", "--annual", "100"],
            "ledger/budget",
        ),
    ] {
        denied(
            &ledger.call(&args, Some(&admin))?,
            "FORBIDDEN",
            7,
            Some(capability),
        )?;
    }
    ledger.run(&["txn", "reverse", "1"], Some(&reverse))?;
    ledger.run(&["txn", "reconcile"], Some(&reconcile))?;
    ledger.run(
        &["budget", "set", "5000", "--year", "2026", "--annual", "100"],
        Some(&budget),
    )?;
    ledger.run(
        &["account", "create", "2000", "Loan", "--type", "liability"],
        Some(&admin),
    )?;
    denied(
        &ledger.call(&["policy", "set", "--enforce", "off"], None)?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    ledger.run(&["policy", "set", "--enforce", "off"], Some(&admin))?;
    let policy_audit = ledger.run(
        &[
            "audit",
            "log",
            "--entity",
            "policy",
            "--action",
            "policy-change",
        ],
        None,
    )?;
    assert_eq!(policy_audit["data"].as_array().map(Vec::len), Some(2));
    assert_eq!(policy_audit["data"][0]["action"], "policy_change");
    let last = policy_audit["data"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["agent_uri"].is_string()))
        .ok_or("missing admin audit")?;
    assert_eq!(
        serde_json::from_str::<Value>(last["before"].as_str().ok_or("before")?)?["enforce"],
        true
    );
    assert_eq!(
        serde_json::from_str::<Value>(last["after"].as_str().ok_or("after")?)?["enforce"],
        false
    );
    ledger.run(&["verify"], None)?;
    Ok(())
}

#[test]
fn enforcement_rejects_missing_invalid_revoked_and_foreign_tokens() -> TestResult {
    let ledger = Ledger::new()?;
    let token = ledger.issue(&["ledger/read", "ledger/post"])?;
    let other = Ledger::new()?;
    let foreign = other.issue(&["ledger/read"])?;
    let admin = ledger.issue(&["ledger/admin"])?;
    let owner =
        Actor::new("owner".parse()?).with_identity(identity::verify(ledger.db.read(), &admin)?);
    ledger.enable()?;
    for args in [
        vec!["company", "list"],
        posting().to_vec(),
        vec!["policy", "show"],
    ] {
        denied(&ledger.call(&args, None)?, "UNAUTHORIZED", 6, None)?;
    }
    for bad in ["invalid", foreign.as_str()] {
        denied(
            &ledger.call(&["company", "list"], Some(bad))?,
            "UNAUTHORIZED",
            6,
            None,
        )?;
    }
    let identity = identity::verify(ledger.db.read(), &token)?;
    assert!(matches!(
        Granted::<Admin>::with_db(&ledger.db, &Actor::new("owner".parse()?), |grant| {
            identity::revoke(grant, identity.jti())
        }),
        Err(beankeeper_cli::error::CliError::Unauthorized(_))
    ));
    Granted::<Admin>::with_db(&ledger.db, &owner, |grant| {
        identity::revoke(grant, identity.jti())
    })?;
    denied(
        &ledger.call(&posting(), Some(&token))?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    assert_eq!(ledger.count("transactions")?, 0);
    Ok(())
}

#[test]
fn bare_namespaces_are_rejected_at_issuance_and_when_accepting_legacy_tokens() -> TestResult {
    for namespace in ["ledger", "ui"] {
        reject_bare_namespace(namespace)?;
    }
    Ok(())
}

fn reject_bare_namespace(namespace: &str) -> TestResult {
    let ledger = Ledger::new()?;
    let before = (ledger.count("agent_tokens")?, ledger.count("audit_log")?);
    denied(
        &ledger.call(
            &[
                "agent",
                "issue",
                "--name",
                "unsafe_grant",
                "--generate-key",
                "--caps",
                namespace,
            ],
            None,
        )?,
        "VALIDATION",
        3,
        None,
    )?;
    assert_eq!(
        before,
        (ledger.count("agent_tokens")?, ledger.count("audit_log")?)
    );
    let issued = Granted::<Admin>::with_db(&ledger.db, &Actor::new("owner".parse()?), |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "legacy",
                capabilities: &[format!("{namespace}/read")],
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    let (root, raw): (String, Vec<u8>) = ledger.db.read().query_row(
        "SELECT trust_root, private_key FROM identity_root",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let seed: &[u8; 32] = raw.as_slice().try_into()?;
    let claims = AttestationClaims::builder()
        .jti(issued["token_jti"].as_str().ok_or("jti")?)
        .agent_uri(issued["agent_uri"].as_str().ok_or("uri")?)
        .agent_key(&SigningKey::generate().verifying_key())
        .capabilities(vec![namespace.into()])
        .issuer(&root)
        .ttl(Duration::from_secs(3600))
        .build()?;
    // Make the registry agree with this legacy token so only the bare grant rejects it.
    ledger.raw()?.execute(
        "UPDATE agent_tokens SET agent_key = ?1 WHERE jti = ?2",
        params![claims.agent_key, claims.jti],
    )?;
    let token = Issuer::new(
        root,
        SigningKey::from_bytes(seed)?,
        Duration::from_secs(3600),
    )
    .issue_claims(&claims)?;
    denied(
        &ledger.call(&["company", "list"], Some(&token))?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    ledger.enable()?;
    denied(
        &ledger.call(&["company", "list"], Some(&token))?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    Ok(())
}

#[test]
fn forced_initialization_cannot_erase_an_enforced_ledger_at_either_destination_flag() -> TestResult
{
    let ledger = Ledger::new()?;
    ledger.enable()?;
    let before = ledger.count("audit_log")?;
    denied(
        &ledger.call(&["init", "--force"], None)?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    let destination = ledger.path.to_str().ok_or("path encoding")?;
    denied(
        &Command::cargo_bin("bk")?
            .args([
                "--json",
                "--db",
                "/nonexistent/unused.db",
                "--passphrase-file",
            ])
            .arg(&ledger.passphrase_file)
            .args(["init", "--path", destination, "--force"])
            .env_remove("BK_AGENT_TOKEN")
            .env_remove("BEANKEEPER_PASSPHRASE_CMD")
            .output()?,
        "UNAUTHORIZED",
        6,
        None,
    )?;
    assert_eq!(ledger.count("audit_log")?, before);
    assert!(db::policy::get(ledger.db.read())?.enforce);
    Ok(())
}

#[test]
fn policy_changes_are_atomic_and_raw_mutation_or_missing_state_fails_closed() -> TestResult {
    let ledger = Ledger::new()?;
    let count = ledger.count("audit_log")?;
    for sql in [
        "DELETE FROM ledger_policy",
        "UPDATE ledger_policy SET enforce = 1",
        "INSERT OR REPLACE INTO ledger_policy VALUES (1, 0, 'tampered')",
    ] {
        assert!(ledger.raw()?.execute(sql, []).is_err());
    }
    ledger.raw()?.execute_batch("CREATE TRIGGER fail_policy_update BEFORE UPDATE ON ledger_policy BEGIN SELECT RAISE(ABORT, 'simulated failure'); END;")?;
    assert!(
        Granted::<Admin>::with_db(&ledger.db, &Actor::new("owner".parse()?), |grant| {
            db::audit::set_policy(grant, true)
        })
        .is_err()
    );
    assert!(!db::policy::get(ledger.db.read())?.enforce);
    assert_eq!(ledger.count("audit_log")?, count);
    ledger
        .raw()?
        .execute_batch("DROP TRIGGER fail_policy_update;")?;
    let admin = ledger.issue(&["ledger/admin"])?;
    ledger.enable()?;
    let before = ledger.count("audit_log")?;
    assert!(matches!(
        Granted::<Admin>::with_db(&ledger.db, &Actor::new("owner".parse()?), |grant| {
            db::audit::set_policy(grant, false)
        }),
        Err(beankeeper_cli::error::CliError::Unauthorized(_))
    ));
    assert!(db::policy::get(ledger.db.read())?.enforce);
    assert_eq!(ledger.count("audit_log")?, before);
    ledger.run(&["policy", "set", "--enforce", "on"], Some(&admin))?;
    assert_eq!(ledger.count("audit_log")?, count + 2); // one policy change and one token issuance
    ledger.raw()?.execute_batch(
        "DROP TRIGGER protect_policy_update; UPDATE ledger_policy SET enforce = 0;",
    )?;
    assert!(db::policy::get(ledger.db.read()).is_err());
    ledger
        .raw()?
        .execute_batch("DROP TRIGGER protect_policy_delete; DELETE FROM ledger_policy;")?;
    assert!(db::policy::get(ledger.db.read()).is_err());
    denied(
        &ledger.call(&["company", "list"], None)?,
        "DATABASE",
        4,
        None,
    )?;
    Ok(())
}

#[test]
fn signed_grants_ignore_registry_edits_and_descendants_do_not_grant_parent_operations() -> TestResult
{
    let ledger = Ledger::new()?;
    let read = ledger.issue(&["ledger/read"])?;
    let narrow = ledger.issue(&["ledger/post/example"])?;
    ledger.enable()?;
    // The registry stores issuance metadata, but grants come only from signed claims.
    ledger.raw()?.execute(
        "UPDATE agent_tokens SET capabilities = '[\"ledger/post\",\"ledger/admin\"]'",
        [],
    )?;
    denied(
        &ledger.call(&posting(), Some(&read))?,
        "FORBIDDEN",
        7,
        Some("ledger/post"),
    )?;
    denied(
        &ledger.call(&posting(), Some(&narrow))?,
        "FORBIDDEN",
        7,
        Some("ledger/post"),
    )?;
    ledger.run(&["company", "list"], Some(&read))?;
    assert_eq!(ledger.count("transactions")?, 0);
    Ok(())
}

#[test]
fn snapshots_require_admin_and_preserve_enforcement_on_restore() -> TestResult {
    let ledger = Ledger::new()?;
    let read = ledger.issue(&["ledger/read"])?;
    let admin = ledger.issue(&["ledger/admin"])?;
    ledger.enable()?;
    let snapshot = ledger.dir.path().join("snapshot.db");
    let output = snapshot.to_str().ok_or("snapshot path")?;
    for (token, code, exit, capability) in [
        (None, "UNAUTHORIZED", 6, None),
        (Some(read.as_str()), "FORBIDDEN", 7, Some("ledger/admin")),
    ] {
        denied(
            &ledger.call(&["backup", "--output", output], token)?,
            code,
            exit,
            capability,
        )?;
        assert!(!snapshot.exists());
    }
    ledger.run(&["backup", "--output", output], Some(&admin))?;
    let destination = ledger.dir.path().join("restored.db");
    for (token, code, exit, capability) in [
        (None, "UNAUTHORIZED", 6, None),
        (Some(read.as_str()), "FORBIDDEN", 7, Some("ledger/admin")),
    ] {
        let mut command = ledger.command_at(&destination)?;
        if let Some(token) = token {
            command.args(["--agent-token", token]);
        }
        denied(
            &command.args(["restore", "--input", output]).output()?,
            code,
            exit,
            capability,
        )?;
        assert!(!destination.exists());
    }
    let restored = ledger
        .command_at(&destination)?
        .args(["--agent-token", &admin, "restore", "--input", output])
        .output()?;
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let passphrase: SecretString = "capability-test-passphrase".into();
    let db = Db::open_existing(&destination, Some(&passphrase))?;
    assert!(db::policy::get(db.read())?.enforce);
    assert!(
        identity::authorize(
            db.read(),
            Some(&admin),
            beankeeper_cli::capability::Capability::Admin
        )
        .is_ok()
    );
    Ok(())
}
