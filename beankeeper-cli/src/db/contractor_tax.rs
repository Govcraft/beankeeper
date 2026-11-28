//! Read dated contractor payment evidence within a company snapshot.

mod model;
mod proof;

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

pub use model::*;
pub(in crate::db) use proof::validate_review;

use crate::{
    db::{companies::Company, counterparties::CounterpartyId, ids::TransactionId},
    error::CliError,
};

use super::ids::ReceiptId;

/// One redacted review and its immutable audit version.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewReceipt {
    pub receipt: ReceiptId,
    pub review: PaymentReview,
}

pub(in crate::db) fn read_review(
    company: &Company<'_>,
    posting: TransactionId,
) -> Result<Option<ReviewReceipt>, CliError> {
    let row = company.read().query_row(
        "SELECT receipt, payload FROM contractor_reviews WHERE company_slug=?1 AND posting_id=?2",
        params![company.slug(), posting],
        |row| Ok((row.get(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    row.map(|(receipt, payload)| {
        Ok(ReviewReceipt {
            receipt,
            review: decode(&payload)?,
        })
    })
    .transpose()
}

pub(in crate::db) fn read_profile(
    company: &Company<'_>,
    party: &CounterpartyId,
) -> Result<Option<TaxProfile>, CliError> {
    let payload: Option<String> = company
        .read()
        .query_row(
            "SELECT payload FROM contractor_tax_profiles WHERE company_slug=?1 AND party_id=?2",
            params![company.slug(), party],
            |row| row.get(0),
        )
        .optional()?;
    payload.as_deref().map(decode).transpose()
}

fn read_profiles(company: &Company<'_>) -> Result<BTreeMap<CounterpartyId, TaxProfile>, CliError> {
    let mut query = company.read().prepare(
        "SELECT party_id,payload FROM contractor_tax_profiles WHERE company_slug=?1 ORDER BY party_id",
    )?;
    let rows = query.query_map([company.slug()], |row| {
        Ok((row.get(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (party, payload) = row?;
        Ok((party, decode(&payload)?))
    })
    .collect()
}

pub(in crate::db) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_str(value)
        .map_err(|_| CliError::Database("invalid stored contractor tax data".into()))
}

/// Classify the dated, supported payment evidence for a single company.
///
/// # Errors
/// Returns validation or database errors if the worksheet cannot be constructed.
pub fn worksheet(company: &Company<'_>, year: TaxYear) -> Result<ContractorWorksheet, CliError> {
    let policy = ReportingYearPolicy::try_from(year)?;
    let (start, through) = policy.range()?;
    let candidates = proof::candidates(company, start, through)?;
    let mut reviews = company.read().prepare(
        "SELECT r.payload FROM contractor_reviews r
         JOIN posted_transactions t ON t.company_slug=r.company_slug AND t.id=r.posting_id
         WHERE r.company_slug=?1 AND t.date<=?2 ORDER BY t.date,t.id",
    )?;
    let rows = reviews.query_map(params![company.slug(), through], |row| {
        row.get::<_, String>(0)
    })?;
    let mut entries = Vec::new();
    let mut used = BTreeSet::new();
    for row in rows {
        let review: PaymentReview = decode(&row?)?;
        used.insert(review.posting_id);
        append_reviewed_evidence(company, review, through, &mut used, &mut entries)?;
    }
    entries.extend(
        candidates
            .into_iter()
            .filter(|candidate| !used.contains(&candidate.posting_id)),
    );
    Ok(classify_payments(
        &entries,
        &read_profiles(company)?,
        policy,
    )?)
}

fn append_reviewed_evidence(
    company: &Company<'_>,
    review: PaymentReview,
    through: NaiveDate,
    used: &mut BTreeSet<TransactionId>,
    entries: &mut Vec<PaymentEvidence>,
) -> Result<(), CliError> {
    let posting = proof::posted(company, review.posting_id)?
        .ok_or_else(|| CliError::Database("reviewed contractor posting is absent".into()))?;
    let reversal = posting.reversal.filter(|(_, date)| *date <= through);
    let valid = proof::check(company, &review, &posting)?;
    let amount_minor = review.amount_minor.minor_units();
    let is_refund = matches!(review.source, EvidenceSource::Refund { .. });
    let kind = if valid {
        match &review.source {
            EvidenceSource::Refund {
                original,
                confirm_cross_year,
                ..
            } => EvidenceKind::Refund {
                review: review.clone(),
                original: *original,
                confirmed: *confirm_cross_year,
            },
            _ => EvidenceKind::Payment(review.clone()),
        }
    } else {
        EvidenceKind::Changed
    };
    entries.push(PaymentEvidence {
        posting_id: review.posting_id,
        date: posting.date,
        party: Some(review.party.clone()),
        amount_minor,
        kind,
    });
    if let Some((id, date)) = reversal {
        used.insert(id);
        entries.push(PaymentEvidence {
            posting_id: id,
            date,
            party: Some(review.party),
            amount_minor,
            kind: if is_refund {
                EvidenceKind::RefundReversal {
                    refund: review.posting_id,
                }
            } else {
                EvidenceKind::Reversal {
                    original: review.posting_id,
                }
            },
        });
    }
    Ok(())
}

/// An annotated legacy row with a date and ID for staged manual review.
#[derive(Debug, Serialize)]
pub struct LegacyCandidate {
    posting_id: TransactionId,
    date: NaiveDate,
    /// Existing structured association when available.
    party: Option<CounterpartyId>,
}

/// Preview candidate legacy annotations without assigning identity or eligibility.
///
/// # Errors
/// Returns calendar validation or database errors if the preview cannot be built.
pub fn legacy_preview(
    company: &Company<'_>,
    year: TaxYear,
) -> Result<Vec<LegacyCandidate>, CliError> {
    let (start, through) = ReportingYearPolicy::try_from(year)?.range()?;
    let mut query = company.read().prepare(
        "SELECT t.id,t.date,p.counterparty_id FROM posted_transactions t
         LEFT JOIN transaction_counterparties p ON p.company_slug=t.company_slug AND p.transaction_id=t.id
         WHERE t.company_slug=?1 AND t.date BETWEEN ?2 AND ?3
           AND (json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END,'$.vendor') IS NOT NULL
             OR json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END,'$.payee') IS NOT NULL
             OR json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END,'$.contractor') IS NOT NULL
             OR json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END,'$.form1099') IS NOT NULL)
           AND NOT EXISTS(SELECT 1 FROM contractor_reviews r WHERE r.company_slug=t.company_slug AND r.posting_id=t.id)
         ORDER BY t.date,t.id",
    )?;
    let rows = query.query_map(params![company.slug(), start, through], |row| {
        Ok(LegacyCandidate {
            posting_id: row.get(0)?,
            date: super::calendar_date::read(row, 1)?,
            party: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
