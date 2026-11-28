//! Corroborate manually reviewed evidence against posted ledger facts.

use beankeeper::types::AccountCode;
use chrono::{Datelike, NaiveDate};
use rusqlite::{OptionalExtension, params};

use super::{
    ContractorTaxError, CounterpartyId, EvidenceKind, EvidenceSource, PaymentEvidence,
    PaymentMethod, PaymentReview, read_review,
};
use crate::{
    db::{
        calendar_date,
        companies::Company,
        counterparties::{CounterpartyRole, find_counterparty},
        ids::TransactionId,
    },
    error::CliError,
};

#[derive(Debug)]
pub(super) struct Posting {
    pub(super) date: NaiveDate,
    usd: bool,
    document: bool,
    is_reversal: bool,
    associated_party: Option<CounterpartyId>,
    pub(super) reversal: Option<(TransactionId, NaiveDate)>,
}

pub(super) fn posted(
    company: &Company<'_>,
    posting: TransactionId,
) -> Result<Option<Posting>, CliError> {
    Ok(company.read().query_row(
        "SELECT t.date, t.currency='USD',
             EXISTS(SELECT 1 FROM book_documents d WHERE d.company_slug=t.company_slug AND d.posting_id=t.id),
             EXISTS(SELECT 1 FROM posting_reversals rr WHERE rr.company_slug=t.company_slug AND rr.reversal_id=t.id),
             p.counterparty_id, u.id, u.date
         FROM posted_transactions t
         LEFT JOIN transaction_counterparties p ON p.company_slug=t.company_slug AND p.transaction_id=t.id
         LEFT JOIN posting_reversals r ON r.company_slug=t.company_slug AND r.original_id=CAST(t.id AS TEXT)
         LEFT JOIN posted_transactions u ON u.company_slug=t.company_slug AND u.id=r.reversal_id
         WHERE t.company_slug=?1 AND t.id=?2",
        params![company.slug(), posting],
        |row| {
            let reversal = match row.get::<_, Option<TransactionId>>(5)? {
                Some(id) => Some((id, calendar_date::read(row, 6)?)),
                None => None,
            };
            Ok(Posting {
                date: calendar_date::read(row, 0)?,
                usd: row.get(1)?,
                document: row.get(2)?,
                is_reversal: row.get(3)?,
                associated_party: row.get(4)?,
                reversal,
            })
        },
    ).optional()?)
}

pub(super) struct At {
    pub(super) through: NaiveDate,
    pub(super) reversal: Option<(TransactionId, NaiveDate)>,
}

