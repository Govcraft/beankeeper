use std::fmt;

use rusqlite::{Connection, ToSql, Transaction, TransactionBehavior, params, types::ToSqlOutput};

use super::connection::ReadConn;
use super::metadata;
use super::snapshot::VerifiedSnapshot;
use crate::error::CliError;

const INITIAL_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(1);
const RESTORE_VERSION_NUMBER: i64 = 38;
const LATEST_VERSION: i64 = 40;

pub(super) const AUDIT_LOG_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(8);
pub(super) const NORMALIZED_METADATA_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(9);
pub(super) const HASH_CHAIN_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(12);
pub(crate) const IDENTITY_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(13);
pub(crate) const POLICY_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(18);
pub(super) const PROPOSAL_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(19);
pub(super) const PERIOD_LOCK_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(20);

/// Current schema version. Bump this when adding migrations.
pub(crate) const CURRENT_VERSION: SchemaVersion = SchemaVersion::known(LATEST_VERSION);
pub(crate) const BOOK_PROFILE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(22);
pub(crate) const STATEMENT_BATCH_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(23);
pub(crate) const CLIENT_HINT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(25);
pub(crate) const RECEIPT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(26);
pub(crate) const STANDING_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(27);
const RULE_UNDO_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(28);
const DIRECT_SETUP_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(29);
const ANNOTATION_UNDO_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(30);
pub(super) const HOLDS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(31);
pub(crate) const ACCOUNT_RENAME_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(32);
const CORRECTION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(33);
const RECATEGORIZATION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(34);
pub(super) const TXN_LINK_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(35);
pub(crate) const STATEMENT_RECONCILIATION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(36);
pub(super) const FX_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(37);
pub(super) const RESTORE_SCHEMA_VERSION: SchemaVersion =
    SchemaVersion::known(RESTORE_VERSION_NUMBER);
pub(crate) const CASH_CLASS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(39);
pub(super) const COUNTERPARTY_SCHEMA_VERSION: SchemaVersion = SchemaVersion::known(40);
/// Earliest ledger that supports the read-only setup operations.
pub(crate) const OLDEST_SETUP_READ_SCHEMA: SchemaVersion = SchemaVersion::known(20);

/// A ledger schema version supported by this Beankeeper build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct SchemaVersion(i64);

impl SchemaVersion {
    const fn is_supported(version: i64) -> bool {
        version >= 1 && version <= LATEST_VERSION
    }

    const fn known(version: i64) -> Self {
        assert!(Self::is_supported(version));
        Self(version)
    }

    pub(super) fn require_at_least(self, expected: Self) -> Result<Self, CliError> {
        if self < expected {
            return Err(CliError::SchemaMismatch {
                stored: self,
                expected,
            });
        }
        Ok(self)
    }
}

impl TryFrom<i64> for SchemaVersion {
    type Error = CliError;

    fn try_from(version: i64) -> Result<Self, Self::Error> {
        match version {
            _ if Self::is_supported(version) => Ok(Self(version)),
            0 => Err(CliError::Database(
                "file is not an initialized beankeeper ledger".into(),
            )),
            _ => Err(CliError::Database(format!(
                "unsupported beankeeper schema version {version}; expected 1 through {CURRENT_VERSION}"
            ))),
        }
    }
}

impl From<SchemaVersion> for i64 {
    fn from(version: SchemaVersion) -> Self {
        version.0
    }
}

impl ToSql for SchemaVersion {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The relation that contains bookkeeping evidence for a schema.
///
/// Before restores, every physical event belongs to the only history.
#[derive(Clone, Copy)]
pub(super) enum AuditHistory {
    Physical,
    Bookkeeping,
}

impl From<SchemaVersion> for AuditHistory {
    fn from(version: SchemaVersion) -> Self {
        if version >= RESTORE_SCHEMA_VERSION {
            Self::Bookkeeping
        } else {
            Self::Physical
        }
    }
}

impl fmt::Display for AuditHistory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Physical => "audit_log",
            Self::Bookkeeping => "bookkeeping_audit",
        })
    }
}

/// Returns the schema version expected by [`Db::open_snapshot`](super::Db::open_snapshot).
///
/// This is a property of the linked build and requires no database connection.
/// Use [`get_schema_version`] to read the version stored in an open ledger.
#[must_use]
pub const fn current_schema_version() -> SchemaVersion {
    CURRENT_VERSION
}

/// Returns the current schema version, or `0` if the `schema_version` table
/// does not yet exist.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn get_schema_version(reader: ReadConn<'_>) -> Result<i64, CliError> {
    // Check whether the schema_version table exists at all.
    let table_exists: bool = reader.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        return Ok(0);
    }

    let version: i64 = reader.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;

    Ok(version)
}

/// Read and validate the SQLite schema version of an initialized ledger.
pub(super) fn stored_schema(reader: ReadConn<'_>) -> Result<SchemaVersion, CliError> {
    get_schema_version(reader)?.try_into()
}

/// A missing schema is represented separately from supported ledger versions.
pub(super) fn read_schema(reader: ReadConn<'_>) -> Result<Option<SchemaVersion>, CliError> {
    match get_schema_version(reader)? {
        0 => Ok(None),
        version => SchemaVersion::try_from(version).map(Some),
    }
}

#[cfg(test)]
fn ensure_schema(conn: &Connection) -> Result<(), CliError> {
    match read_schema(ReadConn::physical(conn))? {
        Some(version) => ensure_schema_at(conn, version),
        None => create_schema(conn),
    }
}

fn create_schema(conn: &Connection) -> Result<(), CliError> {
    apply_v1(conn)?;
    ensure_schema_at(conn, INITIAL_SCHEMA_VERSION)
}

/// Initialize an empty database while holding SQLite's writer reservation.
pub(super) fn initialize(conn: &Connection) -> Result<(), CliError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let reader = ReadConn::physical(&tx);
    if let Some(version) = read_schema(reader)? {
        version.require_at_least(current_schema_version())?;
    } else {
        let empty: bool = reader.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM sqlite_schema)",
            [],
            |row| row.get(0),
        )?;
        if !empty {
            return Err(CliError::Database(
                "file is not an initialized beankeeper ledger".into(),
            ));
        }
        create_schema(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

/// Apply migrations only with proof validated under the caller's writer reservation.
pub(super) fn migrate_verified(
    tx: Transaction<'_>,
    snapshot: VerifiedSnapshot,
) -> Result<(), CliError> {
    let version = snapshot.verify_migration_source(&tx)?;
    ensure_schema_at(&tx, version)?;
    tx.commit()?;
    drop(snapshot);
    Ok(())
}

fn ensure_schema_at(conn: &Connection, version: SchemaVersion) -> Result<(), CliError> {
    if version < SchemaVersion::known(2) {
        apply_v2(conn)?;
    }

    if version < SchemaVersion::known(3) {
        apply_v3(conn)?;
    }

    if version < SchemaVersion::known(4) {
        apply_v4(conn)?;
    }

    if version < SchemaVersion::known(5) {
        apply_v5(conn)?;
    }

    if version < SchemaVersion::known(6) {
        apply_v6(conn)?;
    }

    if version < SchemaVersion::known(7) {
        apply_v7(conn)?;
    }

    if version < AUDIT_LOG_SCHEMA_VERSION {
        apply_v8(conn)?;
    }

    if version < NORMALIZED_METADATA_SCHEMA_VERSION {
        apply_v9(conn)?;
    }

    if version < SchemaVersion::known(10) {
        apply_v10(conn)?;
    }

    if version < SchemaVersion::known(11) {
        apply_v11(conn)?;
    }

    if version < HASH_CHAIN_SCHEMA_VERSION {
        apply_v12(conn)?;
    }

    if version < IDENTITY_SCHEMA_VERSION {
        apply_v13(conn)?;
    }

    if version < SchemaVersion::known(14) {
        apply_v14(conn)?;
    }

    if version < SchemaVersion::known(15) {
        apply_v15(conn)?;
    }

    if version < SchemaVersion::known(16) {
        apply_v16(conn)?;
    }

    if version < SchemaVersion::known(17) {
        apply_v17(conn)?;
    }

    if version < POLICY_SCHEMA_VERSION {
        apply_v18(conn)?;
    }

    if version < PROPOSAL_SCHEMA_VERSION {
        apply_v19(conn)?;
    }

    if version < PERIOD_LOCK_SCHEMA_VERSION {
        apply_v20(conn)?;
    }
    if version < SchemaVersion::known(21) {
        apply_v21(conn)?;
    }
    if version < BOOK_PROFILE_SCHEMA_VERSION {
        apply_v22(conn)?;
    }
    if version < STATEMENT_BATCH_SCHEMA_VERSION {
        apply_v23(conn)?;
    }
    if version < SchemaVersion::known(24) {
        apply_v24(conn)?;
    }
    if version < CLIENT_HINT_SCHEMA_VERSION {
        apply_v25(conn)?;
    }
    apply_receipt_migrations(conn, version)?;
    apply_hold_audit_migrations(conn, version)?;
    if version < FX_SCHEMA_VERSION {
        apply_fx_migration(conn)?;
    }

    if version < RESTORE_SCHEMA_VERSION {
        apply_restore_migration(conn)?;
    }

    if version < CASH_CLASS_SCHEMA_VERSION {
        migrate_with_writer(conn, CASH_CLASS_SCHEMA_VERSION, apply_cash_class_schema)?;
    }

    if version < COUNTERPARTY_SCHEMA_VERSION {
        migrate_with_writer(conn, COUNTERPARTY_SCHEMA_VERSION, apply_counterparty_schema)?;
    }

    debug_assert_eq!(
        stored_schema(ReadConn::physical(conn)).ok(),
        Some(CURRENT_VERSION),
        "schema version mismatch after migrations"
    );

    Ok(())
}

fn apply_counterparty_schema(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.pragma_update(None, "defer_foreign_keys", true)?;
        conn.execute_batch("DROP TRIGGER protect_cash_class_update; DROP VIEW bookkeeping_audit;")?;
        migrate_hold_audit(
            conn,
            AuditTable::Counterparties,
            COUNTERPARTY_SCHEMA_VERSION,
        )?;
        install_cash_class_guard(conn)?;
        conn.execute_batch(include_str!("schema/counterparties.sql"))?;
        super::verification::verify_foreign_keys(ReadConn::physical(conn))?;
        conn.pragma_update(None, "defer_foreign_keys", false)?;
        Ok(())
    })
}

fn apply_cash_class_schema(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.pragma_update(None, "defer_foreign_keys", true)?;
        // This is the only name inference: preserve old reports once at upgrade.
        // Thereafter the stored classification is authoritative, even after rename.
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN cash_class TEXT NOT NULL DEFAULT 'non_cash'
            CHECK(cash_class IN ('non_cash', 'cash', 'cash_equivalent'))
            CHECK(cash_class = 'non_cash' OR type = 'asset');
            UPDATE accounts SET cash_class = 'cash' WHERE type = 'asset' AND (
                lower(name) LIKE '%cash%' OR lower(name) LIKE '%bank%'
                OR lower(name) LIKE '%checking%' OR lower(name) LIKE '%savings%');",
        )?;
        // Rebuild dependent views around the audit vocabulary extension.
        conn.execute_batch("DROP VIEW bookkeeping_audit;")?;
        migrate_hold_audit(
            conn,
            AuditTable::CashClassification,
            CASH_CLASS_SCHEMA_VERSION,
        )?;
        install_cash_class_guard(conn)?;
        super::verification::verify_foreign_keys(ReadConn::physical(conn))?;
        conn.pragma_update(None, "defer_foreign_keys", false)?;
        Ok(())
    })
}

fn install_cash_class_guard(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch("CREATE TRIGGER protect_cash_class_update BEFORE UPDATE OF cash_class ON accounts
            WHEN NEW.cash_class <> OLD.cash_class AND NOT EXISTS (
                SELECT 1 FROM bookkeeping_audit event WHERE event.id = (SELECT MAX(id) FROM audit_log)
                AND event.company_slug = NEW.company_slug AND event.entity = 'account'
                AND event.entity_id = NEW.code AND event.action = 'account_classify'
                AND json_extract(event.before, '$.cash_class') IS OLD.cash_class
                AND json_extract(event.after, '$.cash_class') IS NEW.cash_class)
            BEGIN SELECT RAISE(ABORT, 'cash classification requires an audit receipt'); END;")?;
    Ok(())
}

fn apply_restore_migration(conn: &Connection) -> Result<(), CliError> {
    migrate_with_writer(conn, RESTORE_SCHEMA_VERSION, apply_restore_schema)
}

fn apply_restore_schema(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        // The statement tables keep their receipt IDs across the audit table
        // rebuild. Verify the repaired foreign keys before clearing SQLite's
        // deferred violations from dropping the original parent table.
        conn.pragma_update(None, "defer_foreign_keys", true)?;
        migrate_hold_audit(conn, AuditTable::LedgerRestore, RESTORE_SCHEMA_VERSION)?;
        super::verification::verify_foreign_keys(ReadConn::physical(conn))?;
        conn.pragma_update(None, "defer_foreign_keys", false)?;
        Ok(())
    })
}

fn apply_fx_migration(conn: &Connection) -> Result<(), CliError> {
    migrate_with_writer(conn, FX_SCHEMA_VERSION, apply_fx_schema)
}

fn migrate_with_writer(
    conn: &Connection,
    version: SchemaVersion,
    apply: impl FnOnce(&Connection) -> Result<(), CliError>,
) -> Result<(), CliError> {
    if !conn.is_autocommit() {
        return apply(conn);
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if stored_schema(ReadConn::physical(&tx))? < version {
        apply(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

fn apply_fx_schema(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(include_str!("schema/fx.sql"))?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [FX_SCHEMA_VERSION],
    )?;
    Ok(())
}

fn apply_hold_audit_migrations(conn: &Connection, version: SchemaVersion) -> Result<(), CliError> {
    if version < HOLDS_SCHEMA_VERSION {
        apply_v31(conn)?;
    }
    if version < ACCOUNT_RENAME_SCHEMA_VERSION {
        apply_v32(conn)?;
    }
    if version < CORRECTION_SCHEMA_VERSION {
        migrate_hold_audit(conn, AuditTable::Correction, CORRECTION_SCHEMA_VERSION)?;
    }

    if version < RECATEGORIZATION_SCHEMA_VERSION {
        migrate_hold_audit(
            conn,
            AuditTable::Recategorization,
            RECATEGORIZATION_SCHEMA_VERSION,
        )?;
    }
    if version < TXN_LINK_SCHEMA_VERSION {
        apply_v35(conn)?;
    }
    if version < STATEMENT_RECONCILIATION_SCHEMA_VERSION {
        super::audit::atomically(conn, || {
            migrate_hold_audit(
                conn,
                AuditTable::StatementReconciliation,
                STATEMENT_RECONCILIATION_SCHEMA_VERSION,
            )?;
            conn.execute_batch(include_str!("schema/statement_reconciliation.sql"))?;
            Ok(())
        })?;
    }

    Ok(())
}

fn apply_receipt_migrations(conn: &Connection, version: SchemaVersion) -> Result<(), CliError> {
    if version < RECEIPT_SCHEMA_VERSION {
        apply_v26(conn)?;
    }
    if version < STANDING_SETTINGS_SCHEMA_VERSION {
        apply_v27(conn)?;
    }
    if version < RULE_UNDO_SCHEMA_VERSION {
        apply_v28(conn)?;
    }
    if version < DIRECT_SETUP_SCHEMA_VERSION {
        apply_v29(conn)?;
    }
    if version < ANNOTATION_UNDO_SCHEMA_VERSION {
        apply_v30(conn)?;
    }

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

/// Applies the v6 migration: adds `status` column to `entries` for reconciliation.
fn apply_v6(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        ALTER TABLE entries ADD COLUMN status TEXT NOT NULL DEFAULT 'uncleared' CHECK(status IN ('uncleared', 'cleared', 'reconciled'));

        CREATE INDEX IF NOT EXISTS idx_entries_status
            ON entries(company_slug, account_code, status);
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![6],
    )?;

    Ok(())
}

/// Applies the v7 migration: adds `budgets` table for budgeting and variance analysis.
fn apply_v7(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS budgets (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            company_slug TEXT    NOT NULL REFERENCES companies(slug),
            account_code TEXT    NOT NULL,
            currency     TEXT    NOT NULL DEFAULT 'USD',
            year         INTEGER NOT NULL,
            month        INTEGER NOT NULL CHECK(month >= 1 AND month <= 12),
            amount       INTEGER NOT NULL CHECK(amount >= 0),
            notes        TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (company_slug, account_code) REFERENCES accounts(company_slug, code),
            UNIQUE (company_slug, account_code, currency, year, month)
        );

        CREATE INDEX IF NOT EXISTS idx_budgets_company_year
            ON budgets(company_slug, year);

        CREATE INDEX IF NOT EXISTS idx_budgets_company_account
            ON budgets(company_slug, account_code);
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![7],
    )?;

    Ok(())
}

/// Applies the v8 migration: adds the append-only `audit_log` table.
///
/// Every in-place mutation or deletion in the ledger (clearance-status
/// changes, intercompany correlation, budget revisions, and account/company
/// deletions) appends one immutable row here, recording who changed what and
/// when, with JSON before/after snapshots. This closes the audit gap where
/// such transitions previously left no trace (see issue #16).
fn apply_v8(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS audit_log (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            changed_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            actor        TEXT    NOT NULL,
            company_slug TEXT,
            entity       TEXT    NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company')),
            entity_id    TEXT    NOT NULL,
            action       TEXT    NOT NULL CHECK(action IN ('status_change','correlate','budget_set','budget_delete','account_delete','company_delete')),
            before       TEXT,
            after        TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_audit_entity
            ON audit_log(entity, entity_id);

        CREATE INDEX IF NOT EXISTS idx_audit_company_time
            ON audit_log(company_slug, changed_at);

        CREATE INDEX IF NOT EXISTS idx_audit_action
            ON audit_log(action);
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![8],
    )?;

    Ok(())
}

/// Applies the v9 migration: normalizes `transactions.metadata` so it is
/// always `NULL` or a JSON object (issue #17).
///
/// `txn post -m` used to store free text verbatim, which made the reconcile
/// query's `json_extract` fail with "malformed JSON". Every existing value that
/// is not valid JSON, or is valid JSON but not an object, is rewritten with
/// the same [`metadata::normalize`] rule new writes use: free text becomes
/// `{"memo": "<text>"}`, empty text becomes `NULL`, objects are untouched.
///
/// This one-off rewrite is intentionally *not* recorded in `audit_log`. It is
/// a schema-level data-format migration, not a ledger mutation by an actor:
/// the information content is preserved verbatim under `memo`, and there are
/// no deployed databases holding such rows to protect. Recording it would
/// require a new audit action kind and a rebuild of the `audit_log.action`
/// CHECK constraint for no practical benefit.
fn apply_v9(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch("SAVEPOINT apply_v9")?;
    let result = (|| -> Result<(), CliError> {
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, metadata FROM transactions \
                 WHERE metadata IS NOT NULL \
                   AND (NOT json_valid(metadata) OR json_type(metadata) <> 'object')",
            )?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?
        };

        for (id, raw) in rows {
            conn.execute(
                "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
                params![
                    metadata::normalize(&raw).map(metadata::NormalizedMetadata::into_string),
                    id
                ],
            )?;
        }

        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![9],
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE apply_v9")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO apply_v9; RELEASE apply_v9");
            Err(e)
        }
    }
}

