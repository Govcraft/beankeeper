use rusqlite::{Connection, params};

use crate::error::CliError;

/// Current schema version. Bump this when adding migrations.
const CURRENT_VERSION: i64 = 2;

/// Returns the current schema version, or `0` if the `schema_version` table
/// does not yet exist.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn get_schema_version(conn: &Connection) -> Result<i64, CliError> {
    // Check whether the schema_version table exists at all.
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        return Ok(0);
    }

    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;

    Ok(version)
}

/// Ensures the database schema is at `CURRENT_VERSION`, creating or migrating
/// tables as necessary.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn ensure_schema(conn: &Connection) -> Result<(), CliError> {
    let version = get_schema_version(conn)?;

    if version == 0 {
        apply_v1(conn)?;
    }

    if version < 2 {
        apply_v2(conn)?;
    }

    debug_assert_eq!(
        get_schema_version(conn).unwrap_or(0),
        CURRENT_VERSION,
        "schema version mismatch after migrations"
    );

    Ok(())
}

/// Applies the initial schema (version 1).
fn apply_v1(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS companies (
            slug        TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS accounts (
            company_slug TEXT NOT NULL REFERENCES companies(slug),
            code         TEXT NOT NULL,
            name         TEXT NOT NULL,
            type         TEXT NOT NULL CHECK(type IN ('asset','liability','equity','revenue','expense')),
            created_at   TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (company_slug, code)
        );

        CREATE TABLE IF NOT EXISTS transactions (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            company_slug TEXT    NOT NULL REFERENCES companies(slug),
            description  TEXT    NOT NULL,
            metadata     TEXT,
            currency     TEXT    NOT NULL DEFAULT 'USD',
            date         TEXT    NOT NULL,
            posted_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS entries (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            transaction_id INTEGER NOT NULL REFERENCES transactions(id),
            account_code   TEXT    NOT NULL,
            company_slug   TEXT    NOT NULL,
            direction      TEXT    NOT NULL CHECK(direction IN ('debit','credit')),
            amount         INTEGER NOT NULL CHECK(amount > 0),
            memo           TEXT,
            FOREIGN KEY (company_slug, account_code) REFERENCES accounts(company_slug, code)
        );

        CREATE INDEX IF NOT EXISTS idx_entries_transaction_id
            ON entries(transaction_id);

        CREATE INDEX IF NOT EXISTS idx_entries_company_account
            ON entries(company_slug, account_code);

        CREATE INDEX IF NOT EXISTS idx_transactions_company
            ON transactions(company_slug);

        CREATE INDEX IF NOT EXISTS idx_transactions_company_date
            ON transactions(company_slug, date);
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![1],
    )?;

    Ok(())
}

/// Applies the v2 migration: adds the `attachments` table.
fn apply_v2(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS attachments (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            transaction_id  INTEGER NOT NULL REFERENCES transactions(id),
            entry_id        INTEGER REFERENCES entries(id),
            company_slug    TEXT NOT NULL,
            uri             TEXT NOT NULL,
            document_type   TEXT NOT NULL CHECK(document_type IN ('receipt','invoice','statement','contract','other')),
            hash            TEXT,
            original_filename TEXT,
            attached_at     TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (company_slug) REFERENCES companies(slug)
        );

        CREATE INDEX IF NOT EXISTS idx_attachments_transaction
            ON attachments(transaction_id);
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![2],
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_conn() -> Connection {
        let conn =
            Connection::open_in_memory().unwrap_or_else(|e| panic!("cannot open in-memory db: {e}"));
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn
    }

    #[test]
    fn fresh_db_has_version_zero() {
        let conn = open_conn();
        let v = get_schema_version(&conn);
        assert_eq!(v.ok(), Some(0));
    }

    #[test]
    fn ensure_schema_creates_tables() {
        let conn = open_conn();
        let result = ensure_schema(&conn);
        assert!(result.is_ok());
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        assert!(ensure_schema(&conn).is_ok());
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));
    }

    #[test]
    fn companies_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'companies'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn accounts_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'accounts'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn transactions_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'transactions'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn entries_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn attachments_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'attachments'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn v2_migration_from_v1() {
        let conn = open_conn();
        // Apply only v1
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(1));

        // Now run ensure_schema which should apply v2
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

        // Verify attachments table exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'attachments'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }
}
