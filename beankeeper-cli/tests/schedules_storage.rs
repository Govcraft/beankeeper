//! Schedule validation, atomic posting and audit preservation.

#[path = "support/raw_ledger.rs"]
mod raw_ledger;

use beankeeper::{core::Frequency, types::Currency};
use beankeeper_cli::{
    db::{
        Actor, CreateAccountParams, Db, PostEntryParams, accounts, audit, companies,
        schedules::{
            self, CreateScheduleParams, DEFAULT_RUN_LIMIT, Occurrence, Schedule, ScheduleId,
            ScheduleState,
        },
    },
    error::CliError,
    identity::{Admin, Granted, Post},
};
use chrono::NaiveDate;

type TestResult = Result<(), CliError>;

fn date(value: &str) -> Result<NaiveDate, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("{error}")))
}

fn create_schedule(
    db: &Db,
    actor: &Actor,
    company: &str,
    params: &CreateScheduleParams,
) -> Result<Schedule, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        schedules::create_schedule(grant, &company.parse()?, params)
    })
}

fn cancel_schedule(
    db: &Db,
    actor: &Actor,
    company: &str,
    id: &ScheduleId,
) -> Result<Schedule, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        schedules::cancel_schedule(grant, &company.parse()?, id)
    })
}

fn run_due(
    db: &Db,
    actor: &Actor,
    company: &str,
    through: NaiveDate,
    dry_run: bool,
) -> Result<Vec<Occurrence>, CliError> {
    if dry_run {
        db.read_snapshot(|reader| schedules::preview_due(reader, &company.parse()?, through))
    } else {
        Granted::<Post>::with_db(db, actor, |grant| {
            schedules::run_due(grant, &company.parse()?, through, false)
        })
    }
}

fn audit_count(db: &Db) -> rusqlite::Result<i64> {
    db.read()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
}

fn fixture() -> Result<Db, CliError> {
    seed(Db::open_in_memory()?)
}

fn fixture_with_raw() -> Result<(tempfile::TempDir, Db, rusqlite::Connection), CliError> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("fixture.db");
    let db = seed(Db::open(&path, None)?)?;
    let raw = raw_ledger::open(&path, None)?;
    Ok((directory, db, raw))
}

fn seed(db: Db) -> Result<Db, CliError> {
    Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
        companies::create_company(grant, &"acme".parse()?, "Acme", None)
    })?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
            accounts::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code,
                    name: code,
                    account_type: kind,
                    default_tax_category: Some("default-tax"),
                },
            )
        })?;
    }
    Ok(db)
}

fn template() -> Result<CreateScheduleParams, CliError> {
    Ok(CreateScheduleParams {
        description: "Monthly rent".into(),
        currency: Currency::USD,
        start_date: date("2026-01-31")?,
        frequency: Frequency::Monthly,
        end_date: Some(date("2026-03-31")?),
        metadata: Some("{\"vendor\":\"landlord\"}".into()),
        entries: [
            ("5000", beankeeper::types::DebitOrCredit::Debit),
            ("1000", beankeeper::types::DebitOrCredit::Credit),
        ]
        .into_iter()
        .map(|(account, direction)| PostEntryParams {
            account_code: account.into(),
            direction,
            amount: 125_000_i64
                .try_into()
                .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
            memo: Some("rent memo".into()),
            tax_category: None,
        })
        .collect(),
    })
}

#[test]
fn rejects_invalid_templates_without_state_or_audit() -> TestResult {
    let db = fixture()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-test");
    let mut cases = Vec::new();
    let mut invalid = template()?;
    invalid.entries[0].amount = 1_i64.try_into()?;
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.entries[0].direction = beankeeper::types::DebitOrCredit::Credit;
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.entries[0].account_code = "missing".into();
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.description = "  ".into();
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.end_date = Some(date("2025-01-01")?);
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.entries.push(invalid.entries[0].clone());
    invalid.entries[0].amount = i64::MAX.try_into()?;
    cases.push(invalid);
    for invalid in cases {
        assert!(create_schedule(&db, &actor, "acme", &invalid).is_err());
    }
    assert!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?
        .is_empty()
    );
    assert_eq!(audit_count(&db)?, before_audit);
    Ok(())
}