pub(super) fn check(
    company: &Company<'_>,
    review: &PaymentReview,
    posting: &Posting,
    at: At,
) -> Result<bool, CliError> {
    if !posting.usd
        || posting.document
        || posting.is_reversal
        || posting
            .associated_party
            .as_ref()
            .is_some_and(|party| party != &review.party)
    {
        return Ok(false);
    }

    let historical = at.reversal.map(|(_, date)| date);
    let allocations: (i64, bool, bool) = company.read().query_row(
        "SELECT COALESCE(SUM(a.amount_minor),0), COUNT(*)>0,
            COUNT(*)>0 AND COALESCE(MIN(d.counterparty_id=?3 AND json_extract(d.issued,'$.kind')='bill'),0)
         FROM document_allocations a
         JOIN book_documents d ON d.company_slug=a.company_slug AND d.id=a.document_id
         WHERE a.company_slug=?1 AND a.source_id=?2 AND a.kind='payment' AND a.effective_on<=?4
           AND (a.reversed_on IS NULL OR a.reversed_on>?4 OR a.reversed_on>=?5)",
        params![company.slug(), review.posting_id, &review.party, at.through, historical],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    match &review.source {
        EvidenceSource::BillSettlement => {
            let cash_proof = review.method != PaymentMethod::Direct
                || cash_outflow(company, review.posting_id, None, false)?
                    >= review.amount_minor.minor_units();
            Ok(allocations.2 && cash_proof && allocations.0 == review.amount_minor.minor_units())
        }
        _ if allocations.1 => Ok(false),
        EvidenceSource::DirectPayment { account } | EvidenceSource::Refund { account, .. } => {
            let sign = if matches!(review.source, EvidenceSource::Refund { .. }) {
                -1
            } else {
                1
            };
            let processor = matches!(
                review.method,
                PaymentMethod::PaymentCard | PaymentMethod::ThirdPartyNetwork
            );
            let actual = cash_outflow(company, review.posting_id, Some(account), processor)?;
            Ok(actual == review.amount_minor.minor_units() * sign)
        }
    }
}

fn cash_outflow(
    company: &Company<'_>,
    posting: TransactionId,
    account: Option<&AccountCode>,
    processor: bool,
) -> Result<i64, CliError> {
    Ok(company.read().query_row(
        "SELECT COALESCE(SUM(CASE e.direction WHEN 'credit' THEN e.amount ELSE -e.amount END),0)
         FROM entries e JOIN accounts a ON a.company_slug=?1 AND a.code=e.account_code
         WHERE e.transaction_id=?2 AND (?3 IS NULL OR e.account_code=?3)
           AND ((a.type='asset' AND a.cash_class IN ('cash','cash_equivalent'))
                OR (?4 AND a.type='liability'))",
        params![
            company.slug(),
            posting,
            account.map(AccountCode::as_str),
            processor
        ],
        |row| row.get(0),
    )?)
}

pub(in crate::db) fn validate_review(
    company: &Company<'_>,
    review: &PaymentReview,
) -> Result<(), CliError> {
    let party = find_counterparty(company, &review.party)?;
    if !party.role().includes(CounterpartyRole::Vendor) {
        return Err(ContractorTaxError::AmbiguousEvidence.into());
    }
    let source = posted(company, review.posting_id)?.ok_or(ContractorTaxError::InvalidEvidence)?;
    let through = NaiveDate::from_ymd_opt(9999, 12, 31).ok_or(ContractorTaxError::InvalidYear)?;
    if !check(
        company,
        review,
        &source,
        At {
            through,
            reversal: source.reversal,
        },
    )? {
        return Err(ContractorTaxError::InvalidEvidence.into());
    }
    if let EvidenceSource::Refund {
        original,
        confirm_cross_year,
        ..
    } = &review.source
    {
        validate_refund(company, review, &source, *original, *confirm_cross_year)?;
    }
    Ok(())
}

fn validate_refund(
    company: &Company<'_>,
    review: &PaymentReview,
    refund: &Posting,
    original_id: TransactionId,
    confirm_cross_year: bool,
) -> Result<(), CliError> {
    let original =
        read_review(company, original_id)?.ok_or(ContractorTaxError::InvalidAdjustment)?;
    let posting = posted(company, original_id)?.ok_or(ContractorTaxError::InvalidAdjustment)?;
    if original_id == review.posting_id
        || matches!(original.review.source, EvidenceSource::Refund { .. })
        || original.review.party != review.party
        || original.review.method != review.method
        || posting.date > refund.date
        || (posting.date.year() != refund.date.year() && !confirm_cross_year)
        || posting.reversal.is_some()
    {
        return Err(ContractorTaxError::InvalidAdjustment.into());
    }
    let earlier: i64 = company.read().query_row(
        "SELECT COALESCE(SUM(CAST(json_extract(r.payload,'$.amount_minor') AS INTEGER)),0)
         FROM contractor_reviews r
         WHERE r.company_slug=?1 AND r.posting_id<>?2
           AND json_extract(r.payload,'$.source.kind')='refund'
           AND json_extract(r.payload,'$.source.original')=?3
           AND NOT EXISTS (SELECT 1 FROM posting_reversals p WHERE p.company_slug=r.company_slug
                           AND p.original_id=CAST(r.posting_id AS TEXT))",
        params![company.slug(), review.posting_id, original_id],
        |row| row.get(0),
    )?;
    if earlier
        .checked_add(review.amount_minor.minor_units())
        .is_none_or(|total| total > original.review.amount_minor.minor_units())
    {
        return Err(ContractorTaxError::InvalidAdjustment.into());
    }
    Ok(())
}

pub(super) fn candidates(
    company: &Company<'_>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<PaymentEvidence>, CliError> {
    let mut query = company.read().prepare(
        "SELECT t.id,t.date,d.counterparty_id,0,1
         FROM book_documents d JOIN posted_transactions t ON t.company_slug=d.company_slug AND t.id=d.posting_id
         WHERE d.company_slug=?1 AND t.currency='USD' AND d.issue_date BETWEEN ?2 AND ?3
           AND json_extract(d.issued,'$.kind')='bill'
         UNION ALL
         SELECT t.id,t.date,CASE WHEN COUNT(DISTINCT d.counterparty_id)=1 THEN MIN(d.counterparty_id) END,
                SUM(a.amount_minor),0
         FROM posted_transactions t
         JOIN document_allocations a ON a.company_slug=t.company_slug AND a.source_id=t.id
         JOIN book_documents d ON d.company_slug=a.company_slug AND d.id=a.document_id
         WHERE t.company_slug=?1 AND t.currency='USD' AND t.date BETWEEN ?2 AND ?3
           AND a.kind='payment' AND json_extract(d.issued,'$.kind')='bill'
           AND a.effective_on<=?3 AND (a.reversed_on IS NULL OR a.reversed_on>?3)
         GROUP BY t.id,t.date",
    )?;
    let rows = query.query_map(params![company.slug(), from, to], |row| {
        let accrual: bool = row.get(4)?;
        Ok(PaymentEvidence {
            posting_id: row.get(0)?,
            date: calendar_date::read(row, 1)?,
            party: row.get(2)?,
            amount_minor: row.get(3)?,
            kind: if accrual {
                EvidenceKind::Accrual
            } else {
                EvidenceKind::NeedsReview
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
