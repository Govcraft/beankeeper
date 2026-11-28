//! Read dated issuance and settlement snapshots within one company.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    db::{amounts::PositiveMinorUnits, calendar_date, companies::Company, ids::TransactionId},
    error::CliError,
};

use super::{
    AllocationId, AllocationKind, DocumentDraft, DocumentError, DocumentId, DocumentRevision,
    DocumentSettlementStatus, EffectiveAllocation, IssuedDocument, OpenItem,
};

/// Current draft and, when issued, its immutable financial snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentRecord {
    /// Identity fixed on initial draft creation.
    pub id: DocumentId,
    /// Latest draft revision.
    pub revision: DocumentRevision,
    /// Current or final editable terms.
    pub draft: DocumentDraft,
    /// Financial snapshot and source journal after issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued: Option<IssuedDocument>,
}

pub(in crate::db) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_str(value)
        .map_err(|error| CliError::Database(format!("invalid stored document data: {error}")))
}

/// Fetches one document by ID and company.
///
/// # Errors
/// Returns a not-found error for a missing ID, or a storage error for invalid data.
pub fn find_document(company: &Company<'_>, id: &DocumentId) -> Result<DocumentRecord, CliError> {
    let (revision, draft, issued) = company.read().query_row(
        "SELECT revision, draft, issued FROM book_documents WHERE company_slug = ?1 AND id = ?2",
        params![company.slug(), id],
        |row| Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
    ).optional()?.ok_or_else(|| CliError::NotFound(format!("document '{id}' not found in '{}'", company.slug())))?;
    Ok(DocumentRecord {
        id: id.clone(),
        revision: DocumentRevision::try_from(revision)?,
        draft: decode(&draft)?,
        issued: issued.as_deref().map(decode).transpose()?,
    })
}

/// Issued documents with issue dates on or before a cutoff.
///
/// # Errors
/// Returns database errors for incompatible or corrupt stored data.
pub fn list_issued_as_of(
    company: &Company<'_>,
    as_of: NaiveDate,
) -> Result<Vec<IssuedDocument>, CliError> {
    let mut query = company.read().prepare(
        "SELECT issued FROM book_documents WHERE company_slug = ?1 AND issue_date <= ?2
         ORDER BY issue_date, id",
    )?;
    let rows = query.query_map(params![company.slug(), as_of], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| decode(&row?)).collect()
}

