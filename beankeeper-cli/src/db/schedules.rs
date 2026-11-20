//! Company-scoped future obligations kept separate from the posted ledger.

use std::{fmt, str::FromStr};

use beankeeper::{
    core::{Frequency, JournalEntry, RecurrenceError, occurrence_date},
    types::{Currency, Money},
};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::{Actor, PostEntryParams, PostResult, PostTransactionParams, audit, transactions};
use crate::error::CliError;

/// A globally unique scheduled-template identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleId(MagicTypeId);

impl ScheduleId {
    fn generate() -> Self {
        Self("sched".create_type_id::<V7>())
    }
}

impl fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ScheduleId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value)
            .map_err(|e| CliError::Validation(format!("invalid schedule ID: {e}")))?;
        if id.prefix_str().as_deref() != Ok("sched") {
            return Err(CliError::Validation(
                "schedule ID must have prefix 'sched'".into(),
            ));
        }
        Ok(Self(id))
    }
}

impl Serialize for ScheduleId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A validated template and its pending/completed/cancelled execution state.
#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub id: ScheduleId,
    pub company_slug: String,
    pub description: String,
    pub currency: String,
    pub start_date: NaiveDate,
    pub frequency: String,
    pub end_date: Option<NaiveDate>,
    pub next_due: Option<NaiveDate>,
    pub status: String,
    pub entries: Vec<PostEntryParams>,
    pub metadata: Option<String>,
    pub(crate) occurrence_index: u32,
}

/// Inputs for a future obligation, with entry amounts expressed in minor units.
pub struct CreateScheduleParams {
    pub description: String,
    pub currency: String,
    pub start_date: NaiveDate,
    pub frequency: Frequency,
    pub end_date: Option<NaiveDate>,
    pub entries: Vec<PostEntryParams>,
    pub metadata: Option<String>,
}

/// One due posting, or its preview when `transaction_id` is absent.
#[derive(Debug, Clone, Serialize)]
pub struct Occurrence {
    pub schedule_id: ScheduleId,
    pub date: NaiveDate,
    pub transaction_id: Option<i64>,
}

/// Create a balanced, validated template and an audit snapshot atomically.
///
/// # Errors
/// Rejects invalid descriptions, dates, currency, accounts or unbalanced entries.
pub fn create_schedule(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    params: &CreateScheduleParams,
) -> Result<Schedule, CliError> {
    audit::atomically(conn, || {
        super::companies::get_company(conn, company)?;
        validate_template(params)?;
        validate_posting(
            conn,
            company,
            params.start_date,
            &params.description,
            &params.currency,
            &params.entries,
        )?;
        let mut entries = params.entries.clone();
        for entry in &mut entries {
            let account = super::accounts::get_account(conn, company, &entry.account_code)?;
            if entry.tax_category.is_none() {
                entry.tax_category = account.default_tax_category;
            }
        }
        let schedule = Schedule {
            id: ScheduleId::generate(),
            company_slug: company.into(),
            description: params.description.clone(),
            currency: params.currency.clone(),
            start_date: params.start_date,
            frequency: params.frequency.to_string(),
            end_date: params.end_date,
            next_due: Some(params.start_date),
            status: "pending".into(),
            entries,
            metadata: params
                .metadata
                .as_deref()
                .and_then(super::metadata::normalize),
            occurrence_index: 0,
        };
        conn.execute(
            "INSERT INTO schedules (id, company_slug, description, currency, start_date, frequency,
             end_date, metadata, next_due, status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending')",
            params![
                schedule.id.to_string(),
                company,
                schedule.description,
                schedule.currency,
                schedule.start_date.to_string(),
                schedule.frequency,
                schedule.end_date.map(|d| d.to_string()),
                schedule.metadata,
                schedule.start_date.to_string()
            ],
        )?;
        for (position, entry) in schedule.entries.iter().enumerate() {
            let position = i64::try_from(position)
                .map_err(|_| CliError::Validation("too many schedule entries".into()))?;
            conn.execute(
                "INSERT INTO schedule_entries (schedule_id, company_slug, position, account_code,
                 direction, amount, memo, tax_category) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    schedule.id.to_string(),
                    company,
                    position,
                    entry.account_code,
                    entry.direction,
                    entry.amount,
                    entry.memo,
                    entry.tax_category
                ],
            )?;
        }
        audit::record_insert(
            conn,
            actor,
            company,
            "schedule",
            &schedule.id.to_string(),
            &serde_json::json!(&schedule),
        )?;
        Ok(schedule)
    })
}

