//! Rewind fixture ledgers that do not have restore history to the v37 layout.

use rusqlite::Connection;

pub fn before_restore(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TABLE payroll_funding; DROP TABLE payroll_runs;
         DROP TRIGGER bank_feed_proposal_requires_current_revision;
         DROP TABLE bank_feed_items;
         DROP TABLE bank_feed_connections;
         DROP TABLE transaction_counterparties;
         DROP TABLE counterparties;
         DROP TRIGGER protect_cash_class_update;
         ALTER TABLE accounts DROP COLUMN cash_class;",
    )?;
    let dependents = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_schema
             WHERE type IN ('view', 'trigger')
               AND name NOT IN ('bookkeeping_audit', 'protect_bookkeeping_receipt_uniqueness')
               AND instr(sql, 'bookkeeping_audit') > 0",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (kind, name, sql) in dependents {
        conn.execute_batch(&format!("DROP {kind} {name}"))?;
        conn.execute_batch(&sql.replace("bookkeeping_audit", "audit_log"))?;
    }
    conn.execute_batch(
        "DROP TRIGGER protect_bookkeeping_receipt_uniqueness;
         DROP TRIGGER protect_audit_related_posting;
         DROP VIEW bookkeeping_audit;
         DROP INDEX idx_audit_restore;
         DELETE FROM schema_version WHERE version > 37;",
    )
}
