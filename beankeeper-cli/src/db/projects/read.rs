//! Company-bound project reads and monthly accounting projections.

use std::collections::BTreeMap;

use beankeeper::types::{AccountType, Currency, DebitOrCredit};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    db::{
        amounts::PositiveMinorUnits,
        companies::Company,
        ids::{EntryId, TransactionId},
    },
    error::CliError,
};

use super::{
    BillingSource, EntryDistribution, Project, ProjectError, ProjectId, ProjectMonth,
    ProjectOperatingAmounts, ProjectTotals, TimeEntry, TimeEntryId,
};

pub(in crate::db) fn decode<T: DeserializeOwned>(raw: &str) -> Result<T, CliError> {
    serde_json::from_str(raw)
        .map_err(|error| CliError::Database(format!("invalid stored project data: {error}")))
}

/// Find one project, including archived projects, within the company.
///
/// # Errors
/// Returns an error if the project is missing or its record cannot be decoded.
pub fn find_project(company: &Company<'_>, id: &ProjectId) -> Result<Project, CliError> {
    let raw: String = company
        .read()
        .query_row(
            "SELECT data FROM projects WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("project '{id}' not found")))?;
    decode(&raw)
}

/// List the company's visible project directory.
///
/// # Errors
/// Returns a storage or project decoding error.
pub fn list_projects(company: &Company<'_>, archived: bool) -> Result<Vec<Project>, CliError> {
    let mut query = company.read().prepare(
        "SELECT data FROM projects WHERE company_slug=?1 AND (?2 OR NOT archived) ORDER BY code, id LIMIT 1000",
    )?;
    let rows = query.query_map(params![company.slug(), archived], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| decode(&row?)).collect()
}

pub(in crate::db) fn list_all_projects(
    company: &Company<'_>,
    currency: Currency,
) -> Result<Vec<Project>, CliError> {
    let mut query = company.read().prepare(
        "SELECT data FROM projects WHERE company_slug=?1 AND json_extract(data,'$.currency')=?2 ORDER BY code,id",
    )?;
    let rows = query.query_map(params![company.slug(), currency.code()], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| decode(&row?)).collect()
}

/// Find one time entry within the company.
///
/// # Errors
/// Returns a named not-found or storage error.
pub fn find_time(company: &Company<'_>, id: &TimeEntryId) -> Result<TimeEntry, CliError> {
    let raw: String = company
        .read()
        .query_row(
            "SELECT data FROM project_time_entries WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("time entry '{id}' not found")))?;
    decode(&raw)
}

/// List one project's most recent dated work entries.
///
/// # Errors
/// Returns a named not-found or storage error.
pub fn list_time(company: &Company<'_>, project: &ProjectId) -> Result<Vec<TimeEntry>, CliError> {
    find_project(company, project)?;
    let mut query = company.read().prepare(
        "SELECT data FROM project_time_entries WHERE company_slug=?1 AND project_id=?2
         ORDER BY worked_on DESC,id DESC LIMIT 1000",
    )?;
    let rows = query.query_map(params![company.slug(), project], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| decode(&row?)).collect()
}

#[derive(Debug, Clone)]
pub(in crate::db) struct ProfitEntry {
    pub id: EntryId,
    pub transaction: TransactionId,
    pub date: NaiveDate,
    pub kind: AccountType,
    pub direction: DebitOrCredit,
    pub amount: PositiveMinorUnits,
    pub currency: Currency,
}

pub(in crate::db) fn get_entry(
    company: &Company<'_>,
    id: EntryId,
) -> Result<ProfitEntry, CliError> {
    company.read().query_row(
        "SELECT e.id,t.id,t.date,a.type,e.direction,e.amount,t.currency
         FROM entries e JOIN posted_transactions t ON t.company_slug=e.company_slug AND t.id=e.transaction_id
         JOIN accounts a ON a.company_slug=e.company_slug AND a.code=e.account_code
         WHERE e.company_slug=?1 AND e.id=?2",
        params![company.slug(), id], profit_entry_row,
    ).optional()?.ok_or_else(|| CliError::NotFound(format!("posted journal entry '{id}' not found")))
}

fn profit_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProfitEntry> {
    Ok(ProfitEntry {
        id: row.get(0)?,
        transaction: row.get(1)?,
        date: crate::db::calendar_date::read(row, 2)?,
        kind: row.get(3)?,
        direction: row.get(4)?,
        amount: row.get(5)?,
        currency: row.get(6)?,
    })
}

#[derive(Debug, Clone, Serialize)]
struct OperatingEntry {
    entry: EntryId,
    posting: TransactionId,
    date: NaiveDate,
    account_type: AccountType,
    direction: DebitOrCredit,
    amount_minor: PositiveMinorUnits,
    distribution: Option<EntryDistribution>,
}

fn journal_activity(
    company: &Company<'_>,
    window: ProjectMonth,
    currency: Currency,
) -> Result<Vec<OperatingEntry>, CliError> {
    let mut query = company.read().prepare(
        "SELECT e.id,t.id,t.date,a.type,e.direction,e.amount,t.currency,
            (SELECT d.data FROM project_distributions d
             WHERE d.company_slug=e.company_slug AND d.entry_id=e.id AND d.effective_on<=?3
             ORDER BY d.effective_on DESC LIMIT 1)
         FROM posted_transactions t
         JOIN entries e ON e.transaction_id=t.id AND e.company_slug=t.company_slug
         JOIN accounts a ON a.company_slug=e.company_slug AND a.code=e.account_code
         WHERE t.company_slug=?1 AND t.date BETWEEN ?2 AND ?3 AND t.currency=?4
           AND a.type IN ('expense','revenue') ORDER BY t.date,e.id",
    )?;
    let rows = query.query_map(
        params![
            company.slug(),
            window.start(),
            window.end()?,
            currency.code()
        ],
        |row| Ok((profit_entry_row(row)?, row.get::<_, Option<String>>(7)?)),
    )?;
    rows.map(|row| {
        let (entry, raw) = row?;
        Ok(OperatingEntry {
            entry: entry.id,
            posting: entry.transaction,
            date: entry.date,
            account_type: entry.kind,
            direction: entry.direction,
            amount_minor: entry.amount,
            distribution: raw.as_deref().map(decode).transpose()?,
        })
    })
    .collect()
}

fn project_profitability(
    activity: &[OperatingEntry],
) -> Result<(ProjectTotals, ProjectOperatingAmounts), ProjectError> {
    let mut projects = BTreeMap::<ProjectId, ProjectOperatingAmounts>::new();
    let mut unallocated = ProjectOperatingAmounts::default();
    for movement in activity {
        let mut remainder = movement.amount_minor.minor_units();
        if let Some(distribution) = &movement.distribution {
            for split in &distribution.splits {
                let amount = split.assignment.amount_minor.minor_units();
                projects
                    .entry(split.assignment.project.clone())
                    .or_default()
                    .add_entry(movement.account_type, movement.direction, amount)?;
                remainder = remainder
                    .checked_sub(amount)
                    .filter(|left| *left >= 0)
                    .ok_or(ProjectError::InvalidSplit)?;
            }
        }
        unallocated.add_entry(movement.account_type, movement.direction, remainder)?;
    }
    Ok((projects, unallocated))
}

/// Dated source eligibility used both in project reports and invoice drafting.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableSource {
    /// Stable source identity.
    pub source: BillingSource,
    /// Amount available to this project in minor units.
    pub amount_minor: PositiveMinorUnits,
    /// Monthly posting date or actual work date.
    pub date: NaiveDate,
    /// Description to show the bill reviewer.
    pub description: String,
    /// Time amount when the source is approved work.
    pub minutes: Option<u16>,
}

/// Unclaimed approved time and positive expense splits for one project.
///
/// # Errors
/// Returns missing company/project or storage and range errors.
pub fn unbilled(
    company: &Company<'_>,
    project: &ProjectId,
    from: NaiveDate,
    through: NaiveDate,
) -> Result<Vec<AvailableSource>, CliError> {
    if through < from {
        return Err(ProjectError::InvalidWindow.into());
    }
    find_project(company, project)?;
    available_sources(company, project, from, through)
}

fn available_sources(
    company: &Company<'_>,
    project: &ProjectId,
    from: NaiveDate,
    through: NaiveDate,
) -> Result<Vec<AvailableSource>, CliError> {
    let mut sources = time_sources(company, project, from, through)?;
    sources.extend(expense_sources(company, project, from, through)?);
    sources.sort_unstable_by(|a, b| (a.date, &a.source).cmp(&(b.date, &b.source)));
    Ok(sources)
}

fn time_sources(
    company: &Company<'_>,
    project: &ProjectId,
    from: NaiveDate,
    through: NaiveDate,
) -> Result<Vec<AvailableSource>, CliError> {
    let mut query = company.read().prepare(
        "SELECT work.data FROM project_time_entries work
         WHERE work.company_slug=?1 AND work.project_id=?2 AND work.worked_on BETWEEN ?3 AND ?4
           AND work.approved_on<=?4 AND json_extract(work.data,'$.rate_minor') IS NOT NULL
           AND NOT EXISTS(SELECT 1 FROM project_invoice_sources claim
              WHERE claim.company_slug=work.company_slug AND claim.time_id=work.id
                AND claim.claimed_on<=?4 AND (claim.released_on IS NULL OR claim.released_on>?4))
         ORDER BY work.worked_on,work.id",
    )?;
    let rows = query.query_map(params![company.slug(), project, from, through], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| {
        let work: TimeEntry = decode(&row?)?;
        let amount = work
            .work
            .rate_minor
            .ok_or(ProjectError::SourceUnavailable)?
            .priced_minutes(work.work.minutes)?;
        Ok(AvailableSource {
            source: BillingSource::Time(work.id),
            amount_minor: amount,
            date: work.work.date,
            description: format!(
                "{} ({} minutes)",
                work.work.description,
                work.work.minutes.minutes()
            ),
            minutes: Some(work.work.minutes.minutes()),
        })
    })
    .collect()
}

fn expense_sources(
    company: &Company<'_>,
    project: &ProjectId,
    from: NaiveDate,
    through: NaiveDate,
) -> Result<Vec<AvailableSource>, CliError> {
    let mut query = company.read().prepare(
        "SELECT s.id,s.amount_minor,t.date,t.description
         FROM project_splits s
         JOIN entries e ON e.company_slug=s.company_slug AND e.id=s.entry_id
         JOIN posted_transactions t ON t.company_slug=e.company_slug AND t.id=e.transaction_id
         WHERE s.company_slug=?1 AND s.project_id=?2 AND t.date BETWEEN ?3 AND ?4 AND s.billable=1
           AND s.effective_on=(SELECT MAX(d.effective_on) FROM project_distributions d
               WHERE d.company_slug=s.company_slug AND d.entry_id=s.entry_id AND d.effective_on<=?4)
           AND NOT EXISTS(SELECT 1 FROM bookkeeping_audit a
              JOIN posted_transactions reversal ON reversal.company_slug=a.company_slug AND reversal.id=a.related_posting
              WHERE a.company_slug=t.company_slug AND a.action='reverse'
                AND a.entity_id=CAST(t.id AS TEXT) AND reversal.date<=?4)
           AND NOT EXISTS(SELECT 1 FROM project_invoice_sources claim
              WHERE claim.company_slug=s.company_slug AND claim.split_id=s.id
                AND claim.claimed_on<=?4 AND (claim.released_on IS NULL OR claim.released_on>?4))
         ORDER BY t.date,s.id",
    )?;
    let rows = query.query_map(params![company.slug(), project, from, through], |row| {
        Ok(AvailableSource {
            source: BillingSource::Expense(row.get(0)?),
            amount_minor: row.get(1)?,
            date: crate::db::calendar_date::read(row, 2)?,
            description: row.get(3)?,
            minutes: None,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One project's monthly activity, budget, and unbilled figures.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectReportRow {
    /// Project and customer identity.
    pub project: Project,
    /// Signed recognized ledger amounts.
    #[serde(flatten)]
    pub recognized: ProjectOperatingAmounts,
    /// Net recognized revenue less expense.
    pub profit_minor: i64,
    /// Monthly expense budget, when configured.
    pub budget_minor: Option<i64>,
    /// Budget less recognized expense.
    pub budget_remaining_minor: Option<i64>,
    /// Approved, unclaimed work in the report period.
    pub unbilled_minutes: u64,
    /// Unbilled time value and expense separately from booked revenue.
    pub unbilled_time_minor: i64,
    /// Unbilled expense separately from booked revenue.
    pub unbilled_expense_minor: i64,
}

/// A project profitability report for a month and one currency.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectReport {
    /// Inclusive whole calendar month.
    pub month: ProjectMonth,
    /// Currency of every amount in this report.
    pub currency: Currency,
    /// Active and archived rows.
    pub projects: Vec<ProjectReportRow>,
    /// Recognized activity lacking any project assignment.
    pub unallocated: ProjectOperatingAmounts,
    /// Ledger entries and their exact effective distributions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entries: Vec<OperatingEntry>,
}

/// Report project activity and unallocated residue within one month/currency.
///
/// # Errors
/// Returns amount overflow, invalid stored splits, or storage errors.
pub fn monthly_report(
    company: &Company<'_>,
    month: ProjectMonth,
    currency: Currency,
    detail: bool,
) -> Result<ProjectReport, CliError> {
    let activity = journal_activity(company, month, currency)?;
    let (mut totals, unallocated) = project_profitability(&activity)?;
    let projects = list_all_projects(company, currency)?
        .into_iter()
        .map(|project| {
            let recognized = totals.remove(&project.id).unwrap_or_default();
            project_row(company, month, project, recognized)
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    Ok(ProjectReport {
        month,
        currency,
        projects,
        unallocated,
        entries: if detail { activity } else { Vec::new() },
    })
}

fn project_row(
    company: &Company<'_>,
    month: ProjectMonth,
    project: Project,
    recognized: ProjectOperatingAmounts,
) -> Result<ProjectReportRow, CliError> {
    let budget_minor: Option<i64> = company.read().query_row(
        "SELECT amount_minor FROM project_expense_budgets WHERE company_slug=?1 AND project_id=?2 AND month_start=?3",
        params![company.slug(), project.id, month.start()], |row| row.get(0),
    ).optional()?;
    let budget_remaining_minor = budget_minor
        .map(|budget| {
            budget
                .checked_sub(recognized.expense_minor)
                .ok_or(ProjectError::AmountOutOfRange)
        })
        .transpose()?;
    let mut minutes = 0_u64;
    let mut time = 0_i64;
    let mut expense = 0_i64;
    for source in available_sources(company, &project.id, month.start(), month.end()?)? {
        if let Some(value) = source.minutes {
            minutes += u64::from(value);
        }
        let total = match source.source {
            BillingSource::Time(_) => &mut time,
            BillingSource::Expense(_) => &mut expense,
        };
        *total = total
            .checked_add(source.amount_minor.minor_units())
            .ok_or(ProjectError::AmountOutOfRange)?;
    }
    Ok(ProjectReportRow {
        project,
        recognized,
        profit_minor: recognized.profit()?,
        budget_minor,
        budget_remaining_minor,
        unbilled_minutes: minutes,
        unbilled_time_minor: time,
        unbilled_expense_minor: expense,
    })
}
