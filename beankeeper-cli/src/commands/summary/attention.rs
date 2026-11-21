//! Read-only attention facts, with explicit limits where the ledger lacks a model.

use std::collections::BTreeSet;

use beankeeper::core::{Frequency, RecurrenceError, occurrence_date};
use chrono::{Days, NaiveDate};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::commands::report_aging::{self, Movement};
use crate::db;
use crate::error::CliError;

use super::{add, sub};

/// The already-resolved company, currency, as-of date and account selections.
pub(super) struct AttentionContext<'a> {
    pub conn: &'a Connection,
    pub company: &'a str,
    pub currency: &'a str,
    pub as_of: NaiveDate,
    pub cash: &'a [String],
    pub receivables: &'a [String],
    pub payables: &'a [String],
}

/// Aggregate facts without posting schedules, changing clearings or writing audit
/// records. A missing due-date model remains explicit rather than implying zero.
pub(super) fn compute(context: &AttentionContext<'_>) -> Result<Value, CliError> {
    let unavailable = json!({
        "available": false, "count": null, "total": null,
        "reason": "The ledger has no invoice due dates. Older open items are shown separately; their payment deadlines are unknown."
    });
    Ok(json!({
        "uncategorized": uncategorized(context)?,
        "unreconciled_accounts": unreconciled(context)?,
        "recurring_due": recurring(context)?,
        "aged_receivables": aged(context, context.receivables, false)?,
        "aged_payables": aged(context, context.payables, true)?,
        "overdue_receivables": unavailable,
        "overdue_payables": unavailable,
    }))
}

fn uncategorized(context: &AttentionContext<'_>) -> Result<Value, CliError> {
    let mut statement = context.conn.prepare(
        "SELECT t.id, e.account_code, SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END)
         FROM transactions t JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug
         JOIN accounts a ON a.code = e.account_code AND a.company_slug = e.company_slug
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND t.date <= ?3
           AND (lower(a.name) LIKE '%uncategorized%' OR lower(a.name) LIKE '%suspense%')
         GROUP BY t.id, e.account_code ORDER BY t.id, e.account_code",
    )?;
    let rows = statement.query_map(
        params![context.company, context.currency, context.as_of.to_string()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(2)?)),
    )?;
    let mut transactions = BTreeSet::new();
    let mut total = 0;
    for row in rows {
        let (id, amount) = row?;
        if amount != 0 {
            transactions.insert(id);
            total = add(total, absolute(amount)?)?;
        }
    }
    Ok(json!({
        "id": "uncategorized", "available": true,
        "count": transactions.len(), "total": total,
        "basis": "activity-on-named-uncategorized-or-suspense-accounts",
        "reason": "Counts transactions touching accounts named Uncategorized or Suspense, including historical corrections. This is activity to review, not a guaranteed count of unresolved transactions."
    }))
}

fn absolute(amount: i64) -> Result<i64, CliError> {
    amount.checked_abs().ok_or_else(|| {
        CliError::Validation("summary amount exceeds the supported minor-unit range".into())
    })
}