fn validate_template(params: &CreateScheduleParams) -> Result<(), CliError> {
    if params.description.trim().is_empty() {
        return Err(CliError::Validation(
            "schedule description must not be empty".into(),
        ));
    }
    Currency::from_code(&params.currency).map_err(|e| CliError::Validation(e.to_string()))?;
    if params.end_date.is_some_and(|end| end < params.start_date) {
        return Err(CliError::Validation(
            "schedule end date must not precede start date".into(),
        ));
    }
    validate_entries(&params.entries)
}

fn validate_entries(entries: &[PostEntryParams]) -> Result<(), CliError> {
    let mut debit = 0_i64;
    let mut credit = 0_i64;
    for entry in entries {
        if entry.amount <= 0 {
            return Err(CliError::Validation(
                "schedule entry amounts must be positive".into(),
            ));
        }
        let total = match entry.direction.as_str() {
            "debit" => &mut debit,
            "credit" => &mut credit,
            _ => {
                return Err(CliError::Validation(
                    "schedule entry direction must be debit or credit".into(),
                ));
            }
        };
        *total = total
            .checked_add(entry.amount)
            .ok_or_else(|| CliError::Validation("schedule entry total overflow".into()))?;
    }
    if debit == 0 || debit != credit {
        return Err(CliError::Validation(
            "schedule entries must have equal nonzero debits and credits".into(),
        ));
    }
    Ok(())
}

fn validate_posting(
    conn: &Connection,
    company: &str,
    date: NaiveDate,
    description: &str,
    currency: &str,
    entries: &[PostEntryParams],
) -> Result<(), CliError> {
    validate_entries(entries)?;
    let currency =
        Currency::from_code(currency).map_err(|e| CliError::Validation(e.to_string()))?;
    let mut journal = JournalEntry::new(date, description);
    for entry in entries {
        let account = super::accounts::row_to_account(&super::accounts::get_account(
            conn,
            company,
            &entry.account_code,
        )?)?;
        let amount = Money::from_minor(i128::from(entry.amount), currency);
        journal = match entry.direction.as_str() {
            "debit" => journal.debit(&account, amount)?,
            "credit" => journal.credit(&account, amount)?,
            _ => {
                return Err(CliError::Validation(
                    "schedule entry direction must be debit or credit".into(),
                ));
            }
        };
    }
    journal.post()?;
    Ok(())
}

/// List all template states in next-due-date and ID order.
///
/// # Errors
/// Returns storage errors or errors decoding invalid stored dates or IDs.
pub fn list_schedules(conn: &Connection, company: &str) -> Result<Vec<Schedule>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM schedules WHERE company_slug = ?1 ORDER BY next_due IS NULL, next_due, id",
    )?;
    let ids = stmt
        .query_map([company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|id| get_schedule(conn, company, &id.parse()?))
        .collect()
}