#[test]
fn due_posts_preserve_template_complete_at_end_and_are_idempotent() -> TestResult {
    let db = fixture()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-test");
    let schedule = create_schedule(&db, &actor, "acme", &template()?)?;
    assert_eq!(
        schedule.entries[0].tax_category.as_deref(),
        Some("default-tax")
    );
    let preview = run_due(&db, &actor, "acme", date("2026-03-31")?, true)?;
    assert_eq!(preview.len(), 3);
    assert!(
        preview
            .iter()
            .all(|occurrence| occurrence.transaction_id.is_none())
    );
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?[0]
            .state
            .next_due(),
        Some(date("2026-01-31")?)
    );
    assert_eq!(audit_count(&db)?, before_audit + 1);
    let posted = run_due(&db, &actor, "acme", date("2026-03-31")?, false)?;
    assert_eq!(
        posted
            .iter()
            .map(|item| item.date.to_string())
            .collect::<Vec<_>>(),
        ["2026-01-31", "2026-02-28", "2026-03-31"]
    );
    assert!(
        posted
            .iter()
            .all(|occurrence| occurrence.transaction_id.is_some())
    );
    let completed = &schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
        db.read(),
        "acme".parse()?,
    )?)?[0];
    assert_eq!(completed.state, ScheduleState::Completed);
    assert_eq!(completed.state.next_due(), None);
    assert!(run_due(&db, &actor, "acme", date("2027-01-01")?, false)?.is_empty());
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.read())?;
    assert!(
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            audit::delete_account(
                grant,
                &"acme".parse()?,
                "1000",
                beankeeper_cli::confirmation::Confirmed::forced(true).expect("forced confirmation"),
            )
        })
        .is_err()
    );
    Ok(())
}