fn unreconciled(context: &AttentionContext<'_>) -> Result<Value, CliError> {
    let mut statement = context.conn.prepare(
        "SELECT e.amount, COALESCE(SUM(CASE WHEN c.status = 'reconciled'
             AND c.cleared_date <= ?4 AND c.reconciled_date <= ?4 THEN c.amount ELSE 0 END), 0)
         FROM entries e JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug
         LEFT JOIN entry_clearings c ON c.entry_id = e.id
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND e.account_code = ?3 AND t.date <= ?4
         GROUP BY e.id ORDER BY e.id",
    )?;
    let mut count = 0_usize;
    let mut entry_count = 0_usize;
    let mut total = 0;
    for account in context.cash {
        let rows = statement.query_map(
            params![
                context.company,
                context.currency,
                account,
                context.as_of.to_string()
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut pending = false;
        for row in rows {
            let (amount, reconciled) = row?;
            let remaining = sub(amount, reconciled)?;
            if remaining > 0 {
                pending = true;
                entry_count += 1;
                total = add(total, remaining)?;
            }
        }
        count += usize::from(pending);
    }
    Ok(json!({
        "id": "unreconciled_accounts", "available": true,
        "count": count, "entry_count": entry_count, "total": total,
        "basis": "gross-unreconciled-entry-amounts-on-selected-cash-accounts",
        "reason": "Amounts not reconciled by the as-of date, including amounts already cleared. Incoming and outgoing amounts are added without cancelling each other."
    }))
}

fn recurring(context: &AttentionContext<'_>) -> Result<Value, CliError> {
    let from = context.as_of.succ_opt().ok_or_else(date_overflow)?;
    let through = context
        .as_of
        .checked_add_days(Days::new(14))
        .ok_or_else(date_overflow)?;
    let schedules = db::schedules::list_schedules(context.conn, context.company)?;
    let mut count = 0_usize;
    let mut schedule_count = 0_usize;
    let mut total = 0;
    for schedule in schedules
        .iter()
        .filter(|s| s.status == "pending" && s.currency == context.currency)
    {
        let frequency = schedule
            .frequency
            .parse::<Frequency>()
            .map_err(|error| CliError::Database(error.to_string()))?;
        let mut index = first_future_index(schedule, frequency, context.as_of)?;
        let amount = schedule
            .entries
            .iter()
            .filter(|entry| entry.direction == "debit")
            .try_fold(0, |sum, entry| add(sum, entry.amount))?;
        let before = count;
        while let Some(date) = future_occurrence(schedule, frequency, index)? {
            if date > through {
                break;
            }
            count += 1;
            total = add(total, amount)?;
            index = index.checked_add(1).ok_or_else(date_overflow)?;
        }
        schedule_count += usize::from(count != before);
    }
    Ok(json!({
        "id": "recurring_due", "available": true,
        "from": from, "to": through, "count": count,
        "schedule_count": schedule_count, "total": total,
        "basis": "unposted-occurrences-summed-on-one-side-of-each-balanced-template",
        "reason": "Includes every pending occurrence in the next 14 days, excluding today. Amounts are scheduled transaction volume, which can include income and noncash postings. Schedule status reflects its current state."
    }))
}

fn date_overflow() -> CliError {
    CliError::Validation("summary dates exceed the supported date range".into())
}

/// Search without walking a possibly years-old backlog. Occurrences are anchored
/// to the template start, preserving month-end and leap-year rules.
fn first_future_index(
    schedule: &db::schedules::Schedule,
    frequency: Frequency,
    as_of: NaiveDate,
) -> Result<u32, CliError> {
    let mut low = schedule.occurrence_index;
    let mut high = u32::MAX;
    while low < high {
        let middle = low + (high - low) / 2;
        if future_occurrence(schedule, frequency, middle)?.is_some_and(|date| date <= as_of) {
            low = middle.checked_add(1).ok_or_else(date_overflow)?;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

fn future_occurrence(
    schedule: &db::schedules::Schedule,
    frequency: Frequency,
    index: u32,
) -> Result<Option<NaiveDate>, CliError> {
    match occurrence_date(schedule.start_date, frequency, index) {
        Ok(date) => Ok(date.filter(|date| schedule.end_date.is_none_or(|end| *date <= end))),
        Err(RecurrenceError::DateOverflow) => Ok(None),
        Err(error) => Err(CliError::Database(error.to_string())),
    }
}

fn aged(
    context: &AttentionContext<'_>,
    accounts: &[String],
    payable: bool,
) -> Result<Value, CliError> {
    let mut count = 0_usize;
    let mut total = 0;
    for account in accounts {
        let movements = load_movements(context, account, payable)?;
        let matched = report_aging::apply_specific(&movements);
        for item in matched.open_items {
            if (context.as_of - item.date).num_days() > 30 {
                count += 1;
                total = add(total, item.open)?;
            }
        }
    }
    Ok(json!({
        "id": if payable { "aged_payables" } else { "aged_receivables" },
        "available": true, "count": count, "total": total,
        "older_than_days": 30,
        "basis": "open-charges-by-transaction-date-specific-then-fifo",
        "reason": "Open charges more than 30 days old after linked settlements and FIFO matching. Age does not establish that payment is overdue."
    }))
}

fn load_movements(
    context: &AttentionContext<'_>,
    account: &str,
    payable: bool,
) -> Result<Vec<Movement>, CliError> {
    let mut statement = context.conn.prepare(
        "SELECT t.id, t.date, t.description, t.reference, t.metadata,
             SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END),
             SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END)
         FROM entries e JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND e.account_code = ?3 AND t.date <= ?4
         GROUP BY t.id ORDER BY t.date, t.id",
    )?;
    let rows = statement.query_map(
        params![
            context.company,
            context.currency,
            account,
            context.as_of.to_string()
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (transaction_id, date, description, reference, metadata, debit, credit) = row?;
        let date = date.parse::<NaiveDate>().map_err(|error| {
            CliError::Database(format!("invalid stored transaction date: {error}"))
        })?;
        Movement::new(
            transaction_id,
            date,
            description,
            reference,
            if payable {
                sub(credit, debit)?
            } else {
                sub(debit, credit)?
            },
            report_aging::parse_link_spec(metadata.as_deref(), report_aging::DEFAULT_LINK_KEY),
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Actor, PostEntryParams, PostTransactionParams, connection::Db};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn setup() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        db::companies::create_company(db.conn(), "co", "Company", None)?;
        for (code, name, kind) in [
            ("1000", "Cash", "asset"),
            ("1100", "Receivable", "asset"),
            ("2000", "Payable", "liability"),
            ("3000", "Suspense", "equity"),
            ("4000", "Sales", "revenue"),
            ("5000", "Rent", "expense"),
        ] {
            db::accounts::create_account(db.conn(), "co", code, name, kind, None)?;
        }
        Ok(db)
    }

    fn entry(code: &str, direction: &str, amount: i64) -> PostEntryParams {
        PostEntryParams {
            account_code: code.into(),
            direction: direction.into(),
            amount,
            memo: None,
            tax_category: None,
        }
    }

    fn post(
        db: &Db,
        date: &str,
        accounts: (&str, &str),
        amount: i64,
        reference: Option<&str>,
        metadata: Option<&str>,
    ) -> Result<i64, CliError> {
        let result = db::post_transaction(
            db.conn(),
            &PostTransactionParams {
                company_slug: "co",
                description: "Attention fixture",
                metadata,
                currency: "USD",
                date,
                entries: &[
                    entry(accounts.0, "debit", amount),
                    entry(accounts.1, "credit", amount),
                ],
                correlate: None,
                reference,
                on_conflict: db::ConflictStrategy::Error,
                actor: &Actor::new("test"),
            },
        )?;
        match result {
            db::PostResult::Created(id) | db::PostResult::Skipped(id) => Ok(id),
        }
    }

    fn attention(db: &Db, as_of: &str) -> Result<Value, CliError> {
        compute(&AttentionContext {
            conn: db.conn(),
            company: "co",
            currency: "USD",
            as_of: as_of
                .parse()
                .map_err(|error: chrono::ParseError| CliError::Validation(error.to_string()))?,
            cash: &["1000".into()],
            receivables: &["1100".into()],
            payables: &["2000".into()],
        })
    }

    #[test]
    fn attention_reports_missing_due_dates_and_matches_specific_partial_payments() -> TestResult {
        let db = setup()?;
        post(
            &db,
            "2024-01-01",
            ("1100", "4000"),
            10000,
            Some("old"),
            None,
        )?;
        post(
            &db,
            "2024-01-25",
            ("1100", "4000"),
            8000,
            Some("recent"),
            None,
        )?;
        post(
            &db,
            "2024-02-01",
            ("1000", "1100"),
            6000,
            None,
            Some(r#"{"applies_to":"recent"}"#),
        )?;
        post(&db, "2024-01-01", ("5000", "2000"), 7000, None, None)?;
        post(&db, "2024-02-01", ("2000", "1000"), 2000, None, None)?;
        post(&db, "2024-03-01", ("1000", "1100"), 10000, None, None)?;
        let before = db.conn().total_changes();
        let facts = attention(&db, "2024-02-01")?;
        assert_eq!(facts["aged_receivables"]["count"], 1);
        assert_eq!(facts["aged_receivables"]["total"], 10000);
        assert_eq!(facts["aged_payables"]["count"], 1);
        assert_eq!(facts["aged_payables"]["total"], 5000);
        assert_eq!(facts["overdue_receivables"]["available"], false);
        assert!(facts["overdue_payables"]["total"].is_null());
        assert_eq!(db.conn().total_changes(), before);
        Ok(())
    }

    #[test]
    fn unreconciled_includes_cleared_items_until_reconciliation_date() -> TestResult {
        let db = setup()?;
        let id = post(&db, "2024-01-01", ("1000", "4000"), 10000, None, None)?;
        let entry_id = db::get_entries_for_transaction(db.conn(), id)?
            .into_iter()
            .find(|entry| entry.account_code == "1000")
            .ok_or("missing cash entry")?
            .id;
        let mut request = db::audit::ClearRequest {
            company_slug: "co",
            transaction_id: id,
            entry_id,
            amount: None,
            date: "2024-01-15".parse()?,
            statement: Some("statement"),
            status: db::clearings::ClearingStatus::Cleared,
        };
        db::audit::clear_entry(db.conn(), &Actor::new("test"), &request)?;
        request.date = "2024-02-02".parse()?;
        request.status = db::clearings::ClearingStatus::Reconciled;
        db::audit::clear_entry(db.conn(), &Actor::new("test"), &request)?;
        let prior = attention(&db, "2024-02-01")?;
        assert_eq!(prior["unreconciled_accounts"]["count"], 1);
        assert_eq!(prior["unreconciled_accounts"]["total"], 10000);
        let after = attention(&db, "2024-02-02")?;
        assert_eq!(after["unreconciled_accounts"]["count"], 0);
        assert_eq!(after["unreconciled_accounts"]["total"], 0);
        Ok(())
    }

    #[test]
    fn recurring_counts_every_future_occurrence_without_posting_backlog() -> TestResult {
        let db = setup()?;
        let params = db::schedules::CreateScheduleParams {
            description: "Daily rent".into(),
            currency: "USD".into(),
            start_date: "2020-01-01".parse()?,
            frequency: Frequency::Daily,
            end_date: None,
            entries: vec![entry("5000", "debit", 100), entry("1000", "credit", 100)],
            metadata: None,
        };
        db::schedules::create_schedule(db.conn(), &Actor::new("test"), "co", &params)?;
        let before = db.conn().total_changes();
        let facts = attention(&db, "2024-02-01")?;
        assert_eq!(facts["recurring_due"]["from"], "2024-02-02");
        assert_eq!(facts["recurring_due"]["to"], "2024-02-15");
        assert_eq!(facts["recurring_due"]["count"], 14);
        assert_eq!(facts["recurring_due"]["schedule_count"], 1);
        assert_eq!(facts["recurring_due"]["total"], 1400);
        assert_eq!(db.conn().total_changes(), before);
        let schedules = db::schedules::list_schedules(db.conn(), "co")?;
        assert_eq!(schedules[0].next_due, Some("2020-01-01".parse()?));
        Ok(())
    }

    #[test]
    fn recurring_uses_current_status_and_respects_once_and_window_boundaries() -> TestResult {
        let db = setup()?;
        let actor = Actor::new("test");
        let mut params = db::schedules::CreateScheduleParams {
            description: "One time".into(),
            currency: "USD".into(),
            start_date: "2024-02-03".parse()?,
            frequency: Frequency::Once,
            end_date: None,
            entries: vec![entry("5000", "debit", 100), entry("1000", "credit", 100)],
            metadata: None,
        };
        db::schedules::create_schedule(db.conn(), &actor, "co", &params)?;
        db::schedules::run_due(db.conn(), &actor, "co", "2024-02-03".parse()?, false)?;
        params.start_date = "2024-02-04".parse()?;
        let cancelled = db::schedules::create_schedule(db.conn(), &actor, "co", &params)?;
        db::schedules::cancel_schedule(db.conn(), &actor, "co", &cancelled.id)?;
        params.start_date = "2024-02-15".parse()?;
        db::schedules::create_schedule(db.conn(), &actor, "co", &params)?;
        params.start_date = "2024-02-16".parse()?;
        db::schedules::create_schedule(db.conn(), &actor, "co", &params)?;
        params.start_date = "2024-02-01".parse()?;
        db::schedules::create_schedule(db.conn(), &actor, "co", &params)?;
        let facts = attention(&db, "2024-02-01")?;
        assert_eq!(facts["recurring_due"]["count"], 1);
        assert_eq!(facts["recurring_due"]["total"], 100);
        Ok(())
    }

    #[test]
    fn recurring_preserves_month_end_anchor_and_finite_end() -> TestResult {
        let db = setup()?;
        let params = db::schedules::CreateScheduleParams {
            description: "Month end".into(),
            currency: "USD".into(),
            start_date: "2024-01-31".parse()?,
            frequency: Frequency::Monthly,
            end_date: Some("2024-03-15".parse()?),
            entries: vec![entry("5000", "debit", 100), entry("1000", "credit", 100)],
            metadata: None,
        };
        db::schedules::create_schedule(db.conn(), &Actor::new("test"), "co", &params)?;
        assert_eq!(attention(&db, "2024-02-28")?["recurring_due"]["count"], 1);
        assert_eq!(attention(&db, "2024-02-29")?["recurring_due"]["count"], 0);
        assert_eq!(attention(&db, "2024-03-14")?["recurring_due"]["count"], 0);
        Ok(())
    }

    #[test]
    fn uncategorized_is_explicitly_named_account_activity_and_excludes_future_posts() -> TestResult
    {
        let db = setup()?;
        post(&db, "2024-01-01", ("1000", "3000"), 100, None, None)?;
        post(&db, "2024-01-02", ("3000", "1000"), 200, None, None)?;
        post(&db, "2024-03-01", ("1000", "3000"), 9000, None, None)?;
        let facts = attention(&db, "2024-02-01")?;
        assert_eq!(facts["uncategorized"]["count"], 2);
        assert_eq!(facts["uncategorized"]["total"], 300);
        assert_eq!(
            facts["uncategorized"]["basis"],
            "activity-on-named-uncategorized-or-suspense-accounts"
        );
        Ok(())
    }
}
