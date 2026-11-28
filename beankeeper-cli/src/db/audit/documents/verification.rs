//! Verify document projections against active audit history and posted journals.

use rusqlite::OptionalExtension;

use crate::{db::connection::ReadConn, error::CliError};

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    verify_projection(reader, "document", "book_documents", DOCUMENT_TERMS)?;
    verify_projection(
        reader,
        "document_allocation",
        "document_allocations",
        ALLOCATION_TERMS,
    )?;
    verify_projection(
        reader,
        "document_review",
        "document_review_requests",
        REVIEW_TERMS,
    )?;
    verify_sources(reader)
}

fn verify_projection(
    reader: ReadConn<'_>,
    entity: &str,
    table: &str,
    fields: &str,
) -> Result<(), CliError> {
    let sql = format!(
        "WITH current AS (
             SELECT event.id, event.entity_id, event.company_slug, event.after
             FROM bookkeeping_audit event JOIN (
                 SELECT MAX(id) AS id FROM bookkeeping_audit
                 WHERE entity=?1 GROUP BY company_slug, entity_id
             ) latest ON latest.id=event.id
         )
         SELECT COALESCE(physical.id, current.entity_id)
         FROM {table} physical FULL OUTER JOIN current
           ON current.company_slug=physical.company_slug AND current.entity_id=physical.id
         WHERE current.id IS NOT physical.receipt OR current.id IS NULL OR physical.id IS NULL
              OR ({fields}) LIMIT 1"
    );
    let invalid: Option<String> = reader
        .query_row(&sql, [entity], |row| row.get(0))
        .optional()?;
    if let Some(id) = invalid {
        return Err(CliError::Database(format!(
            "{entity} '{id}' differs from its current audit snapshot"
        )));
    }
    Ok(())
}

const DOCUMENT_TERMS: &str = "
    json_extract(current.after, '$.id') IS NOT physical.id
    OR json_extract(current.after, '$.revision') IS NOT physical.revision
    OR json_extract(current.after, '$.draft') IS NOT physical.draft
    OR json_extract(current.after, '$.issued') IS NOT physical.issued
    OR json_extract(current.after, '$.draft.number') IS NOT physical.number
    OR json_extract(current.after, '$.draft.counterparty_id') IS NOT physical.counterparty_id
    OR json_extract(current.after, '$.issued.issue_date') IS NOT physical.issue_date
    OR json_extract(current.after, '$.issued.due_date') IS NOT physical.due_date
    OR json_extract(current.after, '$.issued.currency') IS NOT physical.currency
    OR json_extract(current.after, '$.issued.control_account') IS NOT physical.control_account
    OR json_extract(current.after, '$.issued.posting_id') IS NOT physical.posting_id";

const ALLOCATION_TERMS: &str = "
    json_extract(current.after, '$.id') IS NOT physical.id
    OR json_extract(current.after, '$.document_id') IS NOT physical.document_id
    OR json_extract(current.after, '$.source_id') IS NOT physical.source_id
    OR json_extract(current.after, '$.effective_on') IS NOT physical.effective_on
    OR json_extract(current.after, '$.reversed_on') IS NOT physical.reversed_on
    OR json_extract(current.after, '$.amount_minor') IS NOT physical.amount_minor
    OR json_extract(current.after, '$.kind') IS NOT physical.kind
    OR json_extract(current.after, '$.credit_line') IS NOT physical.credit_line";

const REVIEW_TERMS: &str = "
    json_extract(current.after, '$.id') IS NOT physical.id
    OR json_extract(current.after, '$.request') IS NOT physical.request
    OR json_extract(current.after, '$.proposer') IS NOT physical.proposer
    OR json_extract(current.after, '$.status') IS NOT physical.status
    OR json_extract(current.after, '$.resolution') IS NOT physical.resolution";

fn verify_sources(reader: ReadConn<'_>) -> Result<(), CliError> {
    let invalid: Option<String> = reader.query_row(
        "SELECT doc.id FROM book_documents doc JOIN posted_transactions txn ON txn.id=doc.posting_id
         JOIN entries entry ON entry.transaction_id=txn.id AND entry.account_code=doc.control_account
         WHERE doc.issue_date IS NOT txn.date OR doc.currency IS NOT txn.currency
            OR entry.amount IS NOT json_extract(doc.issued, '$.total_minor')
            OR entry.direction IS NOT CASE json_extract(doc.issued,'$.kind')
                WHEN 'invoice' THEN 'debit' ELSE 'credit' END
         UNION ALL
         SELECT doc.id FROM book_documents doc WHERE doc.posting_id IS NOT NULL AND NOT EXISTS(
             SELECT 1 FROM posted_transactions txn JOIN entries entry ON entry.transaction_id=txn.id
             WHERE txn.id=doc.posting_id AND entry.account_code=doc.control_account)
         UNION ALL
         SELECT allocation.id FROM document_allocations allocation
         JOIN book_documents doc ON doc.company_slug=allocation.company_slug AND doc.id=allocation.document_id
         LEFT JOIN posted_transactions txn ON txn.id=allocation.source_id
         LEFT JOIN transaction_counterparties party
           ON party.company_slug=allocation.company_slug AND party.transaction_id=allocation.source_id
         WHERE doc.issue_date IS NULL OR txn.date IS NULL OR allocation.effective_on < doc.issue_date
            OR allocation.effective_on < txn.date OR txn.currency IS NOT doc.currency
            OR party.counterparty_id IS NOT doc.counterparty_id
            OR NOT EXISTS(SELECT 1 FROM entries entry
                 WHERE entry.transaction_id=allocation.source_id AND entry.account_code=doc.control_account
                   AND entry.direction=CASE json_extract(doc.issued,'$.kind')
                        WHEN 'invoice' THEN 'credit' ELSE 'debit' END)
         LIMIT 1", [], |row| row.get(0),
    ).optional()?;
    if let Some(id) = invalid {
        return Err(CliError::Database(format!(
            "open item '{id}' differs from its posted source"
        )));
    }
    Ok(())
}
