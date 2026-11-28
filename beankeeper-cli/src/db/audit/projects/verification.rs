//! Reconcile current project tables with the active audit history.

use rusqlite::OptionalExtension;

use crate::{db::connection::ReadConn, error::CliError};

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    for (entity, table, key, extra) in [
        (
            "project",
            "projects",
            "p.id",
            "p.data IS NOT a.after OR p.id IS NOT json_extract(p.data,'$.id')
             OR p.code IS NOT json_extract(p.data,'$.code')
             OR p.customer_id IS NOT json_extract(p.data,'$.customer')
             OR p.archived IS NOT json_extract(p.data,'$.archived')",
        ),
        (
            "project_work",
            "project_time_entries",
            "p.id",
            "p.data IS NOT a.after OR p.id IS NOT json_extract(p.data,'$.id')
             OR p.project_id IS NOT json_extract(p.data,'$.project')
             OR p.worked_on IS NOT json_extract(p.data,'$.date')
             OR p.approved_on IS NOT json_extract(p.data,'$.approved_on')",
        ),
        (
            "project_distribution",
            "project_distributions",
            "p.entry_id||'/'||p.effective_on",
            "p.data IS NOT a.after OR p.entry_id IS NOT json_extract(p.data,'$.entry_id')
             OR p.effective_on IS NOT json_extract(p.data,'$.effective_on')",
        ),
        (
            "project_budget",
            "project_expense_budgets",
            "p.project_id||'/'||substr(p.month_start,1,7)",
            "p.project_id IS NOT json_extract(a.after,'$.project')
             OR p.amount_minor IS NOT json_extract(a.after,'$.amount_minor')
             OR p.month_start IS NOT json_extract(a.after,'$.month')||'-01'",
        ),
        (
            "project_invoice",
            "project_invoice_drafts",
            "p.document_id",
            "p.data IS NOT a.after OR p.document_id IS NOT json_extract(p.data,'$.document_id')
             OR p.project_id IS NOT json_extract(p.data,'$.project_id')
             OR p.cancelled IS NOT json_extract(p.data,'$.cancelled')",
        ),
    ] {
        projection(reader, entity, table, key, extra)?;
    }
    matches_snapshot(reader, SPLITS_SQL, [], "project distribution")?;
    matches_snapshot(reader, SOURCES_SQL, [], "project invoice source")?;
    matches_snapshot(reader, POSTING_SQL, [], "project billing")
}

fn projection(
    reader: ReadConn<'_>,
    entity: &str,
    table: &str,
    key: &str,
    extra: &str,
) -> Result<(), CliError> {
    let sql = format!(
        "WITH latest AS (
           SELECT event.* FROM bookkeeping_audit event JOIN (
             SELECT MAX(id) id FROM bookkeeping_audit WHERE entity=?1 GROUP BY company_slug,entity_id
           ) active ON active.id=event.id
         )
         SELECT COALESCE({key},a.entity_id) FROM {table} p FULL OUTER JOIN latest a
           ON p.company_slug=a.company_slug AND {key}=a.entity_id
         WHERE p.receipt IS NULL OR a.id IS NULL OR p.receipt IS NOT a.id OR ({extra}) LIMIT 1"
    );
    matches_snapshot(reader, &sql, [entity], entity)
}

fn matches_snapshot(
    reader: ReadConn<'_>,
    sql: &str,
    params: impl rusqlite::Params,
    entity: &str,
) -> Result<(), CliError> {
    let invalid: Option<String> = reader.query_row(sql, params, |row| row.get(0)).optional()?;
    if let Some(id) = invalid {
        return Err(CliError::Database(format!(
            "{entity} '{id}' differs from its audited projection"
        )));
    }
    Ok(())
}

