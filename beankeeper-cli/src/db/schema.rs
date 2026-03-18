use rusqlite::{Connection, params};

use crate::error::CliError;

/// Current schema version. Bump this when adding migrations.
const CURRENT_VERSION: i64 = 5;

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

    if version < 3 {
        apply_v3(conn)?;
    }

    if version < 4 {
        apply_v4(conn)?;
    }

    if version < 5 {
        apply_v5(conn)?;
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

/// Applies the v3 migration: adds `reference` column and unique index to `transactions`.
fn apply_v3(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        ALTER TABLE transactions ADD COLUMN reference TEXT;

        CREATE UNIQUE INDEX IF NOT EXISTS idx_transactions_company_reference
            ON transactions(company_slug, reference) WHERE reference IS NOT NULL;
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![3],
    )?;

    Ok(())
}

/// Applies the v4 migration: adds `tax_category` to entries and
/// `default_tax_category` to accounts.
fn apply_v4(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        ALTER TABLE entries ADD COLUMN tax_category TEXT;
        ALTER TABLE accounts ADD COLUMN default_tax_category TEXT;
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![4],
    )?;

    Ok(())
}

/// Applies the v5 migration: adds indexes for query filtering.
fn apply_v5(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_entries_amount
            ON entries(amount);

        CREATE INDEX IF NOT EXISTS idx_entries_tax_category
            ON entries(company_slug, tax_category) WHERE tax_category IS NOT NULL;
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![5],
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory()
            .unwrap_or_else(|e| panic!("cannot open in-memory db: {e}"));
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
    fn v3_migration_adds_reference_column() {
        let conn = open_conn();
        // Apply v1 and v2 first
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(2));

        // Now run ensure_schema which should apply v3
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

        // Verify the reference column exists by querying pragma
        let has_reference: bool = conn
            .prepare("PRAGMA table_info(transactions)")
            .map(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap_or_else(|e| panic!("query failed: {e}"))
                    .filter_map(Result::ok)
                    .collect();
                names.contains(&"reference".to_string())
            })
            .unwrap_or(false);
        assert!(
            has_reference,
            "reference column should exist after v3 migration"
        );
    }

    #[test]
    fn v4_migration_adds_tax_category_columns() {
        let conn = open_conn();
        // Apply v1 through v3
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(3));

        // Now run ensure_schema which should apply v4
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

        // Verify tax_category column on entries
        let has_tax_category: bool = conn
            .prepare("PRAGMA table_info(entries)")
            .map(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap_or_else(|e| panic!("query failed: {e}"))
                    .filter_map(Result::ok)
                    .collect();
                names.contains(&"tax_category".to_string())
            })
            .unwrap_or(false);
        assert!(
            has_tax_category,
            "tax_category column should exist on entries after v4"
        );

        // Verify default_tax_category column on accounts
        let has_default_tax: bool = conn
            .prepare("PRAGMA table_info(accounts)")
            .map(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap_or_else(|e| panic!("query failed: {e}"))
                    .filter_map(Result::ok)
                    .collect();
                names.contains(&"default_tax_category".to_string())
            })
            .unwrap_or(false);
        assert!(
            has_default_tax,
            "default_tax_category column should exist on accounts after v4"
        );
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
