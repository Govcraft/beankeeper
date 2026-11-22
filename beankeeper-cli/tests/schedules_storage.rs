//! Schedule validation, atomic posting and audit preservation.

use beankeeper::{core::Frequency, types::Currency};
use beankeeper_cli::{
    db::{
        Actor, CreateAccountParams, Db, PostEntryParams, accounts, audit, companies,
        schedules::{self, CreateScheduleParams, DEFAULT_RUN_LIMIT, ScheduleState},
    },
    error::CliError,
};
use chrono::NaiveDate;

type TestResult = Result<(), CliError>;

fn date(value: &str) -> Result<NaiveDate, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("{error}")))
}

fn audit_count(db: &Db) -> rusqlite::Result<i64> {
    db.conn()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
}

fn fixture() -> Result<Db, CliError> {
    let db = Db::open_in_memory()?;
    companies::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        accounts::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code,
                name: code,
                account_type: kind,
                default_tax_category: Some("default-tax"),
            },
        )?;
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
            amount: 125_000,
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
    invalid.entries[0].amount = 1;
    cases.push(invalid);
    let mut invalid = template()?;
    invalid.entries[0].amount = 0;
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
    invalid.entries[0].amount = i64::MAX;
    cases.push(invalid);
    for invalid in cases {
        assert!(schedules::create_schedule(db.conn(), &actor, "acme", &invalid).is_err());
    }
    assert!(schedules::list_schedules(db.read(), "acme")?.is_empty());
    assert_eq!(audit_count(&db)?, before_audit);
    Ok(())
}

#[test]
fn due_posts_preserve_template_complete_at_end_and_are_idempotent() -> TestResult {
    let db = fixture()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-test");
    let schedule = schedules::create_schedule(db.conn(), &actor, "acme", &template()?)?;
    assert_eq!(
        schedule.entries[0].tax_category.as_deref(),
        Some("default-tax")
    );
    let preview = schedules::run_due(db.conn(), &actor, "acme", date("2026-03-31")?, true)?;
    assert_eq!(preview.len(), 3);
    assert!(
        preview
            .iter()
            .all(|occurrence| occurrence.transaction_id.is_none())
    );
    assert_eq!(
        schedules::list_schedules(db.read(), "acme")?[0]
            .state
            .next_due(),
        Some(date("2026-01-31")?)
    );
    assert_eq!(audit_count(&db)?, before_audit + 1);
    let posted = schedules::run_due(db.conn(), &actor, "acme", date("2026-03-31")?, false)?;
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
    let completed = &schedules::list_schedules(db.read(), "acme")?[0];
    assert_eq!(completed.state, ScheduleState::Completed);
    assert_eq!(completed.state.next_due(), None);
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2027-01-01")?, false)?.is_empty());
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.conn())?;
    assert!(
        audit::delete_account(
            db.conn(),
            &actor,
            "acme",
            "1000",
            beankeeper_cli::confirmation::Confirmed::forced(true).expect("forced confirmation")
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn schedule_audit_failure_rolls_back_postings_and_progress() -> TestResult {
    let db = fixture()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-test");
    schedules::create_schedule(db.conn(), &actor, "acme", &template()?)?;
    db.conn().execute_batch(
        "CREATE TRIGGER fail_schedule_audit BEFORE INSERT ON audit_log
         WHEN NEW.action = 'schedule_run' BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;",
    )?;
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(db.read(), "acme")?[0]
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
    let db = fixture()?;
    let actor = Actor::new("schedule-test");
    schedules::create_schedule(db.conn(), &actor, "acme", &template()?)?;
    db.conn().execute_batch(
        "CREATE TRIGGER fail_second_post BEFORE INSERT ON transactions
         WHEN NEW.date = '2026-02-28' BEGIN SELECT RAISE(ABORT, 'posting unavailable'); END;",
    )?;
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(db.read(), "acme")?[0]
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
    companies::create_company(db.conn(), &Actor::new("fixture"), "other", "Other", None)?;
    let schedule = schedules::create_schedule(db.conn(), &actor, "acme", &template()?)?;
    assert!(schedules::list_schedules(db.read(), "other")?.is_empty());
    assert!(schedules::cancel_schedule(db.conn(), &actor, "other", &schedule.id).is_err());
    let posted = schedules::run_due(db.conn(), &actor, "acme", date("2026-01-31")?, false)?;
    assert_eq!(posted.len(), 1);
    let cancelled = schedules::cancel_schedule(db.conn(), &actor, "acme", &schedule.id)?;
    assert_eq!(cancelled.state, ScheduleState::Cancelled);
    assert_eq!(cancelled.state.next_due(), None);
    schedules::cancel_schedule(db.conn(), &actor, "acme", &schedule.id)?;
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2027-01-01")?, false)?.is_empty());
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    let count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'schedule_cancel'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    assert!(
        audit::delete_company(
            db.conn(),
            &actor,
            "acme",
            beankeeper_cli::confirmation::Confirmed::forced(true).expect("forced confirmation")
        )
        .is_err()
    );
    audit::verify_hash_chain(db.read())?;
    Ok(())
}

#[test]
fn invalid_persisted_template_is_rejected_before_posting() -> TestResult {
    let db = fixture()?;
    let actor = Actor::new("schedule-test");
    schedules::create_schedule(db.conn(), &actor, "acme", &template()?)?;
    db.conn().execute(
        "UPDATE schedule_entries SET amount = 1 WHERE direction = 'debit'",
        [],
    )?;
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2026-03-31")?, false).is_err());
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    assert_eq!(
        schedules::list_schedules(db.read(), "acme")?[0]
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
    companies::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        accounts::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "acme",
                code,
                name: code,
                account_type: kind,
                default_tax_category: None,
            },
        )?;
    }
    schedules::create_schedule(db.conn(), &Actor::new("creator"), "acme", &template()?)?;
    drop(db);
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || -> Result<usize, CliError> {
                let db = Db::open_existing(&path, None)?;
                db.conn().busy_timeout(std::time::Duration::from_secs(10))?;
                barrier.wait();
                Ok(schedules::run_due(
                    db.conn(),
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
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?;
    assert_eq!(count, 3);
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.conn())?;
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
        schedules::create_schedule(db.conn(), &actor, "acme", &params)?;
        let posted = schedules::run_due(db.conn(), &actor, "acme", NaiveDate::MAX, false)?;
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].date, start_date);
        let completed = &schedules::list_schedules(db.read(), "acme")?[0];
        assert_eq!(completed.state, ScheduleState::Completed);
        assert_eq!(completed.state.next_due(), None);
        assert!(schedules::run_due(db.conn(), &actor, "acme", NaiveDate::MAX, false)?.is_empty());
        audit::verify_hash_chain(db.read())?;
        audit::verify_creations(db.conn())?;
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
    schedules::create_schedule(db.conn(), &actor, "acme", &daily_template(1_000)?)?;
    let posted = schedules::run_due(db.conn(), &actor, "acme", date("2026-01-01")?, false)?;
    assert_eq!(posted.len(), DEFAULT_RUN_LIMIT);
    assert!(posted.iter().all(|row| row.transaction_id.is_some()));
    assert_eq!(
        schedules::list_schedules(db.read(), "acme")?[0]
            .state
            .code(),
        "completed"
    );
    assert!(schedules::run_due(db.conn(), &actor, "acme", date("2026-01-01")?, false)?.is_empty());
    audit::verify_hash_chain(db.read())?;
    audit::verify_creations(db.conn())?;
    Ok(())
}