/// Applies the v10 migration: adds the `entry_clearings` table (issue #25).
///
/// An entry can be cleared in several parts, on different dates, by
/// different statements. `entries.status` stays as a derived summary of the
/// entry's clearings (see `db::clearings::derive_status`).
///
/// A reconciled clearing carries a `reconciled_date` (required, and on or
/// after its `cleared_date`); a cleared one has none. CHECK constraints
/// enforce both.
///
/// Backfill: every entry that is already `cleared` or `reconciled` gets one
/// clearing row for its full amount, with that status, dated on its
/// transaction date (both `cleared_date` and, when reconciled,
/// `reconciled_date`) and with no statement reference.
///
/// Self-contained: it only depends on tables from v1-v6, so it can be
/// renumbered by changing the version literal below and the call site.
fn apply_v10(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "
        SAVEPOINT sp_migrate_entry_clearings;

        CREATE TABLE IF NOT EXISTS entry_clearings (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id      INTEGER NOT NULL REFERENCES entries(id),
            company_slug  TEXT    NOT NULL REFERENCES companies(slug),
            amount        INTEGER NOT NULL CHECK(amount > 0),
            cleared_date  TEXT    NOT NULL,
            statement_ref TEXT,
            status        TEXT    NOT NULL DEFAULT 'cleared' CHECK(status IN ('cleared', 'reconciled')),
            reconciled_date TEXT,
            created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
            -- A reconciled clearing has a reconciled date; a cleared one has none.
            CHECK ((status = 'reconciled') = (reconciled_date IS NOT NULL)),
            CHECK (reconciled_date IS NULL OR reconciled_date >= cleared_date)
        );

        CREATE INDEX IF NOT EXISTS idx_entry_clearings_entry
            ON entry_clearings(entry_id);

        CREATE INDEX IF NOT EXISTS idx_entry_clearings_company_statement
            ON entry_clearings(company_slug, statement_ref);

        -- Idempotency key: at most one clearing per (entry, statement).
        CREATE UNIQUE INDEX IF NOT EXISTS idx_entry_clearings_entry_statement
            ON entry_clearings(entry_id, statement_ref) WHERE statement_ref IS NOT NULL;

        INSERT INTO entry_clearings
            (entry_id, company_slug, amount, cleared_date, status, reconciled_date)
        SELECT e.id, e.company_slug, e.amount, t.date, e.status,
               CASE WHEN e.status = 'reconciled' THEN t.date END
        FROM entries e
        JOIN transactions t ON t.id = e.transaction_id
        WHERE e.status IN ('cleared', 'reconciled')
          AND NOT EXISTS (SELECT 1 FROM entry_clearings c WHERE c.entry_id = e.id);

        INSERT INTO schema_version (version) VALUES (10);

        RELEASE sp_migrate_entry_clearings;
        ",
    )?;

    Ok(())
}

/// Extends the audit action constraint while preserving all existing records.
fn apply_v11(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(
            "CREATE TABLE audit_log_v11 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete')),
                before TEXT,
                after TEXT
            );
            INSERT INTO audit_log_v11 SELECT * FROM audit_log;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v11 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            INSERT INTO schema_version (version) VALUES (11);"
        )?;
        Ok(())
    })
}

/// Establishes a chain over historical records without inventing their actors.
/// All schema changes, backfill, and protections commit together.
fn apply_v12(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(
            "ALTER TABLE transactions ADD COLUMN posted_by TEXT;
            CREATE TABLE audit_log_v12 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64)
            );
            INSERT INTO audit_log_v12
                SELECT *, printf('%064d', 0), printf('%064d', 0) FROM audit_log ORDER BY id;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v12 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);"
        )?;
        super::audit::backfill_hash_chain(conn)?;
        conn.execute_batch(
            "CREATE TRIGGER protect_transactions_insert BEFORE INSERT ON transactions
            WHEN EXISTS(SELECT 1 FROM transactions WHERE id = NEW.id
                OR (company_slug = NEW.company_slug AND reference = NEW.reference))
            BEGIN SELECT RAISE(ABORT, 'posted transactions cannot be replaced'); END;
            CREATE TRIGGER protect_entries_insert BEFORE INSERT ON entries
            WHEN EXISTS(SELECT 1 FROM entries WHERE id = NEW.id)
            BEGIN SELECT RAISE(ABORT, 'posted entries cannot be replaced'); END;
            CREATE TRIGGER protect_transactions_update BEFORE UPDATE ON transactions
            WHEN NEW.id IS NOT OLD.id OR NEW.company_slug IS NOT OLD.company_slug
              OR NEW.description IS NOT OLD.description OR NEW.currency IS NOT OLD.currency
              OR NEW.date IS NOT OLD.date OR NEW.posted_at IS NOT OLD.posted_at
              OR NEW.reference IS NOT OLD.reference OR NEW.posted_by IS NOT OLD.posted_by
            BEGIN SELECT RAISE(ABORT, 'posted transaction fields are immutable'); END;
            CREATE TRIGGER protect_transactions_delete BEFORE DELETE ON transactions
            BEGIN SELECT RAISE(ABORT, 'posted transactions cannot be deleted'); END;
            CREATE TRIGGER protect_entries_update BEFORE UPDATE ON entries
            WHEN NEW.id IS NOT OLD.id OR NEW.transaction_id IS NOT OLD.transaction_id
              OR NEW.account_code IS NOT OLD.account_code OR NEW.company_slug IS NOT OLD.company_slug
              OR NEW.direction IS NOT OLD.direction OR NEW.amount IS NOT OLD.amount
              OR NEW.memo IS NOT OLD.memo OR NEW.tax_category IS NOT OLD.tax_category
            BEGIN SELECT RAISE(ABORT, 'posted entry fields are immutable'); END;
            CREATE TRIGGER protect_entries_delete BEFORE DELETE ON entries
            BEGIN SELECT RAISE(ABORT, 'posted entries cannot be deleted'); END;
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
            INSERT INTO schema_version (version) VALUES (12);"
        )?;
        Ok(())
    })
}

/// Preserves v1 hashes and snapshots; all later records use the v2 payload.
fn apply_v13(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        let tail_valid: bool = conn.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                = COALESCE((SELECT MAX(id) FROM audit_log), 0)",
            [],
            |r| r.get(0),
        )?;
        if !tail_valid {
            return Err(CliError::Database(
                "audit hash chain tail missing before v13 migration".into(),
            ));
        }
        conn.execute_batch(
            "ALTER TABLE transactions ADD COLUMN agent_uri TEXT;
            ALTER TABLE transactions ADD COLUMN token_jti TEXT;
            CREATE TABLE identity_root (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                kid TEXT NOT NULL UNIQUE,
                trust_root TEXT NOT NULL UNIQUE,
                private_key BLOB NOT NULL CHECK(length(private_key) = 32),
                public_key TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE agent_tokens (
                jti TEXT PRIMARY KEY,
                agent_uri TEXT NOT NULL,
                agent_key TEXT NOT NULL,
                capabilities TEXT NOT NULL,
                issued_at TEXT NOT NULL,
                expires_at TEXT NOT NULL
            );
            CREATE INDEX idx_agent_tokens_uri ON agent_tokens(agent_uri);
            CREATE TABLE agent_revocations (
                jti TEXT PRIMARY KEY REFERENCES agent_tokens(jti),
                revoked_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE audit_log_v13 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 2 CHECK(hash_version IN (1,2)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v13
                SELECT *, NULL, NULL, 1 FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v13 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 2
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
            CREATE TRIGGER protect_transaction_identity BEFORE UPDATE ON transactions
            WHEN NEW.agent_uri IS NOT OLD.agent_uri OR NEW.token_jti IS NOT OLD.token_jti
            BEGIN SELECT RAISE(ABORT, 'posted transaction identity is immutable'); END;
            INSERT INTO schema_version (version) VALUES (13);"
        )?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

/// Add bank categorization rules without changing any historical audit hash.
fn apply_v14(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        conn.execute_batch(
            "CREATE TABLE categorization_rules (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL REFERENCES companies(slug),
                contains TEXT NOT NULL CHECK(length(trim(contains)) > 0),
                account_code TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (company_slug, account_code) REFERENCES accounts(company_slug, code)
            );
            CREATE INDEX idx_rules_company_priority
                ON categorization_rules(company_slug, priority DESC, id ASC);
            CREATE TABLE audit_log_v14 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 2 CHECK(hash_version IN (1,2)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v14 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v14 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 2
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
            INSERT INTO schema_version (version) VALUES (14);"
        )?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

/// Allow audited annotation actions while preserving historical hashes and triggers.
fn apply_v15(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        conn.execute_batch(
            "CREATE TABLE audit_log_v15 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 2 CHECK(hash_version IN (1,2)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v15 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v15 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 2
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
            INSERT INTO schema_version (version) VALUES (15);"
        )?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

/// Store pending schedules and extend auditing without changing historical hashes.
fn apply_v16(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        conn.execute_batch(
            "CREATE TABLE schedules (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL REFERENCES companies(slug),
                description TEXT NOT NULL CHECK(length(trim(description)) > 0),
                currency TEXT NOT NULL,
                start_date TEXT NOT NULL,
                frequency TEXT NOT NULL CHECK(frequency IN ('once','daily','weekly','monthly','yearly')),
                end_date TEXT,
                metadata TEXT,
                occurrence_index INTEGER NOT NULL DEFAULT 0 CHECK(occurrence_index >= 0),
                next_due TEXT,
                status TEXT NOT NULL CHECK(status IN ('pending','completed','cancelled')),
                UNIQUE(id, company_slug)
            );
            CREATE INDEX idx_schedules_due ON schedules(company_slug, status, next_due);
            CREATE TABLE schedule_entries (
                schedule_id TEXT NOT NULL,
                company_slug TEXT NOT NULL,
                position INTEGER NOT NULL,
                account_code TEXT NOT NULL,
                direction TEXT NOT NULL CHECK(direction IN ('debit','credit')),
                amount INTEGER NOT NULL CHECK(amount > 0),
                memo TEXT,
                tax_category TEXT,
                PRIMARY KEY(schedule_id, position),
                FOREIGN KEY(schedule_id, company_slug) REFERENCES schedules(id, company_slug),
                FOREIGN KEY(company_slug, account_code) REFERENCES accounts(company_slug, code)
            );
            CREATE TABLE audit_log_v16 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule','schedule')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset','schedule_cancel','schedule_run')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 2 CHECK(hash_version IN (1,2)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v16 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v16 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 2
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
            INSERT INTO schema_version (version) VALUES (16);"
        )?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

/// Add append-only investment records, preserving all historical audit bytes.
fn apply_v17(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        conn.execute_batch(
            "CREATE TABLE securities (
                company_slug TEXT NOT NULL REFERENCES companies(slug),
                symbol TEXT NOT NULL,
                name TEXT NOT NULL,
                currency TEXT NOT NULL,
                PRIMARY KEY(company_slug, symbol)
            );
            CREATE TABLE security_prices (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL,
                symbol TEXT NOT NULL,
                date TEXT NOT NULL,
                price INTEGER NOT NULL CHECK(price >= 0),
                UNIQUE(company_slug, symbol, date),
                FOREIGN KEY(company_slug, symbol) REFERENCES securities(company_slug, symbol)
            );
            CREATE TABLE investment_lots (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL,
                symbol TEXT NOT NULL,
                asset TEXT NOT NULL,
                quantity TEXT NOT NULL,
                cost_basis INTEGER NOT NULL CHECK(cost_basis > 0),
                acquired_date TEXT NOT NULL,
                transaction_id INTEGER NOT NULL UNIQUE REFERENCES transactions(id),
                UNIQUE(id, company_slug),
                FOREIGN KEY(company_slug, symbol) REFERENCES securities(company_slug, symbol),
                FOREIGN KEY(company_slug, asset) REFERENCES accounts(company_slug, code)
            );
            CREATE INDEX idx_investment_lots_position ON investment_lots(company_slug, symbol, asset, acquired_date, id);
            CREATE TABLE lot_reliefs (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL,
                lot_id TEXT NOT NULL,
                quantity TEXT NOT NULL,
                cost_basis INTEGER NOT NULL CHECK(cost_basis >= 0),
                date TEXT NOT NULL,
                transaction_id INTEGER NOT NULL REFERENCES transactions(id),
                UNIQUE(lot_id, transaction_id),
                FOREIGN KEY(lot_id, company_slug) REFERENCES investment_lots(id, company_slug)
            );
            CREATE INDEX idx_lot_reliefs_lot ON lot_reliefs(lot_id, date);
            "
        )?;
        extend_investment_audit(conn)?;
        for (table, conflict) in [
            (
                "securities",
                "company_slug = NEW.company_slug AND symbol = NEW.symbol",
            ),
            (
                "security_prices",
                "id = NEW.id OR (company_slug = NEW.company_slug AND symbol = NEW.symbol AND date = NEW.date)",
            ),
            (
                "investment_lots",
                "id = NEW.id OR transaction_id = NEW.transaction_id",
            ),
            (
                "lot_reliefs",
                "id = NEW.id OR (lot_id = NEW.lot_id AND transaction_id = NEW.transaction_id)",
            ),
        ] {
            conn.execute_batch(&format!(
                "CREATE TRIGGER protect_{table}_update BEFORE UPDATE ON {table}
                 BEGIN SELECT RAISE(ABORT, 'investment records are immutable'); END;
                 CREATE TRIGGER protect_{table}_delete BEFORE DELETE ON {table}
                 BEGIN SELECT RAISE(ABORT, 'investment records cannot be deleted'); END;
                 CREATE TRIGGER protect_{table}_insert BEFORE INSERT ON {table}
                 WHEN EXISTS(SELECT 1 FROM {table} WHERE {conflict})
                 BEGIN SELECT RAISE(ABORT, 'investment records cannot be replaced'); END;"
            ))?;
        }
        conn.execute("INSERT INTO schema_version (version) VALUES (17)", [])?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

fn extend_investment_audit(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "CREATE TABLE audit_log_v17 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule','schedule','security','lot','price')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset','schedule_cancel','schedule_run')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 2 CHECK(hash_version IN (1,2)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v17 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v17 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 2
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
        ",
    )?;
    Ok(())
}

/// Enable explicit grants without changing historical audit hashes or snapshots.
fn apply_v18(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        extend_policy_audit(conn)?;
        conn.execute_batch(
            "CREATE TABLE ledger_policy (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                enforce INTEGER NOT NULL CHECK(enforce IN (0, 1)),
                updated_at TEXT NOT NULL
            );
            INSERT INTO ledger_policy VALUES (1, 0, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            CREATE TRIGGER protect_policy_delete BEFORE DELETE ON ledger_policy
            BEGIN SELECT RAISE(ABORT, 'ledger policy cannot be deleted'); END;
            CREATE TRIGGER protect_policy_insert BEFORE INSERT ON ledger_policy
            WHEN EXISTS(SELECT 1 FROM ledger_policy)
            BEGIN SELECT RAISE(ABORT, 'ledger policy cannot be replaced'); END;
            CREATE TRIGGER protect_policy_update BEFORE UPDATE ON ledger_policy
            WHEN NEW.singleton IS NOT OLD.singleton
              OR NOT EXISTS (
                  SELECT 1 FROM audit_log
                  WHERE id = (SELECT MAX(id) FROM audit_log)
                    AND entity = 'policy' AND entity_id = '1' AND action = 'policy_change'
                    AND json_extract(before, '$.enforce') IS OLD.enforce
                    AND json_extract(before, '$.updated_at') IS OLD.updated_at
                    AND json_extract(after, '$.enforce') IS NEW.enforce
                    AND json_extract(after, '$.updated_at') IS NEW.updated_at
              )
            BEGIN SELECT RAISE(ABORT, 'ledger policy change requires an audit record'); END;",
        )?;
        conn.execute("INSERT INTO schema_version (version) VALUES (18)", [])?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

fn extend_policy_audit(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "CREATE TABLE audit_log_v18 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule','schedule','security','lot','price','policy')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset','schedule_cancel','schedule_run','policy_change')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 3 CHECK(hash_version IN (1,2,3)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v18 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v18 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 3
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
        ",
    )?;
    Ok(())
}

/// Add pending proposals and spend buckets, preserving the complete v18 audit chain.
fn apply_v19(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        // Temporarily remove the trigger that references the rebuilt audit table.
        let policy_trigger: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'protect_policy_update'",
            [], |row| row.get(0),
        )?;
        conn.execute_batch("DROP TRIGGER protect_policy_update")?;
        extend_proposal_audit(conn)?;
        conn.execute_batch(&policy_trigger)?;
        conn.execute_batch(
            "CREATE TABLE txn_proposals (
                id TEXT PRIMARY KEY NOT NULL,
                company_slug TEXT NOT NULL REFERENCES companies(slug),
                status TEXT NOT NULL CHECK(status IN ('pending','approved','rejected','withdrawn')),
                payload TEXT NOT NULL CHECK(json_valid(payload)),
                reference TEXT,
                debit_total INTEGER NOT NULL CHECK(debit_total > 0),
                currency TEXT NOT NULL,
                proposer_uri TEXT NOT NULL,
                proposer_jti TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                resolved_at TEXT,
                resolved_by_uri TEXT,
                resolved_by_jti TEXT,
                transaction_id INTEGER UNIQUE REFERENCES transactions(id),
                reason TEXT,
                CHECK((resolved_by_uri IS NULL) = (resolved_by_jti IS NULL)),
                CHECK((status = 'pending') = (resolved_at IS NULL)),
                CHECK((status = 'approved') = (transaction_id IS NOT NULL))
            );
            CREATE UNIQUE INDEX idx_proposals_open_reference
                ON txn_proposals(company_slug, reference)
                WHERE reference IS NOT NULL AND status = 'pending';
            CREATE INDEX idx_proposals_company ON txn_proposals(company_slug, status);
            CREATE TRIGGER protect_proposal_payload BEFORE UPDATE ON txn_proposals
            WHEN NEW.id IS NOT OLD.id OR NEW.company_slug IS NOT OLD.company_slug
              OR NEW.payload IS NOT OLD.payload OR NEW.reference IS NOT OLD.reference
              OR NEW.debit_total IS NOT OLD.debit_total OR NEW.currency IS NOT OLD.currency
              OR NEW.proposer_uri IS NOT OLD.proposer_uri OR NEW.proposer_jti IS NOT OLD.proposer_jti
              OR NEW.created_at IS NOT OLD.created_at OR OLD.status != 'pending'
            BEGIN SELECT RAISE(ABORT, 'proposal payload and resolved proposals are immutable'); END;
            CREATE TRIGGER protect_proposal_insert BEFORE INSERT ON txn_proposals
            WHEN NEW.status != 'pending' OR NEW.resolved_at IS NOT NULL
              OR NEW.resolved_by_uri IS NOT NULL OR NEW.resolved_by_jti IS NOT NULL
              OR NEW.transaction_id IS NOT NULL OR NEW.reason IS NOT NULL
              OR EXISTS(SELECT 1 FROM txn_proposals WHERE id = NEW.id)
            BEGIN SELECT RAISE(ABORT, 'proposals must start pending and cannot be replaced'); END;
            CREATE TRIGGER protect_proposal_delete BEFORE DELETE ON txn_proposals
            BEGIN SELECT RAISE(ABORT, 'proposals cannot be deleted'); END;
            CREATE TABLE agent_spend_limits (
                agent_uri TEXT NOT NULL,
                company_slug TEXT NOT NULL REFERENCES companies(slug),
                currency TEXT NOT NULL,
                max_post INTEGER CHECK(max_post >= 0),
                daily_limit INTEGER CHECK(daily_limit >= 0),
                CHECK(max_post IS NOT NULL OR daily_limit IS NOT NULL),
                PRIMARY KEY(agent_uri, company_slug, currency)
            );
            INSERT INTO schema_version(version) VALUES (19);",
        )?;
        install_proposal_resolution_trigger(conn, ProposalAuditLink::Snapshot)?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum ProposalAuditLink {
    Snapshot,
    Column,
}

fn install_proposal_resolution_trigger(
    conn: &Connection,
    link: ProposalAuditLink,
) -> Result<(), CliError> {
    let posting_link = match link {
        ProposalAuditLink::Snapshot => "json_extract(after, '$.transaction_id')",
        ProposalAuditLink::Column => "related_posting",
    };
    conn.execute_batch(&format!(
        "CREATE TRIGGER protect_proposal_resolution BEFORE UPDATE ON txn_proposals
         WHEN NOT EXISTS (
             SELECT 1 FROM audit_log
             WHERE id = (SELECT MAX(id) FROM audit_log) AND entity = 'proposal'
               AND company_slug = NEW.company_slug AND entity_id = NEW.id
               AND action = CASE WHEN NEW.status = 'approved' THEN 'proposal_approve' ELSE 'proposal_reject' END
               AND json_extract(before, '$.status') = OLD.status
               AND json_extract(after, '$.id') = NEW.id
               AND json_extract(after, '$.status') = NEW.status
               AND json_extract(after, '$.resolved_at') IS NEW.resolved_at
               AND json_extract(after, '$.resolved_by_uri') IS NEW.resolved_by_uri
               AND json_extract(after, '$.resolved_by_jti') IS NEW.resolved_by_jti
               AND {posting_link} IS NEW.transaction_id
               AND json_extract(after, '$.reason') IS NEW.reason
         )
         BEGIN SELECT RAISE(ABORT, 'proposal resolution requires an audit snapshot'); END;"
    ))?;
    Ok(())
}

fn extend_proposal_audit(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "CREATE TABLE audit_log_v19 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule','schedule','security','lot','price','policy','proposal','spend_limit')),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset','schedule_cancel','schedule_run','policy_change','proposal_approve','proposal_reject','limit_set')),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT 3 CHECK(hash_version IN (1,2,3)),
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
            );
            INSERT INTO audit_log_v19 SELECT * FROM audit_log ORDER BY id;
            DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_v19 RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != 3
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
        ",
    )?;
    Ok(())
}

/// Add company cutoffs without rewriting historical hashes or audit snapshots.
fn apply_v20(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        // These triggers reference the audit table while it is being rebuilt.
        let triggers: Vec<(String, String)> = conn
            .prepare(
                "SELECT name, sql FROM sqlite_master WHERE type = 'trigger'
             AND name IN ('protect_policy_update', 'protect_proposal_resolution')",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (name, _) in &triggers {
            conn.execute_batch(&format!("DROP TRIGGER {name}"))?;
        }
        rebuild_audit(conn, AuditTable::Periods)?;
        for (_, sql) in &triggers {
            conn.execute_batch(sql)?;
        }
        conn.execute_batch(
            "CREATE TABLE period_locks (
                company_slug TEXT PRIMARY KEY NOT NULL REFERENCES companies(slug),
                closed_through TEXT,
                changed_at TEXT NOT NULL,
                actor TEXT NOT NULL,
                reason TEXT,
                CHECK(closed_through IS NULL OR
                    (length(closed_through) = 10 AND date(closed_through, '+0 days') IS closed_through)),
                CHECK(closed_through IS NOT NULL OR (reason IS NOT NULL AND length(trim(reason)) > 0))
            );
            CREATE TRIGGER protect_period_delete BEFORE DELETE ON period_locks
            BEGIN SELECT RAISE(ABORT, 'period history cannot be deleted'); END;
            CREATE TRIGGER protect_period_insert BEFORE INSERT ON period_locks
            WHEN EXISTS(SELECT 1 FROM period_locks WHERE company_slug = NEW.company_slug)
              OR NEW.closed_through IS NULL
              OR NOT EXISTS (
                SELECT 1 FROM audit_log WHERE id = (SELECT MAX(id) FROM audit_log)
                  AND entity = 'period' AND entity_id = NEW.company_slug
                  AND company_slug = NEW.company_slug AND action = 'period_close'
                  AND before IS NULL
                  AND json_extract(after, '$.company_slug') IS NEW.company_slug
                  AND json_extract(after, '$.closed_through') IS NEW.closed_through
                  AND json_extract(after, '$.changed_at') IS NEW.changed_at
                  AND json_extract(after, '$.actor') IS NEW.actor
                  AND json_extract(after, '$.reason') IS NEW.reason
            )
            BEGIN SELECT RAISE(ABORT, 'period close requires an audit snapshot'); END;
            CREATE TRIGGER protect_period_update BEFORE UPDATE ON period_locks
            WHEN NEW.company_slug IS NOT OLD.company_slug
              OR NOT EXISTS (
                SELECT 1 FROM audit_log WHERE id = (SELECT MAX(id) FROM audit_log)
                  AND entity = 'period' AND entity_id = NEW.company_slug
                  AND company_slug = NEW.company_slug
                  AND action = CASE WHEN NEW.closed_through IS NULL THEN 'period_reopen' ELSE 'period_close' END
                  AND json_extract(before, '$.company_slug') IS OLD.company_slug
                  AND json_extract(before, '$.closed_through') IS OLD.closed_through
                  AND json_extract(before, '$.changed_at') IS OLD.changed_at
                  AND json_extract(before, '$.actor') IS OLD.actor
                  AND json_extract(before, '$.reason') IS OLD.reason
                  AND json_extract(after, '$.company_slug') IS NEW.company_slug
                  AND json_extract(after, '$.closed_through') IS NEW.closed_through
                  AND json_extract(after, '$.changed_at') IS NEW.changed_at
                  AND json_extract(after, '$.actor') IS NEW.actor
                  AND json_extract(after, '$.reason') IS NEW.reason
            )
            BEGIN SELECT RAISE(ABORT, 'period change requires an audit snapshot'); END;
            CREATE TRIGGER protect_period_post BEFORE INSERT ON transactions
            WHEN EXISTS(SELECT 1 FROM period_locks
                WHERE company_slug = NEW.company_slug AND date(NEW.date) <= closed_through)
            BEGIN SELECT RAISE(ABORT, 'transaction date is in a closed period'); END;
            INSERT INTO schema_version(version) VALUES (20);",
        )?;
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

fn apply_v21(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(
            "CREATE TABLE setup_sessions (
                generation INTEGER PRIMARY KEY AUTOINCREMENT,
                token_jti TEXT NOT NULL UNIQUE REFERENCES agent_tokens(jti),
                state TEXT NOT NULL CHECK (state IN ('active', 'finished', 'revoked')),
                ended_at TEXT,
                CHECK ((state = 'active') = (ended_at IS NULL))
             );
             CREATE UNIQUE INDEX idx_setup_sessions_active
                 ON setup_sessions(state) WHERE state = 'active';
             INSERT INTO schema_version(version) VALUES (21);",
        )?;
        Ok(())
    })
}

