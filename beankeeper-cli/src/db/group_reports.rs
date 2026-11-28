//! Posted-journal scope and currency checks for cross-company statements.

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::{ToSql, params_from_iter, types::ValueRef};
use serde::Serialize;

use super::{
    calendar_date,
    companies::{Company, CompanySlug},
    fx,
    ids::TransactionId,
    metadata, push_transaction_dates,
};
use crate::error::CliError;

/// The selected, posted side of an intercompany link.
#[derive(Clone, Serialize)]
pub(crate) struct Journal {
    pub company: CompanySlug,
    pub transaction_id: TransactionId,
    pub date: NaiveDate,
}

pub(crate) struct LinkedJournal {
    pub journal: Journal,
    pub partner: Option<TransactionId>,
}

/// Check the book and all its in-scope postings against the requested currency.
/// Only linked postings need to leave this query.
pub(crate) fn scoped_journals(
    company: &Company<'_>,
    from: Option<NaiveDate>,
    to: NaiveDate,
    currency: Currency,
) -> Result<Vec<(TransactionId, LinkedJournal)>, CliError> {
    let slug = company.slug();
    let reader = company.read();
    if let Some(functional) = fx::functional_currency(reader, slug)?
        && currency != functional
    {
        return Err(CliError::Validation(format!(
            "company '{slug}' has functional currency {functional}, but the group report uses {currency}"
        )));
    }

    let correlate = metadata::sql_correlate("t.metadata");
    let expected = currency.code();
    let mut sql = format!(
        "SELECT t.id, t.date, t.currency, {correlate}
         FROM posted_transactions t
         WHERE t.company_slug = ?1 AND (t.currency IS NOT ?2 OR {correlate} IS NOT NULL)"
    );
    let mut bindings: Vec<&dyn ToSql> = vec![slug, &expected];
    push_transaction_dates(&mut sql, &mut bindings, from.as_ref(), Some(&to));
    sql.push_str(" ORDER BY t.date, t.id");
    let mut stmt = reader.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| {
        Ok((
            row.get::<_, TransactionId>(0)?,
            calendar_date::read(row, 1)?,
            row.get::<_, String>(2)?,
            match row.get_ref(3)? {
                ValueRef::Integer(id) => TransactionId::try_from(id).ok(),
                ValueRef::Text(text) => std::str::from_utf8(text)
                    .ok()
                    .and_then(|id| id.parse().ok()),
                _ => None,
            },
        ))
    })?;
    let mut linked = Vec::new();
    for row in rows {
        let (transaction_id, date, actual, partner) = row?;
        if actual != expected {
            return Err(CliError::Validation(format!(
                "company '{slug}' has a {actual} posting ({transaction_id}, {date}) in the report dates, but the group report uses {currency}"
            )));
        }
        linked.push((
            transaction_id,
            LinkedJournal {
                journal: Journal {
                    company: slug.clone(),
                    transaction_id,
                    date,
                },
                partner,
            },
        ));
    }
    Ok(linked)
}