const SPLITS_SQL: &str = "
    WITH snapshots AS (
       SELECT d.company_slug,d.entry_id,d.effective_on,part.value data
       FROM project_distributions d, json_each(d.data,'$.splits') part
    )
    SELECT COALESCE(s.id,json_extract(a.data,'$.id'))
    FROM project_splits s FULL OUTER JOIN snapshots a
      ON a.company_slug=s.company_slug AND json_extract(a.data,'$.id')=s.id
    WHERE s.id IS NULL OR a.data IS NULL OR s.entry_id IS NOT a.entry_id
       OR s.effective_on IS NOT a.effective_on OR s.project_id IS NOT json_extract(a.data,'$.project')
       OR s.amount_minor IS NOT json_extract(a.data,'$.amount_minor')
       OR s.billable IS NOT (json_extract(a.data,'$.billable')='billable')
    UNION ALL
    SELECT CAST(d.entry_id AS TEXT) FROM project_distributions d JOIN entries e
       ON e.company_slug=d.company_slug AND e.id=d.entry_id
    WHERE e.transaction_id IS NOT json_extract(d.data,'$.transaction_id')
       OR e.amount IS NOT json_extract(d.data,'$.unallocated_minor') + COALESCE((
           SELECT SUM(s.amount_minor) FROM project_splits s WHERE s.company_slug=d.company_slug
           AND s.entry_id=d.entry_id AND s.effective_on=d.effective_on
       ),0)
    LIMIT 1";

const SOURCES_SQL: &str = "
    WITH snapshots AS (
      SELECT d.company_slug,d.document_id,line.key+1 line_number,line.value data
      FROM project_invoice_drafts d, json_each(d.data,'$.lines') line
    )
    SELECT COALESCE(s.document_id,a.document_id)
    FROM project_invoice_sources s FULL OUTER JOIN snapshots a
      ON s.company_slug=a.company_slug AND s.document_id=a.document_id AND s.line_number=a.line_number
    WHERE s.document_id IS NULL OR a.data IS NULL
       OR COALESCE(s.time_id,s.split_id) IS NOT json_extract(a.data,'$.source.id')
       OR (CASE WHEN s.time_id IS NULL THEN 'expense' ELSE 'time' END) IS NOT json_extract(a.data,'$.source.kind')
       OR s.expected_minor IS NOT json_extract(a.data,'$.amount_minor')
       OR s.claimed_on IS NOT json_extract(a.data,'$.claimed_on')
       OR s.released_on IS NOT json_extract(a.data,'$.released_on')
    LIMIT 1";

const POSTING_SQL: &str = "
    SELECT s.document_id
    FROM project_invoice_sources s
    JOIN project_invoice_drafts draft USING(company_slug,document_id)
    JOIN book_documents doc ON doc.company_slug=s.company_slug AND doc.id=s.document_id
    JOIN projects job ON job.company_slug=draft.company_slug AND job.id=draft.project_id
    LEFT JOIN project_time_entries work ON work.company_slug=s.company_slug AND work.id=s.time_id
    LEFT JOIN project_splits expense ON expense.company_slug=s.company_slug AND expense.id=s.split_id
    WHERE s.claimed_on IS NOT doc.issue_date
      OR doc.draft IS NOT json_extract(draft.data,'$.terms')
      OR (doc.issue_date IS NOT NULL AND (draft.cancelled=1 OR doc.currency IS NOT json_extract(job.data,'$.currency')))
      OR COALESCE(work.project_id,expense.project_id) IS NOT draft.project_id
      OR (s.claimed_on IS NOT NULL AND s.time_id IS NOT NULL
          AND (work.approved_on IS NULL OR work.approved_on>s.claimed_on))
      OR (s.split_id IS NOT NULL AND (expense.billable!=1 OR expense.amount_minor IS NOT s.expected_minor))
      OR s.released_on IS NOT (
        SELECT CASE WHEN SUM(credit.amount_minor)=COALESCE((
            SELECT json_extract(line.value,'$.gross_minor') FROM json_each(doc.issued,'$.vat') line
            WHERE json_extract(line.value,'$.number')=s.line_number
        ),s.expected_minor) THEN MAX(credit.effective_on) END
        FROM document_allocations credit WHERE credit.company_slug=s.company_slug
           AND credit.document_id=s.document_id AND credit.credit_line=s.line_number
      )
    LIMIT 1";