pub(crate) fn get_schedule(
    conn: &Connection,
    company: &str,
    id: &ScheduleId,
) -> Result<Schedule, CliError> {
    let row = conn
        .query_row(
            "SELECT description, currency, start_date, frequency, end_date, next_due, status,
         metadata, occurrence_index FROM schedules WHERE company_slug = ?1 AND id = ?2",
            params![company, id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, u32>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            CliError::NotFound(format!("schedule '{id}' not found in company '{company}'"))
        })?;
    let mut stmt = conn.prepare(
        "SELECT account_code, direction, amount, memo, tax_category FROM schedule_entries
         WHERE schedule_id = ?1 AND company_slug = ?2 ORDER BY position",
    )?;
    let entries = stmt
        .query_map(params![id.to_string(), company], |row| {
            Ok(PostEntryParams {
                account_code: row.get(0)?,
                direction: row.get(1)?,
                amount: row.get(2)?,
                memo: row.get(3)?,
                tax_category: row.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let parse_date = |date: String| {
        date.parse::<NaiveDate>()
            .map_err(|e| CliError::Database(format!("invalid stored schedule date: {e}")))
    };
    Ok(Schedule {
        id: id.clone(),
        company_slug: company.into(),
        description: row.0,
        currency: row.1,
        start_date: parse_date(row.2)?,
        frequency: row.3,
        end_date: row.4.map(parse_date).transpose()?,
        next_due: row.5.map(parse_date).transpose()?,
        status: row.6,
        metadata: row.7,
        occurrence_index: row.8,
        entries,
    })
}

/// Cancel future occurrences while retaining the template and posted history.
///
/// # Errors
/// Returns not-found for another company's schedule, or storage/audit errors.
pub fn cancel_schedule(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &ScheduleId,
) -> Result<Schedule, CliError> {
    audit::atomically(conn, || {
        audit::lock_schedule_writer(conn)?;
        let before = get_schedule(conn, company, id)?;
        if before.status != "pending" {
            return Ok(before);
        }
        let mut after = before.clone();
        after.status = "cancelled".into();
        after.next_due = None;
        audit::change_schedule(conn, actor, &before, &after, "schedule_cancel")?;
        Ok(after)
    })
}

/// Post due occurrences through an inclusive date, or preview them without changes.
///
/// Acquires the writer before reading progress, preventing two runners from
/// consuming the same occurrence. All due postings and progress commit together.
///
/// # Errors
/// Returns validation, posting, recurrence-overflow or auditing errors; rolls back
/// all postings and template progress on failure.
pub fn run_due(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    through: NaiveDate,
    dry_run: bool,
) -> Result<Vec<Occurrence>, CliError> {
    audit::atomically(conn, || {
        if !dry_run {
            audit::lock_schedule_writer(conn)?;
        }
        let mut occurrences = Vec::new();
        for schedule in list_schedules(conn, company)? {
            if schedule.status == "pending" {
                execute_schedule(conn, actor, &schedule, through, dry_run, &mut occurrences)?;
            }
        }
        occurrences.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then_with(|| a.schedule_id.to_string().cmp(&b.schedule_id.to_string()))
        });
        Ok(occurrences)
    })
}

fn execute_schedule(
    conn: &Connection,
    actor: &Actor,
    before: &Schedule,
    through: NaiveDate,
    dry_run: bool,
    output: &mut Vec<Occurrence>,
) -> Result<(), CliError> {
    validate_posting(
        conn,
        &before.company_slug,
        before.start_date,
        &before.description,
        &before.currency,
        &before.entries,
    )?;
    let mut after = before.clone();
    let frequency = after
        .frequency
        .parse::<Frequency>()
        .map_err(|e| CliError::Database(e.to_string()))?;
    while let Some(date) = after.next_due.filter(|date| *date <= through) {
        let transaction_id = if dry_run {
            None
        } else {
            let reference = format!("schedule:{}:{date}", after.id);
            let date_text = date.to_string();
            let result = transactions::post_transaction(
                conn,
                &PostTransactionParams {
                    company_slug: &after.company_slug,
                    description: &after.description,
                    metadata: after.metadata.as_deref(),
                    currency: &after.currency,
                    date: &date_text,
                    entries: &after.entries,
                    correlate: None,
                    reference: Some(&reference),
                    on_conflict: super::ConflictStrategy::Error,
                    actor,
                },
            )?;
            Some(match result {
                PostResult::Created(id) | PostResult::Skipped(id) => id,
            })
        };
        output.push(Occurrence {
            schedule_id: after.id.clone(),
            date,
            transaction_id,
        });
        after.occurrence_index = after
            .occurrence_index
            .checked_add(1)
            .ok_or_else(|| CliError::Validation("schedule occurrence counter overflow".into()))?;
        after.next_due = match occurrence_date(after.start_date, frequency, after.occurrence_index)
        {
            Ok(next) => next.filter(|date| after.end_date.is_none_or(|end| *date <= end)),
            // An overflowing future occurrence is beyond every finite end date.
            Err(RecurrenceError::DateOverflow) if after.end_date.is_some() => None,
            Err(error) => return Err(CliError::Validation(error.to_string())),
        };
        if after.next_due.is_none() {
            after.status = "completed".into();
        }
    }
    if !dry_run && before.occurrence_index != after.occurrence_index {
        audit::change_schedule(conn, actor, before, &after, "schedule_run")?;
    }
    Ok(())
}
