use beankeeper_cli::{
    cli::Cli,
    commands,
    db::{self, Actor, Db},
    error::CliError,
    identity::{Admin, Granted},
    ledger_target::LedgerTarget,
    output::json::EnvelopeError,
};
use clap::Parser as _;
use serde_json::json;

#[test]
fn embedded_cli_and_library_writes_use_the_host_policy() -> Result<(), CliError> {
    const REASON: &str = "These books are read-only in Desktop.";
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("personal.db");
    let bind = |db: Db| -> Result<Db, CliError> {
        Ok(db.with_write_authorizer(
            "personal".parse()?,
            |_: &LedgerTarget, _| Err(REASON.into()),
        ))
    };

    let mut cli = Cli::parse_from(["bk", "company", "create", "personal", "Personal"]);
    let result = commands::dispatch_with_db(&mut cli, bind(Db::open(&path, None)?)?, None);
    let Err(error @ CliError::WriteDenied { .. }) = result else {
        panic!("expected a typed write denial, got {result:?}");
    };
    assert_eq!(error.exit_code(), 7);
    let envelope = EnvelopeError::from(&error);
    assert_eq!(envelope.code.as_str(), "LEDGER_WRITE_DENIED");
    assert_eq!(
        envelope.details,
        Some(json!({"ledger_target": "personal", "reason": REASON}))
    );

    let db = bind(Db::open_existing(&path, None)?)?;
    let result = Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), |_| {
        panic!("a denied write must never reach the mutation callback")
    });
    assert!(matches!(result, Err::<(), _>(CliError::WriteDenied { .. })));
    assert!(!db::company_exists(db.read(), &"personal".parse()?)?);

    let mut cli = Cli::parse_from(["bk", "company", "list"]);
    commands::dispatch_with_db(&mut cli, db, None)?;
    Ok(())
}