fn apply_v22(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(
            "CREATE TABLE setup_book_profiles (
                revision INTEGER PRIMARY KEY AUTOINCREMENT,
                company_slug TEXT NOT NULL REFERENCES companies(slug) ON DELETE CASCADE,
                purpose TEXT NOT NULL CHECK (purpose IN ('personal', 'business')),
                base_currency TEXT NOT NULL,
                fiscal_year_start TEXT NOT NULL,
                supersedes INTEGER UNIQUE REFERENCES setup_book_profiles(revision),
                generation INTEGER NOT NULL REFERENCES setup_sessions(generation)
             );
             CREATE UNIQUE INDEX idx_setup_book_initial
                 ON setup_book_profiles(company_slug) WHERE supersedes IS NULL;
             CREATE INDEX idx_setup_book_revisions
                 ON setup_book_profiles(company_slug, revision DESC);
             CREATE TABLE setup_requests (
                reference TEXT PRIMARY KEY NOT NULL,
                generation INTEGER NOT NULL REFERENCES setup_sessions(generation),
                fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
                outcome TEXT NOT NULL CHECK (json_valid(outcome))
             );
             INSERT INTO schema_version(version) VALUES (22);",
        )?;
        Ok(())
    })
}

fn apply_v23(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(
            "CREATE TABLE setup_statement_batches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                company_slug TEXT NOT NULL,
                generation INTEGER NOT NULL REFERENCES setup_sessions(generation),
                reference TEXT NOT NULL,
                setup_jti TEXT NOT NULL REFERENCES agent_tokens(jti),
                request_hash TEXT NOT NULL CHECK(length(request_hash) = 64),
                preview_hash TEXT NOT NULL CHECK(length(preview_hash) = 64),
                payload TEXT NOT NULL CHECK(json_valid(payload)),
                UNIQUE (company_slug, generation, reference)
             );
             CREATE TRIGGER protect_setup_statement_batch_update
               BEFORE UPDATE ON setup_statement_batches
               BEGIN SELECT RAISE(ABORT, 'statement preview is immutable'); END;
             CREATE TRIGGER protect_setup_statement_batch_delete
               BEFORE DELETE ON setup_statement_batches
               BEGIN SELECT RAISE(ABORT, 'statement preview cannot be deleted'); END;
             INSERT INTO schema_version(version) VALUES (23);",
        )?;
        Ok(())
    })
}

/// Admit the demo event without changing existing audit rows or their hashes.
fn apply_v24(conn: &Connection) -> Result<(), CliError> {
    migrate_audit(conn, AuditTable::Demo, SchemaVersion::known(24))
}

/// Add unverified client hints without rewriting historical hashes.
fn apply_v25(conn: &Connection) -> Result<(), CliError> {
    migrate_audit(conn, AuditTable::ClientHints, CLIENT_HINT_SCHEMA_VERSION)
}

fn apply_v26(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        migrate_audit(conn, AuditTable::Receipts, RECEIPT_SCHEMA_VERSION)?;
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN archived INTEGER NOT NULL DEFAULT 0
                 CHECK (archived IN (0, 1));
             CREATE TRIGGER protect_archived_account_delete BEFORE DELETE ON accounts
                 WHEN OLD.archived = 1
                 BEGIN SELECT RAISE(ABORT, 'archived accounts are retained'); END;
             CREATE TRIGGER protect_archived_account_insert BEFORE INSERT ON accounts
                 WHEN NEW.archived <> 0 OR EXISTS (
                     SELECT 1 FROM accounts WHERE company_slug = NEW.company_slug
                         AND code = NEW.code AND archived = 1
                 )
                 BEGIN SELECT RAISE(ABORT, 'new accounts must be active'); END;
             CREATE TRIGGER protect_account_archive BEFORE UPDATE OF archived ON accounts
                 WHEN OLD.archived <> 0 OR NEW.archived <> 1 OR NOT EXISTS (
                     SELECT 1 FROM audit_log
                     WHERE id = (SELECT MAX(id) FROM audit_log)
                       AND company_slug = OLD.company_slug AND entity_id = OLD.code
                       AND entity = 'account' AND action = 'account_archive'
                       AND agent_uri IS NOT NULL AND undo_of IS NOT NULL
                 )
                 BEGIN SELECT RAISE(ABORT, 'account archive requires an owner undo audit record'); END;",
        )?;
        install_receipt_read_paths(conn, AuditTable::Receipts)?;
        conn.execute_batch("DROP TRIGGER protect_proposal_resolution;")?;
        install_proposal_resolution_trigger(conn, ProposalAuditLink::Column)?;
        Ok(())
    })
}

fn install_receipt_read_paths(conn: &Connection, table: AuditTable) -> Result<(), CliError> {
    let recategorize = recategorization_action(table);
    conn.execute_batch(&format!(
        "CREATE INDEX idx_receipt_company ON audit_log(company_slug, id);
         CREATE UNIQUE INDEX idx_receipt_undo ON audit_log(undo_of) WHERE undo_of IS NOT NULL;
         CREATE INDEX idx_receipt_posting ON audit_log(company_slug, related_posting)
             WHERE action IN ('reverse', 'proposal_approve'{recategorize});
         CREATE VIEW audit_receipt_postings AS
             SELECT id, company_slug, related_posting AS posting_id
             FROM audit_log WHERE action IN ('reverse', 'proposal_approve'{recategorize});"
    ))?;
    Ok(())
}

/// Add typed standing settings and rule lineage, preserving all existing audit rows.
fn apply_v27(conn: &Connection) -> Result<(), CliError> {
    migrate_audit(
        conn,
        AuditTable::StandingSettings,
        STANDING_SETTINGS_SCHEMA_VERSION,
    )
}

/// Add owner rule reversals without changing the audit hash payload.
fn apply_v28(conn: &Connection) -> Result<(), CliError> {
    migrate_audit(conn, AuditTable::RuleUndo, RULE_UNDO_SCHEMA_VERSION)
}

fn apply_v29(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        migrate_audit(conn, AuditTable::DirectSetup, DIRECT_SETUP_SCHEMA_VERSION)?;
        conn.execute_batch(
            "ALTER TABLE setup_book_profiles RENAME TO legacy_book_profiles;
             CREATE TABLE setup_book_profiles (
                revision INTEGER PRIMARY KEY AUTOINCREMENT,
                company_slug TEXT NOT NULL REFERENCES companies(slug) ON DELETE CASCADE,
                purpose TEXT NOT NULL CHECK (purpose IN ('personal', 'business')),
                base_currency TEXT NOT NULL,
                fiscal_year_start TEXT NOT NULL,
                supersedes INTEGER UNIQUE REFERENCES setup_book_profiles(revision),
                generation INTEGER REFERENCES setup_sessions(generation)
             );
             INSERT INTO setup_book_profiles SELECT * FROM legacy_book_profiles ORDER BY revision;
             DROP TABLE legacy_book_profiles;
             CREATE INDEX idx_setup_book_revisions ON setup_book_profiles(company_slug, revision DESC);",
        )?;
        Ok(())
    })
}

/// Add linked annotation undo events without rewriting historical hashes.
fn apply_v30(conn: &Connection) -> Result<(), CliError> {
    migrate_audit(
        conn,
        AuditTable::AnnotationUndo,
        ANNOTATION_UNDO_SCHEMA_VERSION,
    )
}

/// Add pending holds after the annotation undo migration rebuilds the audit table.
fn apply_v31(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        conn.execute_batch(include_str!("schema/holds.sql"))?;
        install_hold_audit(conn, AuditHistory::Physical)?;
        conn.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            [HOLDS_SCHEMA_VERSION],
        )?;
        Ok(())
    })
}

/// Admit account rename events while preserving the hold rules built on `audit_log`.
fn apply_v32(conn: &Connection) -> Result<(), CliError> {
    migrate_hold_audit(
        conn,
        AuditTable::AccountRename,
        ACCOUNT_RENAME_SCHEMA_VERSION,
    )
}

fn apply_v35(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        migrate_hold_audit(conn, AuditTable::TransactionLinks, TXN_LINK_SCHEMA_VERSION)?;
        conn.execute_batch(include_str!("schema/transaction_links.sql"))?;
        install_transaction_link_views(conn, AuditHistory::from(AuditTable::TransactionLinks))
    })
}

fn migrate_hold_audit(
    conn: &Connection,
    table: AuditTable,
    version: SchemaVersion,
) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        // The index drops with audit_log. Drop its dependent views and
        // trigger explicitly so SQLite can rename the replacement table.
        conn.execute_batch(
            "DROP TRIGGER protect_hold_settlement;
             DROP VIEW hold_adjustments;
             DROP VIEW posting_reversals;",
        )?;
        migrate_audit(conn, table, version)?;
        install_hold_audit(conn, table.into())
    })
}

fn install_hold_audit(conn: &Connection, audit: AuditHistory) -> Result<(), CliError> {
    conn.execute_batch(&format!(
        include_str!("schema/hold_audit.sql"),
        audit = audit
    ))?;
    Ok(())
}

fn install_transaction_link_views(conn: &Connection, audit: AuditHistory) -> Result<(), CliError> {
    conn.execute_batch(&format!(
        "CREATE VIEW transaction_link_creations AS
             SELECT id AS receipt, entity_id AS id, company_slug,
                    json_extract(after, '$.source_id') AS source_id,
                    json_extract(after, '$.target_id') AS target_id,
                    json_extract(after, '$.kind') AS link_kind
             FROM {audit} WHERE action = 'link_create';
         CREATE VIEW active_transaction_links AS
             SELECT link.* FROM transaction_links link
             WHERE NOT EXISTS (
                 SELECT 1 FROM {audit} undo
                 WHERE undo.undo_of = link.receipt AND undo.action = 'link_undo'
             );"
    ))?;
    Ok(())
}

fn install_annotation_undo_guard(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "CREATE TRIGGER protect_annotation_restore BEFORE INSERT ON audit_log
         WHEN NEW.action = 'metadata_restore' AND NOT EXISTS (
             SELECT 1 FROM audit_log original
             WHERE original.id = NEW.undo_of AND original.entity = 'transaction'
               AND original.action IN ('metadata_set', 'metadata_unset')
               AND original.company_slug = NEW.company_slug
               AND original.entity_id = NEW.entity_id
               AND original.after IS NEW.before AND original.before IS NEW.after
         ) BEGIN SELECT RAISE(ABORT, 'annotation restore must reverse the original receipt'); END;",
    )?;
    Ok(())
}

fn install_rule_undo_guards(conn: &Connection, audit: AuditHistory) -> Result<(), CliError> {
    conn.execute_batch(&format!(
        "CREATE TRIGGER protect_rule_deactivation BEFORE INSERT ON audit_log
         WHEN NEW.action = 'rule_deactivate' AND (
             NOT EXISTS (SELECT 1 FROM audit_log original
                 WHERE original.id = NEW.undo_of AND original.entity = 'rule'
                   AND original.action = 'create' AND original.company_slug = NEW.company_slug
                   AND original.entity_id = NEW.entity_id)
             OR NOT EXISTS (SELECT 1 FROM categorization_rules rule
                 WHERE rule.company_slug = NEW.company_slug AND rule.id = NEW.entity_id
                   AND rule.supersedes IS NULL)
         ) BEGIN SELECT RAISE(ABORT, 'rule deactivation requires an original creation'); END;
         CREATE TRIGGER protect_deactivated_rule_insert BEFORE INSERT ON categorization_rules
         WHEN EXISTS (SELECT 1 FROM {audit} event
             WHERE event.entity = 'rule' AND event.entity_id = NEW.id
               AND event.action = 'rule_deactivate')
         BEGIN SELECT RAISE(ABORT, 'deactivated rules cannot be replaced'); END;
         CREATE TRIGGER protect_deactivated_rule_update BEFORE UPDATE ON categorization_rules
         WHEN EXISTS (SELECT 1 FROM {audit} event
             WHERE event.company_slug = OLD.company_slug AND event.entity = 'rule'
               AND event.entity_id = OLD.id AND event.action = 'rule_deactivate')
         BEGIN SELECT RAISE(ABORT, 'deactivated rules are immutable'); END;
         CREATE TRIGGER protect_deactivated_rule_delete BEFORE DELETE ON categorization_rules
         WHEN EXISTS (SELECT 1 FROM {audit} event
             WHERE event.company_slug = OLD.company_slug AND event.entity = 'rule'
               AND event.entity_id = OLD.id AND event.action = 'rule_deactivate')
         BEGIN SELECT RAISE(ABORT, 'deactivated rules cannot be deleted'); END;"
    ))?;
    Ok(())
}

