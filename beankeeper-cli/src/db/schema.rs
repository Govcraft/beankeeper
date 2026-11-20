use rusqlite::{Connection, params};

use super::metadata;
use crate::error::CliError;

/// Current schema version. Bump this when adding migrations.
const CURRENT_VERSION: i64 = 16;

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

    if version < 6 {
        apply_v6(conn)?;
    }

    if version < 7 {
        apply_v7(conn)?;
    }

    if version < 8 {
        apply_v8(conn)?;
    }

    if version < 9 {
        apply_v9(conn)?;
    }

    if version < 10 {
        apply_v10(conn)?;
    }

    if version < 11 {
        apply_v11(conn)?;
    }

    if version < 12 {
        apply_v12(conn)?;
    }

    if version < 13 {
        apply_v13(conn)?;
    }

    if version < 14 {
        apply_v14(conn)?;
    }

    if version < 15 {
        apply_v15(conn)?;
    }

    if version < 16 {
        apply_v16(conn)?;
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
                params![metadata::normalize(&raw), id],
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
        super::audit::verify_hash_chain(conn)?;
        Ok(())
    })
}

/// Add bank categorization rules without changing any historical audit hash.
fn apply_v14(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(conn)?;
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
        super::audit::verify_hash_chain(conn)?;
        Ok(())
    })
}

/// Allow audited annotation actions while preserving historical hashes and triggers.
fn apply_v15(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(conn)?;
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
        super::audit::verify_hash_chain(conn)?;
        Ok(())
    })
}

/// Store pending schedules and extend auditing without changing historical hashes.
fn apply_v16(conn: &Connection) -> Result<(), CliError> {
    super::audit::atomically(conn, || {
        super::audit::verify_hash_chain(conn)?;
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
        super::audit::verify_hash_chain(conn)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(get_schema_version(&conn)?, 15);
        conn.execute_batch("DROP TABLE schedules")?;
        apply_v16(&conn)?;
        let retained: String = conn.query_row(
            "SELECT row_hash FROM audit_log ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(historical, retained);
        super::super::audit::verify_hash_chain(&conn)?;
        super::super::audit::verify_creations(&conn)?;
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        assert!(
            conn.execute("UPDATE audit_log SET actor = 'tampered'", [])
                .is_err()
        );
        ensure_schema(&conn)?;
        assert_eq!(get_schema_version(&conn)?, CURRENT_VERSION);
        Ok(())
    }

    #[test]
    fn v15_preserves_history_and_metadata_edits_extend_the_chain() -> Result<(), CliError> {
        use super::super::{Actor, audit, metadata::MetadataEdit};
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
            assert!(
                audit::edit_metadata(
                    &conn,
                    &Actor::new("v15"),
                    "legacy",
                    audit::MetadataTarget::Id(id),
                    &edit
                )?
                .changed
            );
        }
        audit::verify_hash_chain(&conn)?;
        audit::verify_creations(&conn)?;
        assert!(conn.execute("DELETE FROM audit_log", []).is_err());
        assert!(
            conn.execute("UPDATE audit_log SET actor = 'tampered'", [])
                .is_err()
        );
        ensure_schema(&conn)?;
        assert_eq!(get_schema_version(&conn)?, CURRENT_VERSION);
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
            assert_eq!(get_schema_version(&conn)?, 14);
        }
        Ok(())
    }

    #[test]
    fn v14_preserves_history_and_extends_the_chain_with_rules() -> Result<(), CliError> {
        use super::super::{Actor, audit, rules};
        let conn = v12_connection()?;
        seed_v12_creation(&conn)?;
        apply_v13(&conn)?;
        audit::record_identity(
            &conn,
            &Actor::new("v13"),
            "token_revoke",
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
        assert_eq!(get_schema_version(&conn)?, CURRENT_VERSION);
        assert_eq!(hashes(&conn)?, before);
        let rule = rules::create_rule(
            &conn,
            &Actor::new("v14"),
            "legacy",
            "sale".parse()?,
            "4000",
            0,
        )?;
        audit::delete_rule(&conn, &Actor::new("v14"), "legacy", &rule.id)?;
        assert_eq!(&hashes(&conn)?[..2], before.as_slice());
        audit::verify_hash_chain(&conn)?;
        audit::verify_creations(&conn)?;
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
            assert_eq!(get_schema_version(&conn)?, 13);
            assert!(conn.prepare("SELECT * FROM categorization_rules").is_err());
        }
        Ok(())
    }

    fn v12_connection() -> Result<Connection, CliError> {
        let conn = open_conn();
        for migration in [
            apply_v1, apply_v2, apply_v3, apply_v4, apply_v5, apply_v6, apply_v7, apply_v8,
            apply_v9, apply_v10, apply_v11, apply_v12,
        ] {
            migration(&conn)?;
        }
        Ok(conn)
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
            &super::super::Actor::new("new"),
            "token_revoke",
            "test",
            &serde_json::json!({}),
        )?;
        let tail: (String, i64) = conn.query_row(
            "SELECT prev_hash, hash_version FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(tail, (old_hash, 2));
        super::super::audit::verify_hash_chain(&conn)?;
        super::super::audit::verify_creations(&conn)?;
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
            assert_eq!(get_schema_version(&conn)?, 12);
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
        super::super::audit::verify_hash_chain(&conn)?;
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
        assert_eq!(get_schema_version(&conn)?, CURRENT_VERSION);
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
        assert_eq!(get_schema_version(&conn)?, 11);
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
        assert_eq!(get_schema_version(&conn)?, 11);
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
        assert_eq!(get_schema_version(&conn).ok(), Some(8));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(3));

        // Now run ensure_schema which should apply v4
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(6));

        // Now run ensure_schema which should apply v7
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(7));

        // Now run ensure_schema which should apply v8.
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(8));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
        assert_eq!(get_schema_version(&conn).ok(), Some(5));

        // Now run ensure_schema which should apply v6
        ensure_schema(&conn).unwrap_or_else(|e| panic!("ensure_schema failed: {e}"));
        assert_eq!(get_schema_version(&conn).ok(), Some(CURRENT_VERSION));

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