#[test]
fn schedule_audit_failure_rolls_back_postings_and_progress() -> TestResult {
    let (_directory, db, raw) = fixture_with_raw()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-test");
    create_schedule(&db, &actor, "acme", &template()?)?;
    raw.execute_batch(
        "CREATE TRIGGER fail_schedule_audit BEFORE INSERT ON audit_log
         WHEN NEW.action = 'schedule_run' BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;",
    )?;
    assert!(run_due(&db, &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?[0]
            .state
            .next_due(),
        Some(date("2026-01-31")?)
    );
    assert_eq!(audit_count(&db)?, before_audit + 1);
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn later_post_failure_rolls_back_earlier_occurrences() -> TestResult {
    let (_directory, db, raw) = fixture_with_raw()?;
    let actor = Actor::new("schedule-test");
    create_schedule(&db, &actor, "acme", &template()?)?;
    raw.execute_batch(
        "CREATE TRIGGER fail_second_post BEFORE INSERT ON transactions
         WHEN NEW.date = '2026-02-28' BEGIN SELECT RAISE(ABORT, 'posting unavailable'); END;",
    )?;
    assert!(run_due(&db, &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?[0]
            .state
            .next_due(),
        Some(date("2026-01-31")?)
    );
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn cancellation_is_scoped_audited_and_does_not_remove_postings() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("schedule-test");
    Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
        companies::create_company(grant, &"other".parse()?, "Other", None)
    })?;
    let schedule = create_schedule(&db, &actor, "acme", &template()?)?;
    assert!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "other".parse()?
        )?)?
        .is_empty()
    );
    assert!(cancel_schedule(&db, &actor, "other", &schedule.id).is_err());
    let posted = run_due(&db, &actor, "acme", date("2026-01-31")?, false)?;
    assert_eq!(posted.len(), 1);
    let cancelled = cancel_schedule(&db, &actor, "acme", &schedule.id)?;
    assert_eq!(cancelled.state, ScheduleState::Cancelled);
    assert_eq!(cancelled.state.next_due(), None);
    cancel_schedule(&db, &actor, "acme", &schedule.id)?;
    assert!(run_due(&db, &actor, "acme", date("2027-01-01")?, false)?.is_empty());
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    let count: i64 = db.read().query_row(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'schedule_cancel'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    assert!(
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            audit::delete_company(
                grant,
                &"acme".parse()?,
                beankeeper_cli::confirmation::Confirmed::forced(true).expect("forced confirmation"),
            )
        })
        .is_err()
    );
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn invalid_persisted_template_is_rejected_before_posting() -> TestResult {
    let (_directory, db, raw) = fixture_with_raw()?;
    let actor = Actor::new("schedule-test");
    create_schedule(&db, &actor, "acme", &template()?)?;
    raw.execute(
        "UPDATE schedule_entries SET amount = 1 WHERE direction = 'debit'",
        [],
    )?;
    assert!(run_due(&db, &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?[0]
            .state
            .next_due(),
        Some(date("2026-01-31")?)
    );
    Ok(())
}

#[test]
fn concurrent_runners_consume_each_occurrence_once() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{Arc, Barrier};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("concurrent.db");
    let db = Db::open(&path, None)?;
    Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
        companies::create_company(grant, &"acme".parse()?, "Acme", None)
    })?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
            accounts::create_account(
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
    create_schedule(&db, &Actor::new("creator"), "acme", &template()?)?;
    drop(db);

    let snapshot = Db::open_snapshot(&path, None)?;
    let preview = snapshot.read_snapshot(|reader| {
        schedules::preview_due(reader, &"acme".parse()?, date("2026-03-31")?)
    })?;
    assert_eq!(preview.len(), 3);
    assert!(
        preview
            .iter()
            .all(|occurrence| occurrence.transaction_id.is_none())
    );
    drop(snapshot);

    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || -> Result<usize, CliError> {
                let db = Db::open_existing(&path, None)?;
                barrier.wait();
                Ok(run_due(
                    &db,
                    &Actor::new(format!("runner-{index}")),
                    "acme",
                    date("2026-03-31")?,
                    false,
                )?
                .len())
            })
        })
        .collect::<Vec<_>>();
    let mut posted = 0;
    for handle in handles {
        posted += handle.join().map_err(|_| "runner panicked")??;
    }
    assert_eq!(posted, 3);
    let db = Db::open_existing(&path, None)?;
    let count: i64 = db
        .read()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 3);
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.read())?;
    Ok(())
}

#[test]
fn finite_schedules_complete_at_the_supported_date_boundary() -> TestResult {
    let preceding_day = NaiveDate::MAX
        .pred_opt()
        .ok_or_else(|| CliError::Validation("missing preceding date".into()))?;
    for (frequency, start_date) in [
        (Frequency::Weekly, preceding_day),
        (Frequency::Daily, NaiveDate::MAX),
    ] {
        let db = fixture()?;
        let actor = Actor::new("schedule-boundary-test");
        let mut params = template()?;
        params.start_date = start_date;
        params.end_date = Some(NaiveDate::MAX);
        params.frequency = frequency;
        create_schedule(&db, &actor, "acme", &params)?;
        let posted = run_due(&db, &actor, "acme", NaiveDate::MAX, false)?;
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].date, start_date);
        let completed = &schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?,
        )?)?[0];
        assert_eq!(completed.state, ScheduleState::Completed);
        assert_eq!(completed.state.next_due(), None);
        assert!(run_due(&db, &actor, "acme", NaiveDate::MAX, false)?.is_empty());
        audit::verify_hash_chain(db.read())?;
        audit::verify_creations(db.read())?;
    }
    Ok(())
}