fn create_standing_settings(conn: &Connection) -> Result<(), CliError> {
    conn.execute_batch(
        "ALTER TABLE categorization_rules ADD COLUMN supersedes TEXT CHECK(supersedes IS NULL OR supersedes != id);
         CREATE TABLE standing_settings (
            revision INTEGER PRIMARY KEY,
            company_slug TEXT NOT NULL REFERENCES companies(slug),
            sales_tax TEXT NOT NULL CHECK(sales_tax IN ('unset', 'track-liability', 'marketplace-remitted', 'not-applicable')),
            processor_fees TEXT NOT NULL CHECK(processor_fees IN ('unset', 'separate-expense', 'net-from-revenue')),
            inform_preference TEXT CHECK(inform_preference IN ('per-change', 'daily-digest', 'weekly-digest'))
         );
         CREATE INDEX idx_standing_settings_company ON standing_settings(company_slug, revision DESC);
         CREATE TRIGGER protect_standing_settings_update BEFORE UPDATE ON standing_settings
           BEGIN SELECT RAISE(ABORT, 'standing settings revisions are immutable'); END;
         CREATE TRIGGER protect_standing_settings_delete BEFORE DELETE ON standing_settings
           BEGIN SELECT RAISE(ABORT, 'standing settings revisions cannot be deleted'); END;"
    )?;
    Ok(())
}

fn migrate_audit(
    conn: &Connection,
    table: AuditTable,
    schema: SchemaVersion,
) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        let audit = AuditHistory::from(table);
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        let definitions = take_audit_dependents(conn, table)?;
        rebuild_audit(conn, table)?;
        if table >= AuditTable::StatementReconciliation {
            install_transaction_link_views(conn, audit)?;
        }
        for definition in definitions.into_iter().rev() {
            conn.execute_batch(&definition)?;
        }
        if table >= AuditTable::StandingSettings {
            install_receipt_read_paths(conn, table)?;
        }
        if table >= AuditTable::RuleUndo {
            install_rule_undo_guards(conn, audit)?;
        }
        if table >= AuditTable::DirectSetup {
            conn.execute_batch(
                "CREATE UNIQUE INDEX idx_direct_setup_reference ON audit_log(company_slug, entity_id)
                     WHERE action = 'setup_apply';
                 CREATE INDEX idx_direct_setup_reverts ON audit_log(company_slug)
                     WHERE action = 'setup_revert';",
            )?;
        }
        if table >= AuditTable::AnnotationUndo {
            install_annotation_undo_guard(conn)?;
        }
        if table >= AuditTable::AccountRename {
            conn.execute_batch(
                "CREATE UNIQUE INDEX idx_account_rename_proposal ON audit_log(entity_id, action)
                     WHERE entity = 'account_rename_proposal';",
            )?;
        }
        if table >= AuditTable::Correction {
            conn.execute_batch(
                "CREATE UNIQUE INDEX idx_correction_proposal ON audit_log(entity_id, action)
                     WHERE entity = 'correction_proposal';
                 CREATE INDEX idx_correction_company ON audit_log(company_slug, agent_uri, id)
                     WHERE action = 'correction_propose';",
            )?;
        }
        if table >= AuditTable::Recategorization {
            conn.execute_batch(
                "CREATE UNIQUE INDEX idx_recategorization_posting
                    ON audit_log(company_slug, related_posting) WHERE action = 'recategorize';
                 CREATE INDEX idx_recategorization_source
                    ON audit_log(company_slug, entity_id) WHERE action = 'recategorize';",
            )?;
        }
        if table >= AuditTable::LedgerRestore {
            conn.execute_batch(&format!(include_str!("schema/restore.sql"), audit = audit))?;
        }
        conn.execute("INSERT INTO schema_version(version) VALUES (?1)", [schema])?;
        if table == AuditTable::Receipts {
            super::audit::receipts::seal_legacy_links(conn)?;
        }
        super::audit::verify_hash_chain(ReadConn::physical(conn))?;
        Ok(())
    })
}

fn take_audit_dependents(conn: &Connection, table: AuditTable) -> Result<Vec<String>, CliError> {
    let mut dependents = vec![
        "protect_policy_update",
        "protect_proposal_resolution",
        "protect_period_insert",
        "protect_period_update",
    ];
    if table >= AuditTable::StandingSettings {
        dependents.push("protect_account_archive");
        conn.execute_batch("DROP VIEW audit_receipt_postings")?;
    }
    if table >= AuditTable::DirectSetup {
        conn.execute_batch(
            "DROP TRIGGER protect_deactivated_rule_insert;
             DROP TRIGGER protect_deactivated_rule_update;
             DROP TRIGGER protect_deactivated_rule_delete;",
        )?;
    }
    if table == AuditTable::StandingSettings {
        create_standing_settings(conn)?;
    }
    if table >= AuditTable::StatementReconciliation {
        // The insert trigger must be removed before its views can be rebuilt.
        dependents.extend([
            "protect_transaction_link_insert",
            "protect_transaction_link_undo",
        ]);
    }
    if table >= AuditTable::LedgerRestore {
        dependents.extend([
            "protect_statement_insert",
            "protect_statement_update",
            "protect_statement_undo",
        ]);
    }
    let mut definitions = Vec::with_capacity(dependents.len());
    for name in dependents {
        let definition: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
            [name],
            |row| row.get(0),
        )?;
        definitions.push(definition);
        conn.execute_batch(&format!("DROP TRIGGER {name}"))?;
    }
    if table >= AuditTable::StatementReconciliation {
        conn.execute_batch(
            "DROP VIEW active_transaction_links; DROP VIEW transaction_link_creations;",
        )?;
    }
    Ok(definitions)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AuditTable {
    Periods,
    Demo,
    ClientHints,
    Receipts,
    StandingSettings,
    RuleUndo,
    DirectSetup,
    AnnotationUndo,
    AccountRename,
    Correction,
    Recategorization,
    TransactionLinks,
    StatementReconciliation,
    LedgerRestore,
    CashClassification,
    Counterparties,
}

impl From<AuditTable> for AuditHistory {
    fn from(table: AuditTable) -> Self {
        match table {
            AuditTable::LedgerRestore
            | AuditTable::CashClassification
            | AuditTable::Counterparties => Self::Bookkeeping,
            AuditTable::Periods
            | AuditTable::Demo
            | AuditTable::ClientHints
            | AuditTable::Receipts
            | AuditTable::StandingSettings
            | AuditTable::RuleUndo
            | AuditTable::DirectSetup
            | AuditTable::AnnotationUndo
            | AuditTable::AccountRename
            | AuditTable::Correction
            | AuditTable::Recategorization
            | AuditTable::TransactionLinks
            | AuditTable::StatementReconciliation => Self::Physical,
        }
    }
}

fn audit_extensions(table: AuditTable) -> (String, String) {
    use super::audit::{AuditAction, AuditEntity};
    use std::fmt::Write as _;

    let (mut entity, mut action) = if table == AuditTable::Periods {
        (String::new(), String::new())
    } else {
        (
            format!(",'{}'", AuditEntity::Demo),
            format!(",'{}'", AuditAction::DemoSeed),
        )
    };
    if table >= AuditTable::Receipts {
        let _ = write!(entity, ",'{}'", AuditEntity::Audit);
        let _ = write!(
            action,
            ",'{}','{}'",
            AuditAction::AccountArchive,
            AuditAction::ReceiptLinkMigration
        );
    }
    if table >= AuditTable::StandingSettings {
        let _ = write!(
            entity,
            ",'{}','{}'",
            AuditEntity::BookkeepingPolicy,
            AuditEntity::InformPreference
        );
        let _ = write!(
            action,
            ",'{}','{}','{}'",
            AuditAction::RuleSupersede,
            AuditAction::BookkeepingPolicySet,
            AuditAction::InformPreferenceSet
        );
    }
    if table >= AuditTable::RuleUndo {
        let _ = write!(action, ",'{}'", AuditAction::RuleDeactivate);
    }
    if table >= AuditTable::DirectSetup {
        let _ = write!(
            action,
            ",'{}','{}'",
            AuditAction::SetupApply,
            AuditAction::SetupRevert
        );
    }
    if table >= AuditTable::AnnotationUndo {
        let _ = write!(action, ",'{}'", AuditAction::MetadataRestore);
    }
    if table >= AuditTable::AccountRename {
        let _ = write!(entity, ",'{}'", AuditEntity::AccountRenameProposal);
        let _ = write!(
            action,
            ",'{}','{}','{}'",
            AuditAction::AccountRename,
            AuditAction::AccountRenamePropose,
            AuditAction::AccountRenameResolve
        );
    }
    if table >= AuditTable::Correction {
        let _ = write!(entity, ",'{}'", AuditEntity::CorrectionProposal);
        let _ = write!(
            action,
            ",'{}','{}'",
            AuditAction::CorrectionPropose,
            AuditAction::CorrectionResolve
        );
    }
    action.push_str(recategorization_action(table));
    if table >= AuditTable::TransactionLinks {
        let _ = write!(entity, ",'{}'", AuditEntity::TransactionLink);
        let _ = write!(
            action,
            ",'{}','{}'",
            AuditAction::LinkCreate,
            AuditAction::LinkUndo
        );
    }
    if table >= AuditTable::StatementReconciliation {
        let _ = write!(entity, ",'{}'", AuditEntity::StatementReconciliation);
        let _ = write!(
            action,
            ",'{}','{}'",
            AuditAction::StatementReconcile,
            AuditAction::StatementUnreconcile
        );
    }
    if table >= AuditTable::LedgerRestore {
        let _ = write!(entity, ",'{}'", AuditEntity::Ledger);
        let _ = write!(action, ",'{}'", AuditAction::LedgerRestored);
    }
    if table >= AuditTable::CashClassification {
        let _ = write!(action, ",'{}'", AuditAction::AccountClassify);
    }
    if table >= AuditTable::Counterparties {
        let _ = write!(
            entity,
            ",'{}','{}'",
            AuditEntity::Counterparty,
            AuditEntity::TransactionCounterparty
        );
        let _ = write!(
            action,
            ",'{}','{}','{}','{}'",
            AuditAction::CounterpartyCreate,
            AuditAction::CounterpartyAmend,
            AuditAction::CounterpartyArchive,
            AuditAction::CounterpartyAssociate
        );
    }
    (entity, action)
}

fn recategorization_action(table: AuditTable) -> &'static str {
    if table >= AuditTable::Recategorization {
        ",'recategorize'"
    } else {
        ""
    }
}

fn setup_audit_checks(table: AuditTable) -> (&'static str, &'static str) {
    if table >= AuditTable::DirectSetup {
        (",'setup_revert'", ", CHECK(action <> 'setup_revert' OR (
             entity = 'company' AND undo_of IS NOT NULL AND hash_version = 7)),
             CHECK(action <> 'setup_apply' OR (
             entity = 'company' AND company_slug IS NOT NULL AND agent_uri IS NOT NULL AND hash_version = 7))")
    } else {
        ("", "")
    }
}

fn classification_audit_checks(table: AuditTable) -> (&'static str, &'static str) {
    let classify_undo = if table >= AuditTable::CashClassification {
        ",'account_classify'"
    } else {
        ""
    };
    let classify_check = if table >= AuditTable::CashClassification {
        ", CHECK(action <> 'account_classify' OR (
            entity = 'account' AND company_slug IS NOT NULL AND length(entity_id) > 0
            AND hash_version = 7 AND before IS NOT NULL AND after IS NOT NULL
            AND CASE WHEN json_valid(before) THEN
                json_type(before) IS 'object' AND json_type(before, '$.cash_class') IS 'text'
                AND COALESCE(json_extract(before, '$.cash_class') IN ('non_cash', 'cash', 'cash_equivalent'), 0)
                ELSE 0 END
            AND CASE WHEN json_valid(after) THEN
                json_type(after) IS 'object' AND json_type(after, '$.cash_class') IS 'text'
                AND COALESCE(json_extract(after, '$.cash_class') IN ('non_cash', 'cash', 'cash_equivalent'), 0)
                ELSE 0 END))"
    } else {
        ""
    };
    (classify_undo, classify_check)
}

