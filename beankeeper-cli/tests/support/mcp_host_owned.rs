use std::{
    future::{Future as _, poll_fn},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use beankeeper_cli::db::LedgerWriteKind;
use beankeeper_cli::identity::{IssuedCredential, WriteMode};
use beankeeper_cli::mcp::{
    LEDGER_TARGET_HEADER, LEDGER_TARGET_META_KEY, LedgerTarget, LedgerTargets, NamedLedgerTarget,
    http_router_with_ledger_targets,
};
use secrecy::ExposeSecret as _;

use super::*;

#[test]
fn host_write_authorization_follows_the_ledger_on_each_request() -> TestResult {
    const REASON: &str = "This ledger needs an active Desktop license.";
    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    let backup = demo.dir.path().join("demo.bk-backup");
    db::VerifiedSnapshot::create(
        &demo.path,
        Some(&SecretString::from(PASSPHRASE)),
        &backup,
        db::SnapshotOverwrite::Refuse,
    )?;
    let read_only = Arc::new(AtomicBool::new(true));
    let authorizer = {
        let read_only = read_only.clone();
        move |target: &LedgerTarget, kind| {
            if target.as_ref() == "demo"
                && kind != LedgerWriteKind::Restore
                && read_only.load(Ordering::Relaxed)
            {
                Err(REASON.into())
            } else {
                Ok(())
            }
        }
    };
    let targets = LedgerTargets::new(
        startup
            .db()?
            .with_write_authorizer(LedgerTarget::default(), authorizer.clone()),
    );
    // Registration supplies the canonical name seen by the policy.
    let demo_db = demo
        .db()?
        .with_write_authorizer(LedgerTarget::default(), authorizer);
    targets.register_host_owned("demo".parse::<NamedLedgerTarget>()?.into(), demo_db)?;

    exercise_router_with_check(
        &startup,
        false,
        |cli, config| {
            Ok(http_router_with_ledger_targets(
                cli,
                targets,
                false,
                config,
                TestHost {
                    calls: Arc::default(),
                },
            )?)
        },
        |client, endpoint| {
            let id = initialize_embedded_router(client, endpoint)?;
            let session = TargetSession {
                client,
                endpoint,
                id: &id,
            };
            let writer = Some(token(&startup.writer)?);
            let post = || session.call("txn_post", &posting("per-ledger-policy"), writer);
            let rejected = post()?;
            assert_eq!(rejected["result"]["isError"], true);
            assert_eq!(rejected["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
            let error = payload(&rejected)?;
            assert_eq!(error["code"], "LEDGER_WRITE_DENIED");
            assert_eq!(
                error["details"],
                json!({"ledger_target": "demo", "reason": REASON})
            );

            let rows = session.call("txn_list", &json!({"company":"acme"}), writer)?;
            assert_eq!(payload(&rows)?, json!([]));

            let admin = Some(token(&startup.admin)?);
            let restore = session.call("restore", &json!({"input": backup}), admin)?;
            assert_ne!(restore["result"]["isError"], true, "{restore}");
            let undo = payload(&restore)?["undo_id"].clone();
            let undone = session.call("restore_undo", &json!({"undo_id": undo}), admin)?;
            assert_eq!(payload(&undone)?["undo_of"], undo);
            assert_eq!(payload(&post()?)?["code"], "LEDGER_WRITE_DENIED");

            read_only.store(false, Ordering::Relaxed);
            assert_eq!(payload(&post()?)?["created"], true);
            read_only.store(true, Ordering::Relaxed);
            let rejected = session.call("txn_post", &posting("second-denied-write"), writer)?;
            assert_eq!(payload(&rejected)?["code"], "LEDGER_WRITE_DENIED");
            Ok(())
        },
    )
}

#[test]
fn host_owned_target_preserves_startup_grants_and_attribution() -> TestResult {
    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    let independent = Ledger::new()?;
    let targets = LedgerTargets::new(startup.db()?);
    let proposer = issue_proposer(&startup)?;
    exercise_router_with_check(
        &startup,
        false,
        |cli, config| {
            let host = TestHost {
                calls: std::sync::Arc::default(),
            };
            Ok(http_router_with_ledger_targets(
                cli,
                targets.clone(),
                false,
                config,
                host,
            )?)
        },
        |client, endpoint| {
            let id = initialize_embedded_router(client, endpoint)?;
            let session = TargetSession {
                client,
                endpoint,
                id: &id,
            };
            let writer = token(&startup.writer)?;
            let list = |target, bearer| {
                session
                    .request("tools/list", json!({}), bearer)
                    .header(LEDGER_TARGET_HEADER, target)
                    .send()
            };

            targets.register("independent".parse()?, independent.db()?)?;
            assert_eq!(list("independent", Some(writer))?.status(), 401);
            let target: NamedLedgerTarget = "demo".parse()?;
            targets.register_host_owned(target.into(), demo.db()?)?;
            let discovered = rpc(list("demo", Some(writer))?)?;
            assert_eq!(
                discovered["result"]["_meta"][LEDGER_TARGET_META_KEY],
                "demo"
            );
            assert_eq!(list("demo", None)?.status(), 401);
            assert_eq!(list("demo", Some(token(&demo.writer)?))?.status(), 401);

            // Delegation never expands a startup grant, even with destination policy off.
            let forbidden = session.call(
                "txn_post",
                &posting("out-of-grant"),
                Some(token(&startup.reader)?),
            )?;
            assert_eq!(payload(&forbidden)?["code"], "FORBIDDEN");
            assert_eq!(forbidden["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
            demo.enforce()?;
            verify_delegated_posting(&session, &startup, &demo, &proposer)?;
            revoke_admitted_post(&session, &startup, &demo, &proposer)?;
            assert_eq!(
                list("demo", Some(proposer.token().expose_secret()))?.status(),
                401
            );
            assert_eq!(list("demo", Some(writer))?.status(), 200);
            Ok(())
        },
    )
}

fn revoke_admitted_post(
    session: &TargetSession<'_>,
    startup: &Ledger,
    demo: &Ledger,
    proposer: &IssuedCredential,
) -> TestResult {
    let blocker = block_writes(demo)?;
    let pending = start_post(
        session,
        proposer.token().expose_secret(),
        "revoked-before-write",
    )?;
    startup.revoke(proposer.token_jti.as_str())?;
    drop(blocker);
    let rejected = rpc(pending)?;
    assert_eq!(payload(&rejected)?["code"], "UNAUTHORIZED");
    assert_eq!(rejected["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
    Ok(())
}

fn issue_proposer(ledger: &Ledger) -> TestResult<IssuedCredential> {
    Ok(Granted::<Admin>::with_db(
        &ledger.db()?,
        &ledger.owner,
        |grant| {
            identity::issue_credential_with_mode(
                grant,
                &IssueRequest {
                    name: "startup_proposer",
                    capabilities: &["ledger/read".into(), "ledger/post".into()],
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
                WriteMode::ProposeOnly,
            )
        },
    )?)
}

fn verify_delegated_posting(
    session: &TargetSession<'_>,
    startup: &Ledger,
    demo: &Ledger,
    proposer: &IssuedCredential,
) -> TestResult {
    let writer = token(&startup.writer)?;
    let posted = session.call("txn_post", &posting("delegated-direct"), Some(writer))?;
    assert_eq!(posted["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
    assert_eq!(payload(&posted)?["created"], true);
    let pending = payload(&session.call(
        "txn_post",
        &posting("delegated-pending"),
        Some(proposer.token().expose_secret()),
    )?)?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["proposer_uri"], proposer.agent_uri.as_str());

    let db = demo.db()?;
    let (uri, jti) = db.read().query_row(
        "SELECT agent_uri, token_jti FROM transactions WHERE reference = 'delegated-direct'",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    assert_eq!(uri, startup.writer["agent_uri"]);
    assert_eq!(jti, startup.writer["token_jti"]);
    let audit = session.call("audit_log", &json!({"company":"acme"}), Some(writer))?;
    assert!(
        payload(&audit)?
            .as_array()
            .ok_or("missing audit rows")?
            .iter()
            .any(|row| {
                row["agent_uri"] == startup.writer["agent_uri"]
                    && row["token_jti"] == startup.writer["token_jti"]
            })
    );
    let demo_rows = session.call("txn_list", &json!({"company":"acme"}), Some(writer))?;
    assert_eq!(demo_rows["result"]["_meta"]["pagination"]["returned"], 1);
    assert_eq!(demo_rows["result"]["_meta"][LEDGER_TARGET_META_KEY], "demo");
    let startup_rows = rpc(session
        .request(
            "tools/call",
            json!({"name":"txn_list","arguments":{"company":"acme"}}),
            Some(writer),
        )
        .send()?)?;
    assert_eq!(payload(&startup_rows)?, json!([]));
    assert!(
        startup_rows["result"]["_meta"]
            .get(LEDGER_TARGET_META_KEY)
            .is_none()
    );
    Ok(())
}

#[test]
fn detach_waits_for_an_admitted_http_call_and_keeps_the_session() -> TestResult {
    let startup = Ledger::new()?;
    let demo = Ledger::new()?;
    let targets = LedgerTargets::new(startup.db()?);
    let target: NamedLedgerTarget = "demo".parse()?;
    targets.register_host_owned(target.clone().into(), demo.db()?)?;
    exercise_router_with_check(
        &startup,
        false,
        |cli, config| {
            let host = TestHost {
                calls: std::sync::Arc::default(),
            };
            Ok(http_router_with_ledger_targets(
                cli,
                targets.clone(),
                false,
                config,
                host,
            )?)
        },
        |client, endpoint| {
            let id = initialize_embedded_router(client, endpoint)?;
            let session = TargetSession {
                client,
                endpoint,
                id: &id,
            };
            detach_during_post(&targets, &target, &session, &startup, &demo)
        },
    )
}

fn detach_during_post(
    targets: &LedgerTargets,
    target: &NamedLedgerTarget,
    session: &TargetSession<'_>,
    startup: &Ledger,
    demo: &Ledger,
) -> TestResult {
    let writer = token(&startup.writer)?;
    // Hold a separate SQLite writer until the test observes routing stop.
    let blocker = block_writes(demo)?;
    let in_flight = start_post(session, writer, "during-drain")?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let detach = targets.detach(target);
    tokio::pin!(detach);
    assert!(runtime.block_on(poll_fn(|cx| Poll::Ready(
        detach.as_mut().poll(cx).is_pending()
    ))));
    let discover = || session.request("tools/list", json!({}), Some(writer));
    assert_eq!(
        discover()
            .header(LEDGER_TARGET_HEADER, target.as_ref())
            .send()?
            .status(),
        404
    );
    assert_eq!(discover().send()?.status(), 200);
    drop(blocker);
    assert_eq!(payload(&rpc(in_flight)?)?["created"], true);

    let detached = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), detach).await })??;
    assert_eq!(detached.target(), target);
    let released = detached.path().ok_or("missing detached path")?;
    assert_eq!(released.canonicalize()?, demo.path.canonicalize()?);
    std::fs::remove_file(released)?;
    assert!(startup.path.exists());
    assert_eq!(discover().send()?.status(), 200);
    assert!(matches!(
        runtime.block_on(targets.detach(target)),
        Err(beankeeper_cli::error::CliError::NotFound(_))
    ));
    assert!(
        targets
            .register_host_owned(target.clone().into(), Db::open_in_memory()?)
            .is_err()
    );
    Ok(())
}

fn block_writes(ledger: &Ledger) -> TestResult<rusqlite::Connection> {
    let blocker = rusqlite::Connection::open(&ledger.path)?;
    blocker.pragma_update(None, "key", PASSPHRASE)?;
    blocker.execute_batch("BEGIN IMMEDIATE")?;
    Ok(blocker)
}

fn start_post(session: &TargetSession<'_>, bearer: &str, reference: &str) -> TestResult<Response> {
    let response = session
        .request("tools/call", json!({}), Some(bearer))
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name":"txn_post","arguments":posting(reference)}
        }))
        .header(LEDGER_TARGET_HEADER, "demo")
        .send()?;
    assert_eq!(response.status(), 200);
    Ok(response)
}
