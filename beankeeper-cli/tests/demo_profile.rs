//! Exercise the embedded and command-line boundaries for tailored demo books.

use std::{
    error::Error,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use beankeeper::types::Currency;
use beankeeper_cli::{
    commands::init::create_demo_ledger_from_profile,
    db::{self, Actor, Db, audit::AuditAction, companies::Company, verification::VerifiedLedger},
    demo::{CountryCode, DemoProfile},
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

const PROFILE: &str = r#"{
    "work_shape": "self_employed",
    "country": "JP",
    "currencies": {"secondary": ["BHD"]},
    "books": [
        {"kind": "personal", "accounts": [
            {"kind": "checking"}, {"kind": "savings"}, {"kind":"investment","currency":"BHD"}
        ]},
        {"kind": "llc", "industry": "retail", "accounts": [
            {"kind":"credit_card"}, {"kind":"payment_app","currency":"BHD"},
            {"kind":"loan"}, {"kind":"cash"}, {"kind":"credit_card"}
        ]}
    ],
    "features": {"include_investments": true}
}"#;

#[test]
fn profile_has_closed_choices_and_validates_relationships() -> TestResult {
    let profile = PROFILE.parse::<DemoProfile>()?;
    assert_eq!(profile.primary_currency(), Currency::JPY);
    assert_eq!(
        profile.currencies().collect::<Vec<_>>(),
        [Currency::JPY, Currency::BHD]
    );
    let copy: DemoProfile = serde_json::from_value(serde_json::to_value(&profile)?)?;
    assert_eq!(
        copy.currencies().collect::<Vec<_>>(),
        [Currency::JPY, Currency::BHD]
    );
    let default: DemoProfile = "{}".parse()?;
    assert_eq!(
        default.primary_currency(),
        DemoProfile::default().primary_currency()
    );
    assert!("ZZ".parse::<CountryCode>().is_err());

    for (input, message) in [
        (r#"{"books":[]}"#, "requires 1 to"),
        (
            r#"{"country":"JP","currencies":{"secondary":["JPY"]}}"#,
            "must be distinct",
        ),
        (
            r#"{"currencies":{"secondary":["EUR","EUR"]}}"#,
            "must be distinct",
        ),
        (
            r#"{"books":[{"accounts":[{"currency":"JPY"}]}]}"#,
            "must be listed",
        ),
    ] {
        assert!(
            input
                .parse::<DemoProfile>()
                .is_err_and(|error| error.to_string().contains(message))
        );
    }
    for input in [
        r#"{"work_shape":"employee","name":"MORNINGGLASS-PRIVATE-INPUT"}"#,
        r#"{"work_shape":"MORNINGGLASS-PRIVATE-INPUT"}"#,
        r#"{"books":[{"kind":"partnership"}]}"#,
        r#"{"books":[{"accounts":[{"kind":"mortgage"}]}]}"#,
        r#"{"currencies":{"primary":"XYZ"}}"#,
    ] {
        let Err(error) = input.parse::<DemoProfile>() else {
            return Err("profile must reject unknown fields and choices".into());
        };
        assert!(!error.to_string().contains("MORNINGGLASS-PRIVATE-INPUT"));
    }
    let schema = schemars::schema_for!(DemoProfile).to_value();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["books"]["minItems"], 1);
    Ok(())
}

#[test]
fn generated_books_have_synthetic_charts_currencies_and_linked_draws() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tailored.db");
    let profile = PROFILE.parse()?;
    let actor = Actor::new("demo-fixture-initializer".parse()?);
    let result = create_demo_ledger_from_profile(&path, None, &actor, &profile)?;
    assert_eq!(result.path(), path);
    let ledger = Db::open_existing(&path, None)?;
    VerifiedLedger::read(&ledger)?;

    let companies = db::list_companies(ledger.read())?;
    assert_eq!(companies.len(), 2);
    assert_eq!(companies[0].name, "Fictional Household 1");
    assert_eq!(companies[1].name, "Fictional Moonfern Studio 2 LLC");
    assert!(
        companies
            .iter()
            .all(|company| company.description.is_none())
    );
    let household = Company::bind(ledger.read(), companies[0].slug.clone())?;
    let business = Company::bind(ledger.read(), companies[1].slug.clone())?;

    let accounts = db::list_accounts(
        &business,
        &db::ListAccountParams {
            type_filter: None,
            name_filter: None,
        },
    )?;
    for name in [
        "Sample Credit Card 1 (JPY)",
        "Sample Credit Card 5 (JPY)",
        "Sample Payment App 2 (BHD)",
        "Sample Loan 3 (JPY)",
        "Sample Cash 4 (JPY)",
        "Merchandise Sales",
        "Store Supplies",
        "Owner Draws",
        "Sample Brokerage (JPY)",
    ] {
        assert!(
            accounts.iter().any(|account| account.name == name),
            "{name}"
        );
    }
    let holdings = db::investments::holdings(&ledger, household.slug(), "2025-03-31".parse()?)?;
    assert_eq!(holdings.len(), 1);
    assert_eq!(holdings[0].currency, Currency::BHD);
    assert_eq!(
        holdings[0]
            .market_value
            .map(|money| money.money().amount().minor_units()),
        Some(324_000)
    );
    assert_demo_transactions(&ledger, &household, &business)?;

    let seeds = db::list_audit(
        ledger.read(),
        &db::ListAuditParams {
            action: Some(AuditAction::DemoSeed),
            actor: None,
            agent_uri: None,
            company_slug: None,
            entity: None,
            limit: Default::default(),
            offset: Default::default(),
        },
    )?;
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].actor, "demo-fixture-initializer");
    assert_eq!(seeds[0].entity_id, "demo");
    assert!(seeds[0].company_slug.is_none());
    assert!(create_demo_ledger_from_profile(&path, None, &actor, &profile).is_err());
    VerifiedLedger::read(&Db::open_existing(&path, None)?)?;

    // Interview fields and values are absent even from the SQLite file bytes.
    drop(ledger);
    let contents = std::fs::read(path)?;
    for input in [
        b"work_shape".as_slice(),
        b"self_employed",
        b"include_investments",
        b"country",
    ] {
        assert!(!contents.windows(input.len()).any(|part| part == input));
    }
    assert_eq!(dir.path().read_dir()?.count(), 1);
    Ok(())
}