fn daily_template(occurrences: u64) -> Result<CreateScheduleParams, CliError> {
    let mut params = template()?;
    params.start_date = date("2020-01-01")?;
    params.frequency = Frequency::Daily;
    params.end_date = params
        .start_date
        .checked_add_days(chrono::Days::new(occurrences - 1));
    Ok(params)
}

#[test]
fn run_limit_allows_exactly_one_thousand_occurrences() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("schedule-limit-test");
    create_schedule(&db, &actor, "acme", &daily_template(1_000)?)?;
    let posted = run_due(&db, &actor, "acme", date("2026-01-01")?, false)?;
    assert_eq!(posted.len(), DEFAULT_RUN_LIMIT);
    assert!(posted.iter().all(|row| row.transaction_id.is_some()));
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "acme".parse()?
        )?)?[0]
            .state
            .code(),
        "completed"
    );
    assert!(run_due(&db, &actor, "acme", date("2026-01-01")?, false)?.is_empty());
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.read())?;
    Ok(())
}

#[test]
fn run_limit_is_company_wide_and_checked_before_any_posting() -> TestResult {
    let (_directory, db, raw) = fixture_with_raw()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-limit-test");
    for count in [500, 501] {
        create_schedule(&db, &actor, "acme", &daily_template(count)?)?;
    }
    let before = serde_json::json!(schedules::list_schedules(
        &beankeeper_cli::db::companies::Company::bind(db.read(), "acme".parse()?)?
    )?);
    raw.execute_batch(
        "CREATE TRIGGER reject_any_post BEFORE INSERT ON transactions
         BEGIN SELECT RAISE(ABORT, 'posting attempted before limit check'); END;",
    )?;
    let Err(error) = run_due(&db, &actor, "acme", date("2026-01-01")?, false) else {
        panic!("1001 occurrences must exceed the cap");
    };
    assert!(matches!(error, CliError::Validation(_)));
    assert!(error.to_string().contains("1000 occurrences"));
    assert!(error.to_string().contains("--allow-large-run"));
    assert_eq!(
        serde_json::json!(schedules::list_schedules(
            &beankeeper_cli::db::companies::Company::bind(db.read(), "acme".parse()?)?
        )?),
        before
    );
    for table in ["transactions", "entries"] {
        let count: i64 =
            db.read()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        assert_eq!(count, 0);
    }
    assert_eq!(audit_count(&db)?, before_audit + 2);
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn run_limit_counts_only_pending_due_occurrences_in_the_selected_company() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("schedule-limit-test");
    Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
        companies::create_company(grant, &"other".parse()?, "Other", None)
    })?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        Granted::<Admin>::with_db(&db, &Actor::new("fixture"), |grant| {
            accounts::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"other".parse()?,
                    code,
                    name: code,
                    account_type: kind,
                    default_tax_category: None,
                },
            )
        })?;
    }
    let params = daily_template(1_001)?;
    create_schedule(&db, &actor, "other", &params)?;
    let cancelled = create_schedule(&db, &actor, "acme", &params)?;
    cancel_schedule(&db, &actor, "acme", &cancelled.id)?;
    let mut future = daily_template(1_001)?;
    future.start_date = date("2030-01-01")?;
    future.end_date = None;
    create_schedule(&db, &actor, "acme", &future)?;
    create_schedule(&db, &actor, "acme", &params)?;
    let posted = run_due(&db, &actor, "acme", date("2020-01-01")?, false)?;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        schedules::list_schedules(&beankeeper_cli::db::companies::Company::bind(
            db.read(),
            "other".parse()?
        )?)?[0]
            .state
            .next_due(),
        Some(params.start_date)
    );
    let remaining = run_due(&db, &actor, "acme", date("2026-01-01")?, true)?;
    assert_eq!(remaining.len(), DEFAULT_RUN_LIMIT);
    audit::verify_hash_chain(db.read())?;
    Ok(())
}