fn receipt_audit_checks(table: AuditTable) -> String {
    let (undo_action, rule_check) = if table >= AuditTable::RuleUndo {
        (
            ",'rule_deactivate'",
            ", CHECK(action <> 'rule_deactivate' OR (
                   entity = 'rule' AND undo_of IS NOT NULL AND hash_version >= 7))",
        )
    } else {
        ("", "")
    };
    let (setup_undo, setup_check) = setup_audit_checks(table);
    let (annotation_undo, annotation_check) = if table >= AuditTable::AnnotationUndo {
        (
            ",'metadata_restore'",
            ", CHECK(action <> 'metadata_restore' OR (
            entity = 'transaction' AND undo_of IS NOT NULL AND hash_version = 7))",
        )
    } else {
        ("", "")
    };
    let (rename_undo, rename_check) = if table >= AuditTable::AccountRename {
        (",'account_rename'", ", CHECK(action <> 'account_rename' OR (
            entity = 'account' AND company_slug IS NOT NULL AND before IS NOT NULL AND after IS NOT NULL)),
            CHECK((action IN ('account_rename_propose', 'account_rename_resolve')) = (entity = 'account_rename_proposal')),
            CHECK(entity <> 'account_rename_proposal' OR (
                company_slug IS NOT NULL AND after IS NOT NULL
                AND (action = 'account_rename_resolve' OR (agent_uri IS NOT NULL AND before IS NULL))
            ))")
    } else {
        ("", "")
    };
    let correction_check = if table >= AuditTable::Correction {
        ", CHECK((action IN ('correction_propose', 'correction_resolve')) = (entity = 'correction_proposal')),
           CHECK(entity <> 'correction_proposal' OR (company_slug IS NOT NULL AND agent_uri IS NOT NULL
                 AND after IS NOT NULL AND (action = 'correction_resolve' OR before IS NULL)))"
    } else {
        ""
    };
    let recategorize = recategorization_action(table);
    let recategorize_check = if table >= AuditTable::Recategorization {
        ", CHECK(action <> 'recategorize' OR (
            entity = 'transaction' AND company_slug IS NOT NULL AND before IS NULL
            AND after IS NOT NULL AND related_posting IS NOT NULL AND hash_version = 7))"
    } else {
        ""
    };
    let (link_undo, link_check) = if table >= AuditTable::TransactionLinks {
        (
            ",'link_undo'",
            ", CHECK((action IN ('link_create', 'link_undo')) = (entity = 'transaction_link')),
         CHECK(entity <> 'transaction_link' OR (
            company_slug IS NOT NULL AND hash_version = 7
            AND ((action = 'link_create' AND undo_of IS NULL AND before IS NULL
                    AND after IS NOT NULL AND json_valid(after))
                OR (action = 'link_undo' AND undo_of IS NOT NULL AND before IS NOT NULL
                    AND json_valid(before) AND after IS NULL))))",
        )
    } else {
        ("", "")
    };
    let (statement_undo, statement_check) = if table >= AuditTable::StatementReconciliation {
        (",'statement_unreconcile'",
         ", CHECK((action IN ('statement_reconcile', 'statement_unreconcile')) = (entity = 'statement_reconciliation')),
            CHECK(entity <> 'statement_reconciliation' OR (company_slug IS NOT NULL AND
                hash_version = 7 AND before IS NULL AND after IS NOT NULL AND json_valid(after)
                AND (action = 'statement_unreconcile') = (undo_of IS NOT NULL)))")
    } else {
        ("", "")
    };
    let (classify_undo, classify_check) = classification_audit_checks(table);
    let restore_check = if table >= AuditTable::LedgerRestore {
        ", CHECK((action = 'ledger_restored') = (entity = 'ledger')),
           CHECK(entity <> 'ledger' OR (
                company_slug IS NULL AND hash_version = 7 AND undo_of IS NULL AND before IS NULL
                AND after IS NOT NULL AND json_valid(after)
                AND (json_type(after, '$.source_audit_id') IS 'null'
                     OR (json_type(after, '$.source_audit_id') IS 'integer'
                        AND json_extract(after, '$.source_audit_id') BETWEEN 1 AND id - 1))))"
    } else {
        ""
    };
    format!(", CHECK(client_hint IS NULL OR (hash_version >= 5 AND agent_uri IS NULL AND json_valid(client_hint))),
                   CHECK(undo_of IS NULL OR (typeof(undo_of) = 'integer' AND undo_of > 0
                       AND undo_of < id AND action IN ('reverse', 'account_archive'{undo_action}{setup_undo}{annotation_undo}{rename_undo}{link_undo}{statement_undo}{classify_undo})
                       AND company_slug IS NOT NULL AND agent_uri IS NOT NULL)),
                   CHECK(related_posting IS NULL OR (typeof(related_posting) = 'integer'
                       AND related_posting > 0 AND company_slug IS NOT NULL
                       AND action IN ('reverse', 'proposal_approve', 'correlate'{recategorize}))),
                   CHECK(hash_version < 6 OR (
                       (action IN ('reverse', 'proposal_approve', 'correlate'{recategorize})) = (related_posting IS NOT NULL)
                       AND (action <> 'account_archive' OR undo_of IS NOT NULL))){rule_check}{setup_check}{annotation_check}{rename_check}{correction_check}{recategorize_check}{link_check}{statement_check}{classify_check}{restore_check}")
}

fn rebuild_audit(conn: &Connection, table: AuditTable) -> Result<(), CliError> {
    let (entity, action) = audit_extensions(table);
    let (hash_version, version_suffix, column, check, select_suffix) = match table {
        AuditTable::Receipts | AuditTable::StandingSettings | AuditTable::RuleUndo | AuditTable::DirectSetup | AuditTable::AnnotationUndo | AuditTable::AccountRename | AuditTable::Correction | AuditTable::Recategorization | AuditTable::TransactionLinks | AuditTable::StatementReconciliation | AuditTable::LedgerRestore | AuditTable::CashClassification | AuditTable::Counterparties => {
            let (hash_version, version_suffix, select_suffix) = if table == AuditTable::Receipts {
                (6, ",5,6", ", NULL, NULL")
            } else {
                (7, ",5,6,7", "")
            };
            (
                hash_version,
                version_suffix,
                if table >= AuditTable::LedgerRestore {
                    // Historical posting IDs may refer to a superseded set of books.
                    "client_hint TEXT, undo_of INTEGER REFERENCES audit_log_rebuild(id),
                     related_posting INTEGER,"
                } else {
                    "client_hint TEXT, undo_of INTEGER REFERENCES audit_log_rebuild(id),
                     related_posting INTEGER REFERENCES transactions(id),"
                },
                receipt_audit_checks(table),
                select_suffix,
            )
        }
        AuditTable::ClientHints => (
            5,
            ",5",
            "client_hint TEXT,",
            ", CHECK(client_hint IS NULL OR (hash_version = 5 AND agent_uri IS NULL AND json_valid(client_hint)))".to_owned(),
            ", NULL",
        ),
        _ => (4, "", "", String::new(), ""),
    };
    conn.execute_batch(&format!(
        "CREATE TABLE audit_log_rebuild (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                changed_at TEXT NOT NULL DEFAULT (datetime('now')),
                actor TEXT NOT NULL,
                company_slug TEXT,
                entity TEXT NOT NULL CHECK(entity IN ('entry','transaction','budget','account','company','key','agent','rule','schedule','security','lot','price','policy','proposal','spend_limit','period'{entity})),
                entity_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('create','status_change','correlate','reverse','budget_set','budget_delete','account_delete','company_delete','key_init','token_issue','token_revoke','attach','rule_delete','metadata_set','metadata_unset','schedule_cancel','schedule_run','policy_change','proposal_approve','proposal_reject','limit_set','period_close','period_reopen'{action})),
                before TEXT,
                after TEXT,
                prev_hash TEXT NOT NULL CHECK(length(prev_hash) = 64),
                row_hash TEXT NOT NULL CHECK(length(row_hash) = 64),
                agent_uri TEXT,
                token_jti TEXT,
                hash_version INTEGER NOT NULL DEFAULT {hash_version} CHECK(hash_version IN (1,2,3,4{version_suffix})),
                {column}
                CHECK((agent_uri IS NULL) = (token_jti IS NULL)),
                CHECK(hash_version != 1 OR agent_uri IS NULL)
                {check}
            );
            INSERT INTO audit_log_rebuild SELECT *{select_suffix} FROM audit_log ORDER BY id;"
    ))?;
    if table == AuditTable::Receipts {
        super::audit::receipts::backfill_legacy_links(conn)?;
    }
    conn.execute_batch(&format!(
        "DROP TRIGGER protect_audit_insert;
            DROP TRIGGER protect_audit_update;
            DROP TRIGGER protect_audit_delete;
            DROP TABLE audit_log;
            ALTER TABLE audit_log_rebuild RENAME TO audit_log;
            CREATE INDEX idx_audit_entity ON audit_log(entity, entity_id);
            CREATE INDEX idx_audit_company_time ON audit_log(company_slug, changed_at);
            CREATE INDEX idx_audit_action ON audit_log(action);
            CREATE INDEX idx_audit_agent ON audit_log(agent_uri);
            CREATE TRIGGER protect_audit_update BEFORE UPDATE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END;
            CREATE TRIGGER protect_audit_delete BEFORE DELETE ON audit_log
            BEGIN SELECT RAISE(ABORT, 'audit records cannot be deleted'); END;
            CREATE TRIGGER protect_audit_insert BEFORE INSERT ON audit_log
            WHEN NEW.id <= COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)
                 IS NOT COALESCE((SELECT MAX(id) FROM audit_log), 0)
              OR NEW.prev_hash IS NOT COALESCE((SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1), printf('%064d', 0))
              OR NEW.hash_version != {hash_version}
            BEGIN SELECT RAISE(ABORT, 'audit record must extend the hash chain'); END;
        "
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Admin, Granted};

    fn with_admin_fixture<T>(
        conn: &Connection,
        actor: &crate::db::Actor,
        change: impl for<'a> FnOnce(
            crate::identity::Granted<'a, crate::identity::Admin>,
        ) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        crate::db::audit::savepoint::with_audited_tx(conn, "sp_admin_fixture", |tx| {
            change(crate::identity::Granted::check(tx, actor)?)
        })
    }

    #[test]
    fn summary_reads_a_v22_snapshot_without_migration() -> Result<(), CliError> {
        use crate::commands::setup::{StatementImportSummary, statement_import_summary};
        use crate::db::{Actor, Db, create_company};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("legacy.db");
        let db = Db::open(&path, None)?;
        Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
            create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        db.conn()
            .execute("DROP TABLE setup_statement_batches", [])?;
        db.conn().execute(
            "DELETE FROM schema_version WHERE version > ?1",
            [BOOK_PROFILE_SCHEMA_VERSION],
        )?;
        drop(db);

        let snapshot = Db::open_compatible_snapshot(&path, None, BOOK_PROFILE_SCHEMA_VERSION)?;
        assert_eq!(
            statement_import_summary(&snapshot, &"acme".parse()?)?,
            StatementImportSummary::default()
        );
        assert_eq!(stored_schema(snapshot.read())?, BOOK_PROFILE_SCHEMA_VERSION);
        Ok(())
    }

    fn upgrade_plaintext_fixture(path: &std::path::Path) -> Result<super::super::Db, CliError> {
        use super::super::{Db, SnapshotOverwrite};
        let proof = VerifiedSnapshot::create(
            path,
            None,
            &path.with_extension("bk-backup"),
            SnapshotOverwrite::Refuse,
        )?;
        Db::open_after_snapshot(proof, None)
    }

    #[test]
    fn v20_preserves_v19_policy_proposals_and_mixed_hashes() -> Result<(), CliError> {
        use super::super::{Actor, audit, periods, proposals, verification};
        use mti::prelude::*;
        let conn = v17_connection()?;
        apply_v18(&conn)?;
        apply_v19(&conn)?;
        let owner = Actor::new("owner".parse()?);
        let id = "proposal".create_type_id::<V7>().to_string();
        let payload = proposals::PostingPayload {
            description: "Review".into(),
            metadata: None,
            currency: beankeeper::types::Currency::USD,
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("fixture date"),
            entries: Vec::new(),
            correlate: None,
            reference: None,
            on_conflict: super::super::ConflictStrategy::Error,
            purpose: crate::db::holds::PostingPurpose::default(),
        };
        conn.execute(
            "INSERT INTO txn_proposals(id, company_slug, status, payload, debit_total, currency, proposer_uri, proposer_jti)
             VALUES (?1, 'legacy', 'pending', ?2, 100, 'USD', 'proposer', 'jti')",
            params![id, serde_json::to_string(&payload).map_err(|e| CliError::General(e.to_string()))?],
        )?;
        reject_v19_proposal(&conn, &owner, &id)?;
        with_admin_fixture(&conn, &owner, |grant| audit::set_policy(grant, true))?;
        let history = |conn: &Connection| -> Result<Vec<(i64, String, String)>, CliError> {
            Ok(conn
                .prepare(
                    "SELECT hash_version, row_hash, COALESCE(after, '') FROM audit_log ORDER BY id",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?)
        };
        let before = history(&conn)?;
        assert_eq!(
            before.iter().map(|row| row.0).collect::<Vec<_>>(),
            [1, 2, 3, 3]
        );
        // Exercise the verified file-upgrade path as well as the migration chain.
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("v19.db");
        conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        let migrated = upgrade_plaintext_fixture(&path)?;
        assert_eq!(stored_schema(migrated.read())?, CURRENT_VERSION);
        assert_eq!(history(migrated.conn())?, before);
        crate::db::verification::VerifiedLedger::read(&migrated)?;
        ensure_schema(&conn)?;
        let reader = ReadConn::physical(&conn).at_schema(current_schema_version());
        assert_eq!(stored_schema(reader)?, CURRENT_VERSION);
        assert_eq!(history(&conn)?, before);
        assert!(super::super::policy::get(reader)?.enforce);
        assert_eq!(
            proposals::get(reader, &"legacy".parse()?, &id.parse()?)?
                .state
                .reason(),
            Some("v19 review")
        );
        proposals::verify_resolutions(reader)?;
        // The existing policy and proposal protection triggers survive rebuilding audit_log.
        assert!(
            conn.execute("UPDATE ledger_policy SET enforce = 0", [])
                .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE txn_proposals SET reason = 'forged' WHERE id = ?1",
                [&id]
            )
            .is_err()
        );
        for hash_version in [2, 3] {
            assert!(conn.execute(
                "INSERT INTO audit_log(id, actor, entity, entity_id, action, prev_hash, row_hash, hash_version)
                 SELECT id + 1, 'old-writer', 'company', 'legacy', 'create', row_hash, row_hash, ?1
                 FROM audit_log ORDER BY id DESC LIMIT 1", [hash_version],
            ).is_err());
        }
        audit::change_legacy_policy_fixture(&conn, &owner, false)?;
        with_admin_fixture(&conn, &owner, |grant| {
            audit::change_period(
                grant,
                &"legacy".parse()?,
                periods::PeriodChange::Close("2026-01-31".parse()?),
            )
        })?;
        assert!(
            periods::ensure_open(
                reader,
                &"legacy".parse()?,
                periods::parse_date("2026-01-31")?
            )
            .is_err()
        );
        let forged_reopen = conn.execute("UPDATE period_locks SET closed_through = NULL", []);
        assert!(forged_reopen.is_err());
        audit::verify_hash_chain(reader)?;
        audit::verify_creations(reader)?;
        verification::verify_accounting(reader)?;
        periods::verify(reader)?;
        assert_eq!(history(&conn)?.last().map(|row| row.0), Some(7));
        ensure_schema(&conn)?;
        Ok(())
    }

    fn reject_v19_proposal(
        conn: &Connection,
        owner: &super::super::Actor,
        id: &str,
    ) -> Result<(), CliError> {
        use crate::identity::{Granted, ResolveProposal};
        super::super::audit::savepoint::with_audited_tx(conn, "sp_proposal_fixture", |tx| {
            let grant = Granted::<ResolveProposal>::check(tx, owner)?;
            super::super::audit::reject_proposal(
                grant,
                &"legacy".parse()?,
                &id.parse()?,
                Some("v19 review"),
            )?;
            Ok(())
        })
    }

    #[test]
    fn v20_migration_rolls_back_corrupt_history_and_schema_collisions() -> Result<(), CliError> {
        for corruption in ["collision", "hash", "tail"] {
            let conn = v17_connection()?;
            apply_v18(&conn)?;
            apply_v19(&conn)?;
            match corruption {
                "hash" => conn.execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE id = 1")?,
                "tail" => conn.execute_batch("DROP TRIGGER protect_audit_delete; DELETE FROM audit_log WHERE id = 2")?,
                _ => conn.execute_batch("CREATE TABLE period_locks (collision TEXT)")?,
            }
            assert!(apply_v20(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 19);
            assert!(conn.prepare("SELECT * FROM audit_log_v20").is_err());
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('protect_policy_update', 'protect_proposal_resolution')", [], |row| row.get(0),
            )?;
            assert_eq!(count, 2);
            if corruption == "collision" {
                with_admin_fixture(
                    &conn,
                    &super::super::Actor::new("owner".parse()?),
                    |grant| super::super::audit::set_policy(grant, true),
                )?;
                super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
            }
        }
        Ok(())
    }

    #[test]
    fn v19_preserves_v18_policy_history_and_posted_hashes() -> Result<(), CliError> {
        let conn = v17_connection()?;
        apply_v18(&conn)?;
        let owner = super::super::Actor::new("owner".parse()?);
        with_admin_fixture(&conn, &owner, |grant| {
            super::super::audit::set_policy(grant, true)
        })?;
        let history = |conn: &Connection| -> Result<Vec<(i64, String, String)>, CliError> {
            Ok(conn
                .prepare("SELECT id, row_hash, COALESCE(after, '') FROM audit_log ORDER BY id")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?)
        };
        let before = history(&conn)?;
        apply_v19(&conn)?;
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 19);
        assert_eq!(history(&conn)?, before);
        assert!(super::super::policy::get(ReadConn::physical(&conn))?.enforce);
        super::super::audit::change_legacy_policy_fixture(&conn, &owner, false)?;
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        super::super::audit::verify_creations(ReadConn::physical(&conn))?;
        assert_eq!(
            super::super::spend_limits::list(ReadConn::physical(&conn), "unconfigured", None)?,
            Vec::new()
        );
        ensure_schema(&conn)?;
        Ok(())
    }

    #[test]
    fn v19_rebuild_is_atomic_and_rejects_corrupt_v18_history() -> Result<(), CliError> {
        for corruption in ["collision", "hash", "tail"] {
            let conn = v17_connection()?;
            apply_v18(&conn)?;
            match corruption {
                "hash" => conn.execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE id = 1")?,
                "tail" => conn.execute_batch("DROP TRIGGER protect_audit_delete; DELETE FROM audit_log WHERE id = 2")?,
                _ => conn.execute_batch("CREATE TABLE agent_spend_limits (collision TEXT)")?,
            }
            assert!(apply_v19(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 18);
            assert!(conn.prepare("SELECT * FROM txn_proposals").is_err());
            assert!(conn.prepare("SELECT * FROM audit_log_v19").is_err());
            if corruption == "collision" {
                with_admin_fixture(
                    &conn,
                    &super::super::Actor::new("owner".parse()?),
                    |grant| super::super::audit::set_policy(grant, true),
                )?;
                assert!(super::super::policy::get(ReadConn::physical(&conn))?.enforce);
                super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
            }
        }
        Ok(())
    }

    fn v17_connection() -> Result<Connection, CliError> {
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        for migration in [apply_v13, apply_v14, apply_v15, apply_v16, apply_v17] {
            migration(&conn)?;
        }
        super::super::audit::record_identity(
            &conn,
            &super::super::Actor::new("v17".parse()?),
            crate::db::audit::IdentityEvent::TokenRevoked,
            "legacy",
            &serde_json::json!({}),
        )?;
        Ok(conn)
    }

    // Historical writers cannot call the current account API, whose insert
    // includes later schema fields. Keep this v25 fixture and its audit append
    // together under the same savepoint as the original writer.
    fn seed_v25_account(conn: &Connection, actor: &super::super::Actor) -> Result<(), CliError> {
        super::super::audit::atomically(conn, || {
            conn.execute(
                "INSERT INTO accounts(company_slug, code, name, type)
                VALUES ('legacy', '1200', 'New cash account', 'asset')",
                [],
            )?;
            let created_at: String = conn.query_row(
                "SELECT created_at FROM accounts WHERE company_slug = 'legacy' AND code = '1200'",
                [],
                |row| row.get(0),
            )?;
            super::super::audit::record_insert(
                conn,
                actor,
                &"legacy".parse()?,
                super::super::audit::Creation::Account,
                "1200",
                &serde_json::json!({
                    "company_slug": "legacy", "code": "1200", "name": "New cash account",
                    "type": "asset", "default_tax_category": null, "created_at": created_at,
                }),
            )
        })
    }

    #[test]
    fn v25_preserves_all_earlier_hash_versions_and_requires_v5_appends() -> Result<(), CliError> {
        use super::super::{Actor, audit};

        let conn = v17_connection()?;
        let owner = Actor::new("owner".parse()?);
        let append = |id| {
            audit::record_identity(
                &conn,
                &owner,
                audit::IdentityEvent::TokenRevoked,
                id,
                &serde_json::json!({}),
            )
        };
        apply_v18(&conn)?;
        append("v3")?;
        apply_v19(&conn)?;
        apply_v20(&conn)?;
        append("v4")?;
        apply_v21(&conn)?;
        apply_v22(&conn)?;
        apply_v23(&conn)?;
        apply_v24(&conn)?;

        let history = || -> Result<Vec<(i64, String, String)>, CliError> {
            Ok(conn
                .prepare("SELECT hash_version, prev_hash, row_hash FROM audit_log ORDER BY id")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?)
        };
        let before = history()?;
        assert_eq!(
            before.iter().map(|row| row.0).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        apply_v25(&conn)?;
        assert_eq!(history()?, before);
        assert!(
            conn.execute(
                "INSERT INTO audit_log(id, actor, entity, entity_id, action, prev_hash, row_hash, hash_version)
                 SELECT id + 1, 'old-writer', 'company', 'legacy', 'create', row_hash, row_hash, 4
                 FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
            )
            .is_err()
        );
        append("v5")?;
        let mixed = history()?;
        assert_eq!(&mixed[..before.len()], before);
        assert_eq!(mixed.last().map(|row| row.0), Some(5));
        assert_eq!(
            audit::verify_hash_chain(ReadConn::physical(&conn))?.length(),
            5
        );
        seed_v25_account(&conn, &owner)?;
        let v25 = history()?;
        apply_v26(&conn)?;
        assert_eq!(history()?, v25);
        let company =
            crate::db::companies::Company::bind(ReadConn::physical(&conn), "legacy".parse()?)?;
        assert_eq!(
            crate::db::accounts::account_status(&company, "1200")?,
            crate::db::accounts::AccountStatus::Active
        );
        audit::receipts::verify_archives(ReadConn::physical(&conn))?;
        Ok(())
    }

    #[test]
    fn v26_commits_backfilled_receipt_links_without_rehashing_history() -> Result<(), CliError> {
        use sha2::{Digest, Sha256};

        let conn = v17_connection()?;
        for migration in [
            apply_v18, apply_v19, apply_v20, apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
        ] {
            migration(&conn)?;
        }
        conn.execute_batch(
            "INSERT INTO companies (slug, name) VALUES ('other', 'Other');
             INSERT INTO transactions (company_slug, description, currency, date) VALUES
               ('legacy', 'Second posting', 'USD', '2026-01-02'),
               ('other', 'Intercompany posting', 'USD', '2026-01-02');",
        )?;

        let previous: (i64, String) = conn.query_row(
            "SELECT id, row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut history = Vec::new();
        let mut previous = previous;
        for (entity, target, action, key, posting) in [
            ("transaction", "1", "reverse", "reversed_by", 2),
            (
                "proposal",
                "legacy-proposal",
                "proposal_approve",
                "transaction_id",
                2,
            ),
            ("transaction", "1", "correlate", "correlated_with", 3),
        ] {
            let id = previous.0 + 1;
            let after = serde_json::json!({ (key): posting }).to_string();
            let canonical = serde_json::json!([
                "beankeeper.audit.v5",
                id,
                "2026-01-02 00:00:00",
                "legacy-writer",
                "legacy",
                entity,
                target,
                action,
                null,
                after,
                null,
                null,
                null
            ])
            .to_string();
            let mut hash = Sha256::new();
            hash.update(previous.1.as_bytes());
            hash.update(canonical.as_bytes());
            let hash = format!("{:x}", hash.finalize());
            conn.execute(
                "INSERT INTO audit_log(id, changed_at, actor, company_slug, entity, entity_id,
                    action, after, prev_hash, row_hash, hash_version)
                 VALUES (?1, '2026-01-02 00:00:00', 'legacy-writer', 'legacy', ?2, ?3,
                    ?4, ?5, ?6, ?7, 5)",
                params![id, entity, target, action, after, previous.1, hash],
            )?;
            history.push((id, after, hash.clone(), posting));
            previous = (id, hash);
        }

        apply_v26(&conn)?;
        for (id, after, hash, posting) in history {
            let persisted: (String, String, i64) = conn.query_row(
                "SELECT after, row_hash, related_posting FROM audit_log WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(persisted, (after, hash, posting));
        }
        let reader = ReadConn::physical(&conn);
        assert_eq!(crate::db::audit::verify_hash_chain(reader)?.length(), 6);
        let tail: (i64, String) = conn.query_row(
            "SELECT hash_version, action FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(tail, (6, "receipt_link_migration".into()));

        // The next migration must retain the backfilled links and their v6 commitment.
        ensure_schema(&conn)?;
        assert_eq!(crate::db::audit::verify_hash_chain(reader)?.length(), 6);
        let projected_postings: i64 =
            conn.query_row("SELECT COUNT(*) FROM audit_receipt_postings", [], |row| {
                row.get(0)
            })?;
        assert_eq!(projected_postings, 2);

        conn.execute_batch(
            "DROP TRIGGER protect_audit_update;
             UPDATE audit_log SET related_posting = 1 WHERE action = 'reverse';",
        )?;
        let error = crate::db::audit::verify_hash_chain(reader)
            .expect_err("changing a legacy typed link must invalidate the commitment");
        assert!(error.to_string().contains("receipt links"));
        Ok(())
    }

    #[test]
    fn v27_keeps_existing_rules_active_and_migrates_atomically() -> Result<(), CliError> {
        use super::super::{Actor, audit, companies::Company, rules, standing};
        use crate::identity::Post;

        let conn = v17_connection()?;
        for migration in [
            apply_v18, apply_v19, apply_v20, apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
        ] {
            migration(&conn)?;
        }
        apply_v26(&conn)?;
        let company = "legacy".parse()?;
        let actor = Actor::new("owner".parse()?);
        audit::record_identity(
            &conn,
            &actor,
            audit::IdentityEvent::TokenRevoked,
            "v26",
            &serde_json::json!({}),
        )?;
        let tail = || -> Result<(audit::HashVersion, String, String), CliError> {
            Ok(conn.query_row(
                "SELECT hash_version, prev_hash, row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        };
        let v26_tail = tail()?;
        assert_eq!(v26_tail.0, audit::HashVersion::V6);
        let id = rules::RuleId::generate();
        conn.execute(
            "INSERT INTO categorization_rules (id, company_slug, contains, account_code, priority)
             VALUES (?1, ?2, 'sale', '4000', -7)",
            params![id.to_string(), &company],
        )?;
        // A failure during the audit table rebuild must undo the new rule column and settings table.
        conn.execute_batch("CREATE TABLE audit_log_rebuild (collision TEXT)")?;
        assert!(apply_v27(&conn).is_err());
        assert_eq!(
            stored_schema(ReadConn::physical(&conn))?,
            RECEIPT_SCHEMA_VERSION
        );
        assert!(
            conn.prepare("SELECT supersedes FROM categorization_rules")
                .is_err()
        );
        assert!(conn.prepare("SELECT * FROM standing_settings").is_err());
        assert!(conn.prepare("SELECT * FROM audit_receipt_postings").is_ok());
        conn.execute_batch("DROP TABLE audit_log_rebuild")?;

        apply_v27(&conn)?;
        assert_eq!(
            stored_schema(ReadConn::physical(&conn))?,
            STANDING_SETTINGS_SCHEMA_VERSION
        );
        assert_eq!(tail()?, v26_tail);
        let scope = Company::bind(ReadConn::physical(&conn), company)?;
        let active = rules::list_rules(&scope)?;
        let rule = rules::matching_rule(&active, "Retail SALE").expect("migrated match");
        assert_eq!(rule.id(), &id);
        assert_eq!(rule.supersedes(), None);
        assert_eq!(i32::from(rule.priority()), -7);
        assert_eq!(
            standing::bookkeeping_policies(&scope)?,
            standing::BookkeepingPolicies::default()
        );
        assert_eq!(
            standing::inform_preference(&scope)?,
            standing::InformPreference::default()
        );
        assert!(
            conn.execute(
                "INSERT INTO standing_settings(company_slug, sales_tax, processor_fees)
             VALUES ('legacy', 'separate-expense', 'unset')",
                [],
            )
            .is_err()
        );
        assert!(conn.execute(
            "INSERT INTO audit_log(id, actor, entity, entity_id, action, prev_hash, row_hash, hash_version)
             SELECT id + 1, 'old-writer', 'company', 'legacy', 'create', row_hash, row_hash, 6
             FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
        ).is_err());
        let reader = ReadConn::physical(&conn);
        let old_chain = audit::verify_hash_chain(reader)?;
        audit::savepoint::with_audited_tx(&conn, "sp_standing_fixture", |tx| {
            audit::set_inform_preference(
                Granted::<Post>::check(tx, &actor)?,
                scope.slug(),
                standing::InformPreference::DailyDigest,
            )
        })?;
        let (version, previous, _) = tail()?;
        assert_eq!(version, audit::HashVersion::V7);
        assert_eq!(previous, v26_tail.2);
        assert_eq!(
            audit::verify_hash_chain(reader)?.length(),
            old_chain.length() + 1
        );
        Ok(())
    }

    #[test]
    fn v28_preserves_existing_rule_rows_and_their_audit_hashes() -> Result<(), CliError> {
        use super::super::{Actor, audit, companies::Company, rules};
        use crate::identity::Post;

        let conn = v17_connection()?;
        for migration in [
            apply_v18, apply_v19, apply_v20, apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
            apply_v26, apply_v27,
        ] {
            migration(&conn)?;
        }
        let actor = Actor::new("bookkeeper".parse()?);
        let company = "legacy".parse()?;
        let rule = audit::savepoint::with_audited_tx(&conn, "sp_rule_fixture", |tx| {
            audit::create_rule(
                Granted::<Post>::check(tx, &actor)?,
                &company,
                (
                    "retail".parse()?,
                    "4000".parse().map_err(beankeeper::error::BeanError::from)?,
                    None,
                )
                    .into(),
            )
        })?;
        let reader = ReadConn::physical(&conn);
        let rule_hash = || -> Result<String, CliError> {
            Ok(conn.query_row(
                "SELECT row_hash FROM audit_log WHERE entity = 'rule' AND entity_id = ?1",
                [rule.id().to_string()],
                |row| row.get(0),
            )?)
        };
        let before = audit::verify_hash_chain(reader)?.length();
        let hash = rule_hash()?;
        apply_v28(&conn)?;
        assert_eq!(before, audit::verify_hash_chain(reader)?.length());
        assert_eq!(hash, rule_hash()?);
        assert_eq!(
            rules::list_active_rules(&Company::bind(reader, company)?)?,
            [rule]
        );
        assert_eq!(stored_schema(reader)?, RULE_UNDO_SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn v29_through_current_preserve_setup_profiles_and_audit_chain() -> Result<(), CliError> {
        let conn = v17_connection()?;
        for migrate in [
            apply_v18, apply_v19, apply_v20, apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
            apply_v26, apply_v27, apply_v28,
        ] {
            migrate(&conn)?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"INSERT INTO agent_tokens VALUES ('setup-token', 'setup-agent', 'key',
                '["ledger/setup"]', 'issued', 'expires');
             INSERT INTO setup_sessions VALUES (1, 'setup-token', 'active', NULL);
             INSERT INTO setup_book_profiles VALUES
                (3, 'legacy', 'personal', 'USD', '01-01', NULL, 1),
                (7, 'legacy', 'personal', 'USD', '04-01', 3, 1);"#,
        )?;
        let reader = ReadConn::physical(&conn);
        let audit_head = || -> Result<String, CliError> {
            Ok(conn.query_row(
                "SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )?)
        };
        let before = audit_head()?;
        apply_v29(&conn)?;
        assert_eq!(stored_schema(reader)?, DIRECT_SETUP_SCHEMA_VERSION);
        apply_v30(&conn)?;
        assert_eq!(stored_schema(reader)?, ANNOTATION_UNDO_SCHEMA_VERSION);
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(reader)?, CURRENT_VERSION);
        let mut migrations =
            conn.prepare("SELECT version FROM schema_version WHERE version >= ?1 ORDER BY rowid")?;
        let versions = migrations
            .query_map([ANNOTATION_UNDO_SCHEMA_VERSION], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        assert_eq!(
            versions,
            [
                ANNOTATION_UNDO_SCHEMA_VERSION,
                HOLDS_SCHEMA_VERSION,
                ACCOUNT_RENAME_SCHEMA_VERSION,
                CORRECTION_SCHEMA_VERSION,
                RECATEGORIZATION_SCHEMA_VERSION,
                TXN_LINK_SCHEMA_VERSION,
                STATEMENT_RECONCILIATION_SCHEMA_VERSION,
                FX_SCHEMA_VERSION,
                RESTORE_SCHEMA_VERSION,
                CASH_CLASS_SCHEMA_VERSION,
            ]
            .map(i64::from)
        );
        assert_eq!(audit_head()?, before);
        super::super::audit::verify_hash_chain(reader)?;
        let preserved: (i64, i64, String) = conn.query_row(
            "SELECT supersedes, generation, fiscal_year_start FROM setup_book_profiles WHERE revision = 7",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(preserved, (3, 1, "04-01".into()));
        let indexes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
             AND name IN ('idx_direct_setup_reference', 'idx_direct_setup_reverts', 'idx_hold_reversals')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(indexes, 3);
        Ok(())
    }

    #[test]
    fn v36_preserves_pending_holds_and_audited_links() -> Result<(), CliError> {
        use super::super::{
            Actor, audit,
            holds::EntryStatus,
            transactions::{
                ConflictStrategy, PostEntryParams, PostTransactionParams, PostingSource,
                post_from_source_with_status,
            },
        };
        use crate::identity::Post;
        use crate::request::CalendarDateArg;
        use beankeeper::types::{Currency, DebitOrCredit};

        let conn = v17_connection()?;
        for migrate in [
            apply_v18, apply_v19, apply_v20, apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
            apply_v26, apply_v27, apply_v28, apply_v29, apply_v30, apply_v31,
        ] {
            migrate(&conn)?;
        }
        let reader = ReadConn::physical(&conn);
        assert_eq!(stored_schema(reader)?, HOLDS_SCHEMA_VERSION);
        let actor = Actor::new("v31-bookkeeper".parse()?);
        let amount = 250_i64.try_into()?;
        let entries = [
            ("4000", DebitOrCredit::Debit),
            ("1000", DebitOrCredit::Credit),
        ]
        .map(|(code, direction)| PostEntryParams {
            account_code: code.into(),
            direction,
            amount,
            memo: None,
            tax_category: None,
        });
        let request = PostTransactionParams {
            company_slug: &"legacy".parse()?,
            description: "Pending card hold",
            metadata: None,
            currency: Currency::USD,
            date: "2026-01-02".parse::<CalendarDateArg>()?.into(),
            entries: &entries,
            correlate: None,
            reference: Some("v31-hold"),
            on_conflict: ConflictStrategy::Error,
            actor: &actor,
        };
        audit::savepoint::with_audited_tx(&conn, "sp_hold_fixture", |tx| {
            post_from_source_with_status(
                Granted::<Post>::check(tx, &actor)?,
                &request,
                PostingSource::Command,
                EntryStatus::Pending,
            )
        })?;
        let hold_adjustment = || -> Result<i64, CliError> {
            Ok(conn.query_row(
                "SELECT net FROM hold_adjustments WHERE company_slug = 'legacy'
                 AND account_code = '1000'",
                [],
                |row| row.get(0),
            )?)
        };
        assert_eq!(hold_adjustment()?, -250);
        let audit_length = audit::verify_hash_chain(reader)?.length();

        conn.execute_batch("CREATE TABLE audit_log_rebuild (collision TEXT)")?;
        assert!(ensure_schema(&conn).is_err());
        assert_eq!(stored_schema(reader)?, HOLDS_SCHEMA_VERSION);
        assert_eq!(hold_adjustment()?, -250);
        conn.execute_batch("DROP TABLE audit_log_rebuild")?;

        apply_v32(&conn)?;
        migrate_hold_audit(&conn, AuditTable::Correction, CORRECTION_SCHEMA_VERSION)?;
        assert_eq!(stored_schema(reader)?, CORRECTION_SCHEMA_VERSION);
        assert_statement_upgrade_preserves_hold_history(&conn, audit_length)
    }

    fn assert_statement_upgrade_preserves_hold_history(
        conn: &Connection,
        audit_length: u64,
    ) -> Result<(), CliError> {
        use super::super::audit;
        let reader = ReadConn::physical(conn);
        let hashes = |reader: ReadConn<'_>| -> Result<Vec<(String, String)>, CliError> {
            let mut stmt =
                reader.prepare("SELECT prev_hash, row_hash FROM audit_log ORDER BY id")?;
            Ok(stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?)
        };
        let hold_history = hashes(reader)?;
        migrate_hold_audit(
            conn,
            AuditTable::Recategorization,
            RECATEGORIZATION_SCHEMA_VERSION,
        )?;
        assert_eq!(stored_schema(reader)?, RECATEGORIZATION_SCHEMA_VERSION);
        apply_v35(conn)?;
        assert_eq!(stored_schema(reader)?, TXN_LINK_SCHEMA_VERSION);
        assert_eq!(hashes(reader)?, hold_history);
        let link = seed_v35_link(conn)?;
        let history = hashes(reader)?;
        let active_link = |reader: ReadConn<'_>| -> Result<_, CliError> {
            Ok(reader.query_row(
                "SELECT receipt FROM active_transaction_links WHERE company_slug = 'legacy'",
                [],
                |row| row.get::<_, crate::db::ids::ReceiptId>(0),
            )?)
        };
        assert_eq!(active_link(reader)?, link);

        // Failing after the audit rebuild must roll back both the schema version
        // and the audit rebuild, leaving the original ledger usable.
        conn.execute_batch("CREATE TABLE statement_reconciliation_clearings (collision TEXT)")?;
        assert!(ensure_schema(conn).is_err());
        assert_eq!(stored_schema(reader)?, TXN_LINK_SCHEMA_VERSION);
        assert_eq!(hashes(reader)?, history);
        assert_eq!(active_link(reader)?, link);
        let table_exists: bool = reader.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'statement_reconciliations')",
            [],
            |row| row.get(0),
        )?;
        assert!(!table_exists);
        conn.execute_batch("DROP TABLE statement_reconciliation_clearings")?;
        assert_failed_fx_migration_preserves_statement_schema(conn)?;
        assert_eq!(hashes(reader)?, history);
        assert_eq!(active_link(reader)?, link);
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("before-fx.db");
        conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        let migrated = upgrade_plaintext_fixture(&path)?;
        let migrated_reader = migrated.read();
        assert_eq!(stored_schema(migrated_reader)?, CURRENT_VERSION);
        assert_eq!(hashes(migrated_reader)?, history);
        assert_eq!(active_link(migrated_reader)?, link);
        assert_new_statement_and_fx_tables(migrated_reader)?;
        assert_restore_sql_uses_bookkeeping_history(migrated_reader)?;
        assert_eq!(
            audit::verify_hash_chain(migrated_reader)?.length(),
            audit_length + 1
        );
        super::super::verification::VerifiedLedger::read(&migrated)?;
        let net: i64 = migrated_reader.query_row(
            "SELECT net FROM hold_adjustments WHERE company_slug='legacy' AND account_code='1000'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(net, -250);
        assert!(
            migrated
                .conn()
                .execute(
                    "UPDATE entries SET amount = 1 WHERE transaction_id IN (
                SELECT id FROM transactions WHERE reference = 'v31-hold')",
                    [],
                )
                .is_err()
        );
        for name in [
            "protect_hold_settlement",
            "posting_reversals",
            "audit_receipt_postings",
            "idx_correction_proposal",
            "idx_correction_company",
            "active_transaction_links",
            "protect_transaction_link_insert",
            "protect_transaction_link_undo",
        ] {
            let present: bool = migrated_reader.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                [name],
                |row| row.get(0),
            )?;
            assert!(present, "{name} was lost in migration");
        }
        ensure_schema(migrated.conn())?;
        Ok(())
    }

    fn assert_restore_sql_uses_bookkeeping_history(reader: ReadConn<'_>) -> Result<(), CliError> {
        for name in [
            "posting_reversals",
            "transaction_link_creations",
            "active_transaction_links",
            "protect_deactivated_rule_insert",
            "protect_deactivated_rule_update",
            "protect_deactivated_rule_delete",
            "protect_bookkeeping_receipt_uniqueness",
        ] {
            let sql: String = reader.query_row(
                "SELECT sql FROM sqlite_schema WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?;
            assert!(sql.contains("FROM bookkeeping_audit"), "{name}: {sql}");
            assert!(!sql.contains("FROM audit_log"), "{name}: {sql}");
        }
        Ok(())
    }

    fn assert_failed_fx_migration_preserves_statement_schema(
        conn: &Connection,
    ) -> Result<(), CliError> {
        // The second FX table fails to create. Its migration must roll back
        // without undoing the preceding statement reconciliation migration.
        conn.execute_batch("CREATE TABLE fx_postings (collision TEXT)")?;
        assert!(ensure_schema(conn).is_err());
        let reader = ReadConn::physical(conn);
        assert_eq!(
            stored_schema(reader)?,
            STATEMENT_RECONCILIATION_SCHEMA_VERSION
        );
        let table_exists = |table: &str| {
            reader.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
        };
        assert!(table_exists("statement_reconciliations")?);
        assert!(table_exists("statement_reconciliation_clearings")?);
        assert!(!table_exists("fx_accounts")?);
        conn.execute_batch("DROP TABLE fx_postings")?;
        Ok(())
    }

    fn assert_new_statement_and_fx_tables(reader: ReadConn<'_>) -> Result<(), CliError> {
        for table in [
            "statement_reconciliations",
            "statement_reconciliation_clearings",
            "fx_accounts",
            "fx_postings",
        ] {
            let empty: bool = reader.query_row(
                &format!("SELECT NOT EXISTS (SELECT 1 FROM {table})"),
                [],
                |row| row.get(0),
            )?;
            assert!(empty);
            let strict_table: bool = reader.query_row(
                "SELECT strict = 1 AND type = 'table' FROM pragma_table_list WHERE name = ?1",
                [table],
                |row| row.get(0),
            )?;
            assert!(strict_table);
        }
        let foreign_key_violations: i64 =
            reader.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        assert_eq!(foreign_key_violations, 0);
        Ok(())
    }

    fn seed_v35_link(conn: &Connection) -> Result<crate::db::ids::ReceiptId, CliError> {
        use crate::db::{
            Actor,
            audit::{savepoint::with_audited_tx, txn_links},
            ids::TransactionId,
        };
        use crate::identity::Post;

        let transaction = |reference| {
            conn.query_row(
                "SELECT id FROM transactions WHERE company_slug = 'legacy' AND reference = ?1",
                [reference],
                |row| row.get::<_, TransactionId>(0),
            )
        };
        let spec = (
            transaction("old")?,
            transaction("v31-hold")?,
            txn_links::TxnLinkKind::Related,
        )
            .try_into()?;
        let actor = Actor::new("bookkeeper".parse()?);
        with_audited_tx(conn, "sp_link_fixture", |tx| {
            Ok(
                txn_links::create(Granted::<Post>::check(tx, &actor)?, "legacy".parse()?, spec)?
                    .receipt(),
            )
        })
    }

    #[test]
    fn v18_preserves_mixed_history_and_blocks_old_writers() -> Result<(), CliError> {
        let conn = v17_connection()?;
        let hashes = |conn: &Connection| -> Result<Vec<(i64, String, String)>, CliError> {
            let mut statement =
                conn.prepare("SELECT hash_version, row_hash, after FROM audit_log ORDER BY id")?;
            Ok(statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?)
        };
        let before = hashes(&conn)?;
        assert_eq!(before.iter().map(|row| row.0).collect::<Vec<_>>(), [1, 2]);
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        assert_eq!(hashes(&conn)?, before);
        assert!(!super::super::policy::get(ReadConn::physical(&conn))?.enforce);
        // Simulate the explicit v2 insert emitted by a v17 binary.
        let result = conn.execute("INSERT INTO audit_log (id, actor, entity, entity_id, action, prev_hash, row_hash, hash_version)
            SELECT id + 1, 'old-writer', 'company', 'legacy', 'create', row_hash, row_hash, 2 FROM audit_log ORDER BY id DESC LIMIT 1", []);
        assert!(result.is_err());
        with_admin_fixture(
            &conn,
            &super::super::Actor::new("owner".parse()?),
            |grant| super::super::audit::set_policy(grant, true),
        )?;
        assert_eq!(hashes(&conn)?.last().map(|row| row.0), Some(7));
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        super::super::audit::verify_creations(ReadConn::physical(&conn))?;
        for sql in [
            "DELETE FROM audit_log",
            "UPDATE audit_log SET actor = 'tampered'",
            "DELETE FROM ledger_policy",
            "UPDATE ledger_policy SET enforce = 0",
        ] {
            assert!(conn.execute(sql, []).is_err(), "unprotected: {sql}");
        }
        let indexes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'audit_log'
               AND name IN ('idx_audit_entity', 'idx_audit_company_time', 'idx_audit_action', 'idx_audit_agent')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(indexes, 4);
        ensure_schema(&conn)?;
        assert!(super::super::policy::get(ReadConn::physical(&conn))?.enforce);
        Ok(())
    }

    #[test]
    fn v18_rejects_corruption_and_rolls_back_a_failed_rebuild() -> Result<(), CliError> {
        for corruption in ["collision", "hash", "tail"] {
            let conn = v17_connection()?;
            match corruption {
                "hash" => conn.execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE id = 1")?,
                "tail" => conn.execute_batch("DROP TRIGGER protect_audit_delete; DELETE FROM audit_log WHERE id = 2")?,
                _ => conn.execute_batch("CREATE TABLE ledger_policy (collision TEXT)")?,
            }
            assert!(apply_v18(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 17);
            assert!(conn.prepare("SELECT * FROM audit_log_v18").is_err());
            if corruption == "collision" {
                // Rebuild had already run before the collision; its trigger change rolls back.
                super::super::audit::record_identity(
                    &conn,
                    &super::super::Actor::new("v17".parse()?),
                    crate::db::audit::IdentityEvent::TokenRevoked,
                    "still-v17",
                    &serde_json::json!({}),
                )?;
                conn.execute_batch("DROP TABLE ledger_policy")?;
                apply_v18(&conn)?;
            }
        }
        Ok(())
    }

    #[test]
    fn fresh_schema_defaults_off_without_inventing_policy_history() -> Result<(), CliError> {
        let conn = open_conn();
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        assert!(!super::super::policy::get(ReadConn::physical(&conn))?.enforce);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM audit_log", [], |row| row
                .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn v17_preserves_hashes_triggers_and_rolls_back_failed_migration() -> Result<(), CliError> {
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        apply_v13(&conn)?;
        apply_v14(&conn)?;
        apply_v15(&conn)?;
        apply_v16(&conn)?;
        let before: Vec<(i64, String, String)> = {
            let mut stmt =
                conn.prepare("SELECT id, prev_hash, row_hash FROM audit_log ORDER BY id")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        // Fail after creating the investment tables, while rebuilding auditing.
        conn.execute_batch("CREATE TABLE audit_log_v17 (collision TEXT)")?;
        assert!(apply_v17(&conn).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 16);
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('securities','security_prices','investment_lots','lot_reliefs')",
            [], |r| r.get(0),
        )?;
        assert_eq!(tables, 0);
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        conn.execute_batch("DROP TABLE audit_log_v17")?;
        apply_v17(&conn)?;
        let after: Vec<(i64, String, String)> = {
            let mut stmt =
                conn.prepare("SELECT id, prev_hash, row_hash FROM audit_log ORDER BY id")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        assert_eq!(before, after);
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        super::super::audit::verify_creations(ReadConn::physical(&conn))?;
        for sql in [
            "DELETE FROM audit_log",
            "UPDATE audit_log SET actor = 'tampered'",
            "DELETE FROM entries",
            "UPDATE entries SET amount = 42",
            "DELETE FROM transactions",
            "UPDATE transactions SET description = 'tampered'",
        ] {
            assert!(conn.execute(sql, []).is_err(), "unprotected: {sql}");
        }
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn v16_preserves_hashes_and_rolls_back_failed_migration() -> Result<(), CliError> {
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        apply_v13(&conn)?;
        apply_v14(&conn)?;
        apply_v15(&conn)?;
        let historical: String = conn.query_row(
            "SELECT row_hash FROM audit_log ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        conn.execute_batch("CREATE TABLE schedules (collision TEXT)")?;
        assert!(apply_v16(&conn).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 15);
        conn.execute_batch("DROP TABLE schedules")?;
        apply_v16(&conn)?;
        let retained: String = conn.query_row(
            "SELECT row_hash FROM audit_log ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(historical, retained);
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        super::super::audit::verify_creations(ReadConn::physical(&conn))?;
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        assert!(
            conn.execute("UPDATE audit_log SET actor = 'tampered'", [])
                .is_err()
        );
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn v15_preserves_history_and_metadata_edits_extend_the_chain() -> Result<(), CliError> {
        use super::super::{Actor, audit, metadata::MetadataEdit};
        use crate::identity::{Granted, Post};
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        apply_v13(&conn)?;
        apply_v14(&conn)?;
        let historical: (String, String) = conn.query_row(
            "SELECT prev_hash, row_hash FROM audit_log ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure_schema(&conn)?;
        let retained: (String, String) = conn.query_row(
            "SELECT prev_hash, row_hash FROM audit_log ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(historical, retained);
        let id: i64 =
            conn.query_row("SELECT id FROM transactions LIMIT 1", [], |row| row.get(0))?;
        for edit in [
            MetadataEdit::Set(vec![("status".into(), "disputed".into())]),
            MetadataEdit::Unset(vec!["status".into()]),
        ] {
            let changed = audit::savepoint::with_audited_tx(&conn, "sp_metadata_fixture", |tx| {
                let actor = Actor::new("v15".parse()?);
                let grant = Granted::<Post>::check(tx, &actor)?;
                audit::edit_metadata(
                    grant,
                    &"legacy".parse()?,
                    audit::MetadataTarget::Id(id.try_into()?),
                    &edit,
                )
            })?;
            assert!(changed.changed);
        }
        audit::verify_hash_chain(ReadConn::physical(&conn))?;
        audit::verify_creations(ReadConn::physical(&conn))?;
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        assert!(
            conn.execute("UPDATE audit_log SET actor = 'tampered'", [])
                .is_err()
        );
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn v15_rejects_corrupt_history_and_rolls_back_failed_migrations() -> Result<(), CliError> {
        for corrupt in [false, true] {
            let conn = v12_connection()?;
            seed_v12_creation(&conn)?;
            apply_v13(&conn)?;
            apply_v14(&conn)?;
            if corrupt {
                conn.execute_batch("DROP TRIGGER protect_audit_delete; DELETE FROM audit_log;")?;
            } else {
                conn.execute_batch("CREATE TABLE audit_log_v15 (collision TEXT);")?;
            }
            assert!(apply_v15(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 14);
        }
        Ok(())
    }

    #[test]
    fn v14_preserves_history_and_extends_the_chain_with_rules() -> Result<(), CliError> {
        use super::super::{Actor, audit, rules};
        use crate::identity::{Granted, Post};

        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        apply_v13(&conn)?;
        audit::record_identity(
            &conn,
            &Actor::new("v13".parse()?),
            crate::db::audit::IdentityEvent::TokenRevoked,
            "test",
            &serde_json::json!({}),
        )?;
        let hashes = |conn: &Connection| -> Result<Vec<String>, CliError> {
            let mut stmt = conn.prepare("SELECT row_hash FROM audit_log ORDER BY id")?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?)
        };
        let before = hashes(&conn)?;
        ensure_schema(&conn)?;
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        assert_eq!(hashes(&conn)?, before);
        let actor = Actor::new("v14".parse()?);
        let rule = audit::savepoint::with_audited_tx(&conn, "sp_rule_fixture", |tx| {
            let grant = Granted::<Post>::check(tx, &actor)?;
            rules::create_rule(
                grant,
                &"legacy".parse()?,
                (
                    "sale".parse()?,
                    "4000".parse().expect("fixture account code"),
                    None,
                )
                    .into(),
            )
        })?;
        audit::savepoint::with_audited_tx(&conn, "sp_rule_fixture", |tx| {
            let grant = Granted::<Post>::check(tx, &actor)?;
            audit::delete_rule(
                grant,
                &"legacy".parse()?,
                rule.id(),
                crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
            )
        })?;
        assert_eq!(&hashes(&conn)?[..2], before.as_slice());
        audit::verify_hash_chain(ReadConn::physical(&conn))?;
        audit::verify_creations(ReadConn::physical(&conn))?;
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        assert!(
            conn.execute("UPDATE audit_log SET actor = 'tampered'", [])
                .is_err()
        );
        ensure_schema(&conn)?;
        assert_eq!(hashes(&conn)?.len(), 4);
        Ok(())
    }

    #[test]
    fn v14_rejects_corrupt_history_and_rolls_back_failed_migrations() -> Result<(), CliError> {
        for corrupt in [false, true] {
            let conn = v12_connection()?;
            seed_v12_creation(&conn)?;
            apply_v13(&conn)?;
            if corrupt {
                conn.execute_batch("DROP TRIGGER protect_audit_delete; DELETE FROM audit_log;")?;
            } else {
                conn.execute_batch("CREATE TABLE audit_log_v14 (collision TEXT);")?;
            }
            assert!(apply_v14(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 13);
            assert!(conn.prepare("SELECT * FROM categorization_rules").is_err());
        }
        Ok(())
    }

    fn v12_connection() -> Result<Connection, CliError> {
        let conn = open_conn();
        apply_through_v12(&conn)?;
        Ok(conn)
    }

    fn apply_through_v12(conn: &Connection) -> Result<(), CliError> {
        for migration in [
            apply_v1, apply_v2, apply_v3, apply_v4, apply_v5, apply_v6, apply_v7, apply_v8,
            apply_v9, apply_v10, apply_v11, apply_v12,
        ] {
            migration(conn)?;
        }
        Ok(())
    }

    #[test]
    fn restore_migrates_a_v37_copy_and_keeps_a_statement_receipt() -> Result<(), CliError> {
        use crate::db::{
            Actor, Db, SnapshotOverwrite, VerifiedSnapshot,
            audit::restore::RestoreRequest,
            audit::statement::{StatementInput, reconcile},
            statement::{StatementClose, StatementSelection},
            verification::VerifiedLedger,
        };
        use crate::identity::{self, Reconcile};
        use std::time::Duration;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("books.db");
        let backup = directory.path().join("old.bk-backup");
        let key = secrecy::SecretString::from("legacy-restore-test");
        let conn = super::super::connection::open_keyed(
            &path,
            Some(&key),
            rusqlite::OpenFlags::default(),
        )?;
        conn.pragma_update(None, "foreign_keys", true)?;
        apply_through_v12(&conn)?;
        seed_v12_creation(&conn)?;
        ensure_schema_at_before_restore(&conn)?;
        let old = Db::from_keyed_connection(conn, None, FX_SCHEMA_VERSION);
        let owner = Actor::new("owner".parse()?);
        let company = "legacy".parse()?;
        Granted::<Reconcile>::with_db(&old, &owner, |grant| {
            reconcile(
                grant,
                &company,
                StatementInput {
                    account: "1000".parse().map_err(beankeeper::error::BeanError::from)?,
                    month: "2026-01-31".parse()?,
                    ending: 0_u32.into(),
                    currency: Some(beankeeper::types::Currency::USD),
                    statement_ref: None,
                    selection: StatementSelection::default(),
                    close: StatementClose::default(),
                },
            )
        })?;
        Granted::<Admin>::with_db(&old, &owner, identity::init_root)?;
        let credential = Granted::<Admin>::with_db(&old, &owner, |grant| {
            identity::issue(
                grant,
                &identity::IssueRequest {
                    name: "restore",
                    capabilities: &["ledger/admin".into()],
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        drop(old);

        let proof =
            VerifiedSnapshot::create(&path, Some(&key), &backup, SnapshotOverwrite::Refuse)?;
        assert_eq!(proof.schema_version(), FX_SCHEMA_VERSION);
        let original = std::fs::read(&backup)?;
        let live = Db::open_after_snapshot(proof, Some(&key))?;
        let token = credential["token"]
            .as_str()
            .ok_or_else(|| CliError::Database("missing test token".into()))?;
        let owner = owner.with_identity(identity::verify(live.read(), token)?);
        Granted::<Admin>::with_db(&live, &owner, |grant| {
            super::super::create_company(grant, &"extra".parse()?, &"Extra".parse()?, None)
        })?;
        let request = RestoreRequest::Backup(backup.clone().try_into()?);
        let receipt = request.execute(&live, &owner)?;
        assert_eq!(std::fs::read(backup)?, original);
        let count: i64 = live.read().query_row(
            "SELECT COUNT(*) FROM statement_reconciliations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        assert!(!super::super::company_exists(
            live.read(),
            &"extra".parse()?
        )?);
        RestoreRequest::Undo(receipt.restore_id()).execute(&live, &owner)?;
        assert!(super::super::company_exists(
            live.read(),
            &"extra".parse()?
        )?);
        VerifiedLedger::read(&live)?;
        Ok(())
    }

    #[test]
    fn v39_classifies_legacy_cash_once_without_changing_audit_history() -> Result<(), CliError> {
        let conn = Connection::open_in_memory()?;
        apply_through_v12(&conn)?;
        ensure_schema_at_before_restore(&conn)?;
        apply_restore_migration(&conn)?;
        assert_eq!(
            stored_schema(ReadConn::physical(&conn))?,
            RESTORE_SCHEMA_VERSION
        );
        conn.execute_batch(
            "INSERT INTO companies(slug, name) VALUES ('legacy', 'Legacy');
            INSERT INTO accounts(company_slug, code, name, type) VALUES
            ('legacy', '1000', 'Checking', 'asset'),
            ('legacy', '1100', 'Travel Wallet', 'asset'),
            ('legacy', '2000', 'Bank Loan', 'liability');",
        )?;
        super::super::audit::record_identity(
            &conn,
            &super::super::Actor::new("migration fixture".parse()?),
            super::super::audit::IdentityEvent::TokenRevoked,
            "legacy-token",
            &serde_json::json!({}),
        )?;
        let history = |conn: &Connection| -> Result<Vec<String>, CliError> {
            Ok(conn
                .prepare("SELECT row_hash FROM audit_log ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let before = history(&conn)?;
        assert_eq!(before.len(), 1);
        ensure_schema(&conn)?;
        assert_eq!(history(&conn)?, before);
        let classes = |conn: &Connection| -> Result<Vec<String>, CliError> {
            Ok(conn
                .prepare("SELECT cash_class FROM accounts ORDER BY code")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        };
        assert_eq!(classes(&conn)?, ["cash", "non_cash", "non_cash"]);
        conn.execute(
            "UPDATE accounts SET name = 'Cash Wallet' WHERE code = '1100'",
            [],
        )?;
        ensure_schema(&conn)?;
        assert_eq!(classes(&conn)?, ["cash", "non_cash", "non_cash"]);
        assert_restore_sql_uses_bookkeeping_history(ReadConn::physical(&conn))?;
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        Ok(())
    }

    fn ensure_schema_at_before_restore(conn: &Connection) -> Result<(), CliError> {
        for migrate in [
            apply_v13, apply_v14, apply_v15, apply_v16, apply_v17, apply_v18, apply_v19, apply_v20,
            apply_v21, apply_v22, apply_v23, apply_v24, apply_v25,
        ] {
            migrate(conn)?;
        }
        apply_receipt_migrations(conn, CLIENT_HINT_SCHEMA_VERSION)?;
        apply_hold_audit_migrations(conn, ANNOTATION_UNDO_SCHEMA_VERSION)?;
        apply_fx_migration(conn)
    }

    #[test]
    fn encrypted_v12_snapshot_proves_legacy_postings_before_upgrade() -> Result<(), CliError> {
        use super::super::{Db, SnapshotOverwrite, VerifiedSnapshot};
        use secrecy::SecretString;

        let directory = tempfile::tempdir()?;
        let source = directory.path().join("ledger.db");
        let destination = directory.path().join("upgrade.bk-backup");
        let passphrase = SecretString::from("legacy-test-secret");
        let conn = super::super::connection::open_keyed(
            &source,
            Some(&passphrase),
            rusqlite::OpenFlags::default(),
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        apply_through_v12(&conn)?;
        seed_v12_creation(&conn)?;
        let hashes = |reader: ReadConn<'_>| -> Result<Vec<String>, CliError> {
            Ok(reader
                .prepare("SELECT row_hash FROM audit_log ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let before = hashes(ReadConn::physical(&conn))?;

        assert!(
            VerifiedSnapshot::create(&source, None, &destination, SnapshotOverwrite::Refuse)
                .is_err()
        );
        assert!(!destination.exists());
        let proof = VerifiedSnapshot::create(
            &source,
            Some(&passphrase),
            &destination,
            SnapshotOverwrite::Refuse,
        )?;
        assert_eq!(i64::from(proof.schema_version()), 12);
        assert_eq!(proof.path(), destination.canonicalize()?);
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 12);
        assert!(proof.encrypted());
        assert!(!std::fs::read(&destination)?.starts_with(b"SQLite format 3\0"));
        let copy = Db::open_compatible_snapshot(
            &destination,
            Some(&passphrase),
            HASH_CHAIN_SCHEMA_VERSION,
        )?;
        assert_eq!(hashes(copy.read())?, before);
        assert_eq!(
            proof.verification().schema_version(),
            proof.schema_version()
        );
        assert!(
            Db::open_compatible_snapshot(&destination, None, HASH_CHAIN_SCHEMA_VERSION).is_err()
        );
        let upgraded = Db::open_after_snapshot(proof, Some(&passphrase))?;
        assert_eq!(stored_schema(upgraded.read())?, CURRENT_VERSION);
        assert_eq!(hashes(upgraded.read())?, before);
        assert_eq!(get_schema_version(copy.read())?, 12);
        Ok(())
    }

    #[test]
    fn v1_upgrade_requires_an_unchanged_source_and_published_copy() -> Result<(), CliError> {
        use super::super::{Db, SnapshotOverwrite, VerifiedSnapshot};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ledger.db");
        let destination = directory.path().join("upgrade.bk-backup");
        let source = Connection::open(&path)?;
        apply_v1(&source)?;
        let snapshot =
            || VerifiedSnapshot::create(&path, None, &destination, SnapshotOverwrite::Replace);

        let proof = snapshot()?;
        source.execute(
            "INSERT INTO companies (slug, name) VALUES ('later', 'Later')",
            [],
        )?;
        assert!(Db::open_after_snapshot(proof, None).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&source))?, 1);

        let proof = snapshot()?;
        std::fs::remove_file(&destination)?;
        assert!(Db::open_after_snapshot(proof, None).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&source))?, 1);

        drop(source);
        let upgraded = Db::open_after_snapshot(snapshot()?, None)?;
        assert_eq!(stored_schema(upgraded.read())?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn snapshot_failure_preserves_destination_and_migration_failure_keeps_snapshot()
    -> Result<(), CliError> {
        use super::super::{Db, SnapshotOverwrite, VerifiedSnapshot};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ledger.db");
        let destination = directory.path().join("upgrade.bk-backup");
        let source = Connection::open(&path)?;
        apply_v1(&source)?;
        std::fs::write(&destination, b"keep this copy")?;
        source.execute_batch(
            "INSERT INTO companies (slug, name) VALUES ('legacy', 'Legacy');
             INSERT INTO transactions (company_slug, description, date) VALUES ('legacy', 'Unbalanced', '2026-01-01');",
        )?;
        let snapshot =
            || VerifiedSnapshot::create(&path, None, &destination, SnapshotOverwrite::Replace);
        assert!(
            matches!(snapshot(), Err(CliError::Database(message)) if message.contains("accounting failed"))
        );
        assert_eq!(std::fs::read(&destination)?, b"keep this copy");
        assert_eq!(get_schema_version(ReadConn::physical(&source))?, 1);
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 2);

        source.execute_batch(
            "DELETE FROM transactions; CREATE TABLE identity_root (collision TEXT);",
        )?;
        let proof = snapshot()?;
        let published = std::fs::read(&destination)?;
        assert!(Db::open_after_snapshot(proof, None).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&source))?, 1);
        assert_eq!(std::fs::read(&destination)?, published);
        Ok(())
    }

    fn seed_v12_creation(conn: &Connection) -> Result<(), CliError> {
        use sha2::{Digest, Sha256};
        let snapshot = serde_json::json!({
            "id": 1, "company_slug": "legacy", "description": "Old sale",
            "currency": "USD", "date": "2026-01-01", "posted_at": "2026-01-01 00:00:00",
            "reference": "old", "posted_by": "v12", "entries": [
                {"id":1, "account_code":"1000", "company_slug":"legacy", "direction":"debit", "amount":100, "memo":null, "tax_category":null},
                {"id":2, "account_code":"4000", "company_slug":"legacy", "direction":"credit", "amount":100, "memo":null, "tax_category":null}
            ]
        }).to_string();
        conn.execute_batch("INSERT INTO companies (slug, name) VALUES ('legacy','Legacy');
            INSERT INTO accounts (company_slug, code, name, type) VALUES
                ('legacy','1000','Cash','asset'), ('legacy','4000','Revenue','revenue');
            INSERT INTO transactions (company_slug, description, currency, date, posted_at, reference, posted_by)
                VALUES ('legacy','Old sale','USD','2026-01-01','2026-01-01 00:00:00','old','v12');
            INSERT INTO entries (transaction_id, account_code, company_slug, direction, amount)
                VALUES (1,'1000','legacy','debit',100), (1,'4000','legacy','credit',100);")?;
        let canonical = serde_json::json!([
            "beankeeper.audit.v1",
            1,
            "2026-01-01 00:00:00",
            "v12",
            "legacy",
            "transaction",
            "1",
            "create",
            null,
            snapshot
        ])
        .to_string();
        let genesis = "0".repeat(64);
        let mut hash = Sha256::new();
        hash.update(genesis.as_bytes());
        hash.update(canonical.as_bytes());
        conn.execute("INSERT INTO audit_log (id, changed_at, actor, company_slug, entity, entity_id, action, after, prev_hash, row_hash)
            VALUES (1,'2026-01-01 00:00:00','v12','legacy','transaction','1','create',?1,?2,?3)",
            params![snapshot, genesis, format!("{:x}", hash.finalize())])?;
        Ok(())
    }

    #[test]
    fn v13_preserves_v12_creation_snapshots_and_extends_the_same_chain() -> Result<(), CliError> {
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        let old_hash: String =
            conn.query_row("SELECT row_hash FROM audit_log", [], |r| r.get(0))?;
        apply_v13(&conn)?;
        let preserved: (String, Option<String>, Option<String>, i64) = conn.query_row(
            "SELECT row_hash, agent_uri, token_jti, hash_version FROM audit_log",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert_eq!(preserved, (old_hash.clone(), None, None, 1));
        super::super::audit::record_identity(
            &conn,
            &super::super::Actor::new("new".parse()?),
            crate::db::audit::IdentityEvent::TokenRevoked,
            "test",
            &serde_json::json!({}),
        )?;
        let tail: (String, i64) = conn.query_row(
            "SELECT prev_hash, hash_version FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(tail, (old_hash, 2));
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        super::super::audit::verify_creations(ReadConn::physical(&conn))?;
        ensure_schema(&conn)?;
        Ok(())
    }

    #[test]
    fn v13_rejects_corrupt_history_and_rolls_back_all_schema_changes() -> Result<(), CliError> {
        for corruption in [
            "DROP TRIGGER protect_audit_update; UPDATE audit_log SET row_hash = printf('%064d', 1)",
            "DROP TRIGGER protect_audit_delete; DELETE FROM audit_log",
        ] {
            let conn = v12_connection()?;
            seed_v12_creation(&conn)?;
            conn.execute_batch(corruption)?;
            assert!(apply_v13(&conn).is_err());
            assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 12);
            assert!(conn.prepare("SELECT agent_uri FROM transactions").is_err());
            assert!(conn.prepare("SELECT * FROM identity_root").is_err());
        }
        Ok(())
    }

    #[test]
    fn v12_preserves_history_and_backfills_a_stable_chain() -> Result<(), CliError> {
        let conn = open_conn();
        for migration in [
            apply_v1, apply_v2, apply_v3, apply_v4, apply_v5, apply_v6, apply_v7, apply_v8,
            apply_v9, apply_v10, apply_v11,
        ] {
            migration(&conn)?;
        }
        conn.execute_batch(
            "INSERT INTO companies (slug, name) VALUES ('legacy', 'Legacy');
             INSERT INTO transactions (company_slug, description, date)
                 VALUES ('legacy', 'Old posting', '2026-01-01');
             INSERT INTO audit_log (id, changed_at, actor, company_slug, entity, entity_id, action, before, after)
                 VALUES (7, '2026-01-02 03:04:05', 'old-actor', NULL, 'company', 'legacy', 'company_delete', NULL, ''),
                        (42, '2026-02-03 04:05:06', 'José', 'legacy', 'budget', '3', 'budget_set', '{}', '{\"amount\":20}');"
        )?;
        let query = "SELECT json_group_array(json_array(id, changed_at, actor, company_slug,
            entity, entity_id, action, before, after)) FROM (SELECT * FROM audit_log ORDER BY id)";
        let before: String = conn.query_row(query, [], |r| r.get(0))?;
        apply_v12(&conn)?;
        let after: String = conn.query_row(query, [], |r| r.get(0))?;
        assert_eq!(before, after);
        super::super::audit::verify_hash_chain(ReadConn::physical(&conn))?;
        let first_hash: String =
            conn.query_row("SELECT row_hash FROM audit_log WHERE id = 7", [], |r| {
                r.get(0)
            })?;
        // Independent SHA-256 vector for the documented compact JSON tuple.
        assert_eq!(
            first_hash,
            "ac0f25e546c34445cb6be81db3e9e8d9778b74a9646e848a73ca58fd178a7f75"
        );
        let hash: String =
            conn.query_row("SELECT row_hash FROM audit_log WHERE id = 42", [], |r| {
                r.get(0)
            })?;
        ensure_schema(&conn)?;
        let reopened_hash: String =
            conn.query_row("SELECT row_hash FROM audit_log WHERE id = 42", [], |r| {
                r.get(0)
            })?;
        assert_eq!(hash, reopened_hash);
        let poster: Option<String> =
            conn.query_row("SELECT posted_by FROM transactions", [], |r| r.get(0))?;
        assert_eq!(poster, None);
        assert_eq!(stored_schema(ReadConn::physical(&conn))?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn v12_migration_failure_rolls_back_schema_changes() -> Result<(), CliError> {
        let conn = open_conn();
        for migration in [
            apply_v1, apply_v2, apply_v3, apply_v4, apply_v5, apply_v6, apply_v7, apply_v8,
            apply_v9, apply_v10, apply_v11,
        ] {
            migration(&conn)?;
        }
        conn.execute_batch("CREATE TABLE audit_log_v12 (collision TEXT);")?;
        assert!(apply_v12(&conn).is_err());
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 11);
        assert!(conn.prepare("SELECT posted_by FROM transactions").is_err());
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))?;
        assert_eq!(count, 0);
        Ok(())
    }

    /// `(entry_id, company_slug, amount, cleared_date, statement_ref, status,
    /// reconciled_date)`.
    type ClearingRow = (
        i64,
        String,
        i64,
        String,
        Option<String>,
        String,
        Option<String>,
    );

    #[test]
    fn v11_migration_preserves_audit_rows_and_extends_action_constraint() -> Result<(), CliError> {
        let conn = open_conn();
        // The old audit table can be built independently of the ledger tables.
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL);")?;
        apply_v8(&conn)?;
        conn.execute_batch(
            "INSERT INTO audit_log (id, changed_at, actor, company_slug, entity, entity_id, action, before, after)
             VALUES (42, '2026-01-01 12:34:56', 'old-actor', 'acme', 'transaction', '17', 'correlate', '{\"before\":true}', '{\"after\":true}');"
        )?;
        let query = "SELECT json_object('id', id, 'changed_at', changed_at, 'actor', actor,
            'company_slug', company_slug, 'entity', entity, 'entity_id', entity_id,
            'action', action, 'before', before, 'after', after) FROM audit_log";
        let before: String = conn.query_row(query, [], |r| r.get(0))?;
        apply_v11(&conn)?;
        let after: String = conn.query_row(query, [], |r| r.get(0))?;
        assert_eq!(before, after);
        conn.execute("INSERT INTO audit_log (actor, entity, entity_id, action) VALUES ('new', 'transaction', '17', 'reverse')", [])?;
        assert!(conn.last_insert_rowid() > 42);
        assert!(conn.execute("INSERT INTO audit_log (actor, entity, entity_id, action) VALUES ('bad', 'transaction', '17', 'invalid')", []).is_err());
        let indices: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'audit_log'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(indices, 3);
        assert_eq!(get_schema_version(ReadConn::physical(&conn))?, 11);
        Ok(())
    }

    #[test]
    fn v10_migration_backfills_clearings_from_v8() {
        let conn = open_conn();
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        apply_v4(&conn).unwrap_or_else(|e| panic!("v4 failed: {e}"));
        apply_v5(&conn).unwrap_or_else(|e| panic!("v5 failed: {e}"));
        apply_v6(&conn).unwrap_or_else(|e| panic!("v6 failed: {e}"));
        apply_v7(&conn).unwrap_or_else(|e| panic!("v7 failed: {e}"));
        apply_v8(&conn).unwrap_or_else(|e| panic!("v8 failed: {e}"));
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(8));

        // A v8-shaped ledger: one transaction with a cleared, a reconciled,
        // and an uncleared entry.
        conn.execute_batch(
            "
            INSERT INTO companies (slug, name) VALUES ('co', 'Co');
            INSERT INTO accounts (company_slug, code, name, type) VALUES
                ('co', '1000', 'Cash', 'asset'),
                ('co', '1200', 'Receivable', 'asset'),
                ('co', '4000', 'Revenue', 'revenue');
            INSERT INTO transactions (id, company_slug, description, date)
                VALUES (1, 'co', 'Sale', '2026-03-15');
            INSERT INTO entries (id, transaction_id, account_code, company_slug, direction, amount, status) VALUES
                (1, 1, '1000', 'co', 'debit', 500, 'cleared'),
                (2, 1, '1200', 'co', 'debit', 300, 'reconciled'),
                (3, 1, '4000', 'co', 'credit', 800, 'uncleared');
            ",
        )
        .unwrap_or_else(|e| panic!("seed v8 ledger: {e}"));

        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        let mut stmt = conn
            .prepare(
                "SELECT entry_id, company_slug, amount, cleared_date, statement_ref, status, \
                        reconciled_date \
                 FROM entry_clearings ORDER BY entry_id",
            )
            .unwrap_or_else(|e| panic!("prepare: {e}"));
        let rows: Vec<ClearingRow> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap_or_else(|e| panic!("query: {e}"))
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    1,
                    "co".into(),
                    500,
                    "2026-03-15".into(),
                    None,
                    "cleared".into(),
                    None
                ),
                (
                    2,
                    "co".into(),
                    300,
                    "2026-03-15".into(),
                    None,
                    "reconciled".into(),
                    Some("2026-03-15".into())
                ),
            ]
        );

        // Re-running ensure_schema neither re-applies nor duplicates.
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema rerun: {e}"));
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM entry_clearings", [], |r| r.get(0))
            .unwrap_or(-1);
        assert_eq!(n, 2);
    }

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory()
            .unwrap_or_else(|e| panic!("cannot open in-memory db: {e}"));
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn
    }

    #[test]
    fn fresh_db_has_version_zero() {
        let conn = open_conn();
        let v = get_schema_version(ReadConn::physical(&conn));
        assert_eq!(v.ok(), Some(0));
    }

    #[test]
    fn ensure_schema_creates_tables() {
        let conn = open_conn();
        let result = ensure_schema(&conn);
        assert!(result.is_ok());
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        assert!(ensure_schema(&conn).is_ok());
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );
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
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(2));

        // Now run ensure_schema which should apply v3
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        // Verify the reference column exists by querying pragma
        let has_reference: bool =
            conn.prepare("PRAGMA table_info(transactions)")
                .is_ok_and(|mut stmt| {
                    let names: Vec<String> = stmt
                        .query_map([], |row| row.get::<_, String>(1))
                        .unwrap_or_else(|e| panic!("query failed: {e}"))
                        .filter_map(Result::ok)
                        .collect();
                    names.contains(&"reference".to_string())
                });
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
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(3));

        // Now run ensure_schema which should apply v4
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        // Verify tax_category column on entries
        let has_tax_category: bool =
            conn.prepare("PRAGMA table_info(entries)")
                .is_ok_and(|mut stmt| {
                    let names: Vec<String> = stmt
                        .query_map([], |row| row.get::<_, String>(1))
                        .unwrap_or_else(|e| panic!("query failed: {e}"))
                        .filter_map(Result::ok)
                        .collect();
                    names.contains(&"tax_category".to_string())
                });
        assert!(
            has_tax_category,
            "tax_category column should exist on entries after v4"
        );

        // Verify default_tax_category column on accounts
        let has_default_tax: bool =
            conn.prepare("PRAGMA table_info(accounts)")
                .is_ok_and(|mut stmt| {
                    let names: Vec<String> = stmt
                        .query_map([], |row| row.get::<_, String>(1))
                        .unwrap_or_else(|e| panic!("query failed: {e}"))
                        .filter_map(Result::ok)
                        .collect();
                    names.contains(&"default_tax_category".to_string())
                });
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
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(1));

        // Now run ensure_schema which should apply v2
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

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

    #[test]
    fn v7_migration_adds_budgets_table() {
        let conn = open_conn();
        // Apply v1 through v6
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        apply_v4(&conn).unwrap_or_else(|e| panic!("v4 failed: {e}"));
        apply_v5(&conn).unwrap_or_else(|e| panic!("v5 failed: {e}"));
        apply_v6(&conn).unwrap_or_else(|e| panic!("v6 failed: {e}"));
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(6));

        // Now run ensure_schema which should apply v7
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        // Verify budgets table exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'budgets'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn budgets_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'budgets'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn v8_migration_adds_audit_log_table() {
        let conn = open_conn();
        // Apply v1 through v7.
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        apply_v4(&conn).unwrap_or_else(|e| panic!("v4 failed: {e}"));
        apply_v5(&conn).unwrap_or_else(|e| panic!("v5 failed: {e}"));
        apply_v6(&conn).unwrap_or_else(|e| panic!("v6 failed: {e}"));
        apply_v7(&conn).unwrap_or_else(|e| panic!("v7 failed: {e}"));
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(7));

        // Now run ensure_schema which should apply v8.
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        // Verify audit_log table exists.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'audit_log'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn v9_migration_normalizes_metadata() {
        let conn = open_conn();
        // Build a v8-shaped database.
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        apply_v4(&conn).unwrap_or_else(|e| panic!("v4 failed: {e}"));
        apply_v5(&conn).unwrap_or_else(|e| panic!("v5 failed: {e}"));
        apply_v6(&conn).unwrap_or_else(|e| panic!("v6 failed: {e}"));
        apply_v7(&conn).unwrap_or_else(|e| panic!("v7 failed: {e}"));
        apply_v8(&conn).unwrap_or_else(|e| panic!("v8 failed: {e}"));
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(8));

        conn.execute(
            "INSERT INTO companies (slug, name) VALUES ('acme', 'Acme')",
            [],
        )
        .unwrap_or_else(|e| panic!("insert company: {e}"));
        let samples: [Option<&str>; 7] = [
            Some("open:1000"),
            Some(r#"{"ref":"INV-1"}"#),
            Some("[1,2]"),
            Some("42"),
            Some(""),
            None,
            Some(r#"{"correlate":2}"#),
        ];
        for m in samples {
            conn.execute(
                "INSERT INTO transactions (company_slug, description, metadata, date) \
                 VALUES ('acme', 'x', ?1, '2026-01-01')",
                params![m],
            )
            .unwrap_or_else(|e| panic!("insert txn: {e}"));
        }

        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        let mut stmt = conn
            .prepare("SELECT metadata FROM transactions ORDER BY id")
            .unwrap_or_else(|e| panic!("prepare: {e}"));
        let got: Vec<Option<String>> = stmt
            .query_map([], |r| r.get(0))
            .unwrap_or_else(|e| panic!("query: {e}"))
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            got,
            vec![
                Some(r#"{"memo":"open:1000"}"#.to_string()),
                Some(r#"{"ref":"INV-1"}"#.to_string()),
                Some(r#"{"memo":[1,2]}"#.to_string()),
                Some(r#"{"memo":"42"}"#.to_string()),
                None,
                None,
                Some(r#"{"correlate":2}"#.to_string()),
            ]
        );

        // The migration is unaudited by design.
        let audited: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .unwrap_or(-1);
        assert_eq!(audited, 0);
    }

    #[test]
    fn audit_log_table_exists() {
        let conn = open_conn();
        assert!(ensure_schema(&conn).is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'audit_log'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn v6_migration_adds_status_column() {
        let conn = open_conn();
        // Apply v1 through v5
        apply_v1(&conn).unwrap_or_else(|e| panic!("v1 failed: {e}"));
        apply_v2(&conn).unwrap_or_else(|e| panic!("v2 failed: {e}"));
        apply_v3(&conn).unwrap_or_else(|e| panic!("v3 failed: {e}"));
        apply_v4(&conn).unwrap_or_else(|e| panic!("v4 failed: {e}"));
        apply_v5(&conn).unwrap_or_else(|e| panic!("v5 failed: {e}"));
        assert_eq!(get_schema_version(ReadConn::physical(&conn)).ok(), Some(5));

        // Now run ensure_schema which should apply v6
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(
            stored_schema(ReadConn::physical(&conn)).ok(),
            Some(CURRENT_VERSION)
        );

        // Verify status column on entries
        let has_status: bool = conn
            .prepare("PRAGMA table_info(entries)")
            .is_ok_and(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap_or_else(|e| panic!("query failed: {e}"))
                    .filter_map(Result::ok)
                    .collect();
                names.contains(&"status".to_string())
            });
        assert!(has_status, "status column should exist on entries after v6");
    }
}