fn assert_demo_transactions(
    ledger: &Db,
    household: &Company<'_>,
    business: &Company<'_>,
) -> TestResult {
    let mut draws = Vec::new();
    for book in [household, business] {
        let transactions = db::list_transactions(book, &db::ListTransactionParams::default())?;
        assert!(transactions.iter().any(|row| row.currency == Currency::BHD));
        assert!(transactions.iter().any(|row| row.currency == Currency::JPY));
        for row in transactions {
            assert_eq!(row.posted_by.as_deref(), Some("demo"));
            let metadata: Value =
                serde_json::from_str(row.metadata.as_deref().ok_or("missing metadata")?)?;
            assert_eq!(metadata["beankeeper_source"], "demo");
            if row.description.contains("owner draw") {
                assert_eq!(row.currency, Currency::JPY);
                draws.push((row.id, metadata["correlate"].clone()));
            }
        }
    }
    assert_eq!(draws.len(), 2);
    assert_eq!(draws[0].1, serde_json::to_value(draws[1].0)?);
    assert_eq!(draws[1].1, serde_json::to_value(draws[0].0)?);
    assert!(db::find_orphaned_correlations(ledger.read(), db::companies::AllCompanies)?.is_empty());
    Ok(())
}

fn bk(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bk"));
    command.args(["--quiet", "--json", "--db"]).arg(path);
    for var in [
        "BK_ACTOR",
        "BK_AGENT_TOKEN",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_PASSPHRASE_CMD",
    ] {
        command.env_remove(var);
    }
    command
}

#[test]
fn cli_accepts_inline_file_and_stdin_profiles_at_fresh_paths() -> TestResult {
    let dir = tempfile::tempdir()?;
    let interview = dir.path().join("choices.json");
    std::fs::write(&interview, PROFILE)?;
    for (index, input) in [
        std::ffi::OsStr::new(PROFILE),
        interview.as_os_str(),
        "-".as_ref(),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("demo-{index}.db"));
        let mut command = bk(&path);
        command
            .args(["init", "--demo", "--profile"])
            .arg(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = if input == "-" {
            let mut child = command.stdin(Stdio::piped()).spawn()?;
            child
                .stdin
                .take()
                .ok_or("stdin was not piped")?
                .write_all(PROFILE.as_bytes())?;
            child.wait_with_output()?
        } else {
            command.output()?
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?["ok"], true);
        VerifiedLedger::read(&Db::open_existing(&path, None)?)?;
    }

    let path = dir.path().join("protected.db");
    std::fs::write(&path, b"keep this file intact")?;
    for opts in [
        &["init", "--demo", "--force", "--profile", PROFILE][..],
        &["init", "--profile", PROFILE],
        &["init", "--demo", "--profile", PROFILE],
    ] {
        assert!(!bk(&path).args(opts).output()?.status.success());
        assert_eq!(std::fs::read(&path)?, b"keep this file intact");
    }
    let invalid = dir.path().join("invalid.db");
    let output = bk(&invalid)
        .args(["init", "--demo", "--profile", r#"{"books":[]}"#])
        .output()?;
    assert!(!output.status.success());
    let result = serde_json::from_slice::<Value>(&output.stdout)?;
    assert_eq!(result["error"]["code"], "VALIDATION");
    assert!(!invalid.exists());
    Ok(())
}

#[test]
fn feature_toggles_keep_the_requested_accounts_without_generating_activity() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("without-optional-activity.db");
    let profile: DemoProfile = serde_json::from_value(json!({
        "books": [
            {"kind": "personal", "accounts": [{"kind":"investment"}]},
            {"kind": "corporation"}
        ],
        "features": {"include_owner_draws":false, "include_investments":false}
    }))?;
    let actor = Actor::new("demo-fixture-initializer".parse()?);
    create_demo_ledger_from_profile(&path, None, &actor, &profile)?;
    let ledger = Db::open_existing(&path, None)?;
    VerifiedLedger::read(&ledger)?;
    let household = Company::bind(ledger.read(), "demo-book-1".parse()?)?;
    assert!(db::investments::list_securities(&household)?.is_empty());
    let accounts = db::list_accounts(
        &household,
        &db::ListAccountParams {
            type_filter: None,
            name_filter: Some("Brokerage"),
        },
    )?;
    assert_eq!(accounts.len(), 1);
    for company in db::list_companies(ledger.read())? {
        let book = Company::bind(ledger.read(), company.slug)?;
        let rows = db::list_transactions(&book, &db::ListTransactionParams::default())?;
        assert_eq!(rows.len(), 7);
    }
    Ok(())
}