fn allocation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EffectiveAllocation> {
    let kind = match row.get::<_, String>(5)?.as_str() {
        "payment" => AllocationKind::Payment,
        "credit" => AllocationKind::Credit,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(EffectiveAllocation {
        id: row.get(0)?,
        document_id: row.get(1)?,
        source_id: row.get(2)?,
        effective_on: calendar_date::read(row, 3)?,
        amount_minor: row.get(4)?,
        kind,
    })
}

/// Settlement allocations in effect at a cutoff, including partial payments.
///
/// # Errors
/// Returns database errors for incompatible or corrupt stored data.
pub fn list_allocations_as_of(
    company: &Company<'_>,
    as_of: NaiveDate,
) -> Result<Vec<EffectiveAllocation>, CliError> {
    let mut query = company.read().prepare(
        "SELECT id, document_id, source_id, effective_on, amount_minor, kind
         FROM document_allocations WHERE company_slug = ?1
           AND effective_on <= ?2 AND (reversed_on IS NULL OR reversed_on > ?2)
         ORDER BY effective_on, id",
    )?;
    let rows = query.query_map(params![company.slug(), as_of], allocation_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Complete dated history for one document allocation, including revoked ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllocationHistory {
    /// Original allocation terms.
    #[serde(flatten)]
    pub allocation: EffectiveAllocation,
    /// Date the allocation stopped applying, when revoked.
    pub reversed_on: Option<NaiveDate>,
    /// Original line number for credits.
    pub credit_line: Option<u32>,
}

pub(in crate::db) fn read_allocation(
    company: &Company<'_>,
    id: &AllocationId,
) -> Result<AllocationHistory, CliError> {
    company
        .read()
        .query_row(
            "SELECT id, document_id, source_id, effective_on, amount_minor, kind, reversed_on, credit_line
         FROM document_allocations WHERE company_slug = ?1 AND id = ?2",
            params![company.slug(), id],
            allocation_history_row,
        )
        .optional()?
        .ok_or_else(|| {
            CliError::NotFound(format!(
                "allocation '{id}' not found in '{}'",
                company.slug()
            ))
        })
}

/// Lists allocation lifecycle evidence effective through the cutoff.
///
/// # Errors
/// Returns database errors for incompatible or corrupt stored data.
pub fn allocation_history(
    company: &Company<'_>,
    through: NaiveDate,
) -> Result<Vec<AllocationHistory>, CliError> {
    let mut query = company.read().prepare(
        "SELECT id, document_id, source_id, effective_on, amount_minor, kind,
                CASE WHEN reversed_on <= ?2 THEN reversed_on END, credit_line
         FROM document_allocations WHERE company_slug = ?1 AND effective_on <= ?2
         ORDER BY effective_on, id",
    )?;
    let rows = query.query_map(params![company.slug(), through], allocation_history_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn allocation_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AllocationHistory> {
    let reversed_on: Option<String> = row.get(6)?;
    let reversed_on = reversed_on
        .map(|raw| {
            raw.parse::<NaiveDate>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(AllocationHistory {
        allocation: allocation_row(row)?,
        reversed_on,
        credit_line: row.get(7)?,
    })
}

/// Issued documents with balances, due dates and applied allocations at a cutoff.
/// A fully settled item appears with zero open and overdue amounts.
///
/// # Errors
/// Returns an accounting error for incoherent amounts, or a storage error.
pub fn open_items_as_of(
    company: &Company<'_>,
    as_of: NaiveDate,
) -> Result<Vec<OpenItem>, CliError> {
    let mut by_document = BTreeMap::<DocumentId, Vec<EffectiveAllocation>>::new();
    for allocation in list_allocations_as_of(company, as_of)? {
        by_document
            .entry(allocation.document_id.clone())
            .or_default()
            .push(allocation);
    }
    let mut refunds = refunds_as_of(company, as_of)?;
    list_issued_as_of(company, as_of)?
        .into_iter()
        .map(|document| {
            let allocations = by_document.remove(&document.id).unwrap_or_default();
            let applied = allocations.iter().try_fold(0_i64, |total, allocation| {
                total
                    .checked_add(allocation.amount_minor.minor_units())
                    .ok_or(DocumentError::OutOfRange)
            })?;
            let refunded_minor = refunds.remove(&document.id).unwrap_or_default();
            let applied = applied
                .checked_sub(refunded_minor)
                .filter(|amount| *amount >= 0)
                .ok_or(DocumentError::AllocationExceedsBalance)?;
            let open_minor = document
                .total_minor
                .minor_units()
                .checked_sub(applied)
                .filter(|open| *open >= 0)
                .ok_or(DocumentError::AllocationExceedsBalance)?;
            let days_overdue = if open_minor > 0 {
                document.due_date.days_overdue(as_of)
            } else {
                0
            };
            let status = if open_minor == 0 {
                DocumentSettlementStatus::Paid
            } else if applied == 0 {
                DocumentSettlementStatus::Open
            } else {
                DocumentSettlementStatus::Partial
            };
            Ok(OpenItem {
                document,
                open_minor,
                status,
                days_overdue,
                allocations,
                refunded_minor: if refunded_minor > 0 {
                    Some(beankeeper::types::Amount::from(refunded_minor).try_into()?)
                } else {
                    None
                },
            })
        })
        .collect()
}

fn refunds_as_of(
    company: &Company<'_>,
    as_of: NaiveDate,
) -> Result<BTreeMap<DocumentId, i64>, CliError> {
    let mut stmt = company.read().prepare_cached(
        "SELECT invoice,SUM(amount) FROM stripe_refund_postings
        WHERE company_slug=?1 AND booked_on<=?2 GROUP BY invoice",
    )?;
    let rows = stmt.query_map(params![company.slug(), as_of], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Copy)]
pub(in crate::db) enum CapacityFor<'a> {
    Document(&'a DocumentId),
    Source(TransactionId),
}

pub(in crate::db) fn peak_applied(
    company: &Company<'_>,
    item: CapacityFor<'_>,
    from: NaiveDate,
) -> Result<i128, CliError> {
    let (query, key): (&str, &dyn rusqlite::ToSql) = match &item {
        CapacityFor::Document(id) => ("SELECT effective_on, reversed_on, amount_minor FROM document_allocations
            WHERE company_slug = ?1 AND document_id = ?2 AND (reversed_on IS NULL OR reversed_on > ?3)", id),
        CapacityFor::Source(id) => ("SELECT effective_on, reversed_on, amount_minor FROM document_allocations
            WHERE company_slug = ?1 AND source_id = ?2 AND (reversed_on IS NULL OR reversed_on > ?3)", id),
    };
    let mut stmt = company.read().prepare_cached(query)?;
    let rows = stmt.query_map(params![company.slug(), key, from], |row| {
        let effective: NaiveDate = calendar_date::read(row, 0)?;
        let reversed: Option<NaiveDate> = row.get(1)?;
        let amount: PositiveMinorUnits = row.get(2)?;
        Ok((effective, reversed, amount))
    })?;
    let mut deltas = BTreeMap::<NaiveDate, i128>::new();
    for row in rows {
        let (effective, reversed, amount) = row?;
        let raw = i128::from(amount);
        *deltas.entry(effective.max(from)).or_default() += raw;
        if let Some(reversed) = reversed {
            *deltas.entry(reversed).or_default() -= raw;
        }
    }
    if let CapacityFor::Document(id) = item {
        let mut refunds = company.read().prepare_cached(
            "SELECT booked_on,amount FROM stripe_refund_postings
            WHERE company_slug=?1 AND invoice=?2",
        )?;
        let rows = refunds.query_map(params![company.slug(), id], |row| {
            Ok((
                calendar_date::read(row, 0)?,
                row.get::<_, PositiveMinorUnits>(1)?,
            ))
        })?;
        for row in rows {
            let (booked_on, amount) = row?;
            *deltas.entry(booked_on.max(from)).or_default() -= i128::from(amount);
        }
    }
    let mut used = 0_i128;
    let mut maximum = 0_i128;
    for (_, delta) in deltas {
        used += delta;
        maximum = maximum.max(used);
    }
    Ok(maximum)
}