#[test]
fn run_limit_is_company_wide_and_checked_before_any_posting() -> TestResult {
    let db = fixture()?;
    let before_audit = audit_count(&db)?;
    let actor = Actor::new("schedule-limit-test");
    for count in [500, 501] {
        schedules::create_schedule(db.conn(), &actor, "acme", &daily_template(count)?)?;
    }
    let before = serde_json::json!(schedules::list_schedules(db.read(), "acme")?);
    db.conn().execute_batch(
        "CREATE TRIGGER reject_any_post BEFORE INSERT ON transactions
         BEGIN SELECT RAISE(ABORT, 'posting attempted before limit check'); END;",
    )?;
    let Err(error) = schedules::run_due(db.conn(), &actor, "acme", date("2026-01-01")?, false)
    else {
        panic!("1001 occurrences must exceed the cap");
    };
    assert!(matches!(error, CliError::Validation(_)));
    assert!(error.to_string().contains("1000 occurrences"));
    assert!(error.to_string().contains("--allow-large-run"));
    assert_eq!(
        serde_json::json!(schedules::list_schedules(db.read(), "acme")?),
        before
    );
    for table in ["transactions", "entries"] {
        let count: i64 =
            db.conn()
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
    companies::create_company(db.conn(), &Actor::new("fixture"), "other", "Other", None)?;
    for (code, kind) in [
        ("1000", beankeeper::types::AccountType::Asset),
        ("5000", beankeeper::types::AccountType::Expense),
    ] {
        accounts::create_account(
            db.conn(),
            &Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: "other",
                code,
                name: code,
                account_type: kind,
                default_tax_category: None,
            },
        )?;
    }
    let params = daily_template(1_001)?;
    schedules::create_schedule(db.conn(), &actor, "other", &params)?;
    let cancelled = schedules::create_schedule(db.conn(), &actor, "acme", &params)?;
    schedules::cancel_schedule(db.conn(), &actor, "acme", &cancelled.id)?;
    let mut future = daily_template(1_001)?;
    future.start_date = date("2030-01-01")?;
    future.end_date = None;
    schedules::create_schedule(db.conn(), &actor, "acme", &future)?;
    schedules::create_schedule(db.conn(), &actor, "acme", &params)?;
    let posted = schedules::run_due(db.conn(), &actor, "acme", date("2020-01-01")?, false)?;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        schedules::list_schedules(db.read(), "other")?[0]
            .state
            .next_due(),
        Some(params.start_date)
    );
    let remaining = schedules::run_due(db.conn(), &actor, "acme", date("2026-01-01")?, true)?;
    assert_eq!(remaining.len(), DEFAULT_RUN_LIMIT);
    audit::verify_hash_chain(db.read())?;
    Ok(())
}
