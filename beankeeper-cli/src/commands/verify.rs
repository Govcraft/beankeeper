use crate::cli::Cli;
use crate::db::connection::Db;
use crate::db::schema;
use crate::error::CliError;

/// Run the `bk verify` command.
///
/// Checks schema version, foreign key integrity, and transaction balance invariants.
///
/// # Errors
///
/// Returns [`CliError`] if verification encounters a problem.
pub fn run(cli: &Cli) -> Result<(), CliError> {
    let db = cli.open_db()?;

    let version = verify(&db)?;
    if cli.verbosity.verbose {
        eprintln!("[ok] Schema version: {version}");
    }

    if cli.is_json() {
        let meta = cli.meta("verify", None);
        let rendered = crate::output::json::render_verify(version, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Ledger is healthy");
    }

    Ok(())
}

pub(crate) fn verify(db: &Db) -> Result<i64, CliError> {
    // Check schema version
    let version = schema::get_schema_version(db.conn())?;

    crate::db::audit::atomically(db.conn(), || {
        crate::db::audit::verify_hash_chain(db.conn())?;
        crate::db::policy::get(db.conn())?;
        crate::db::periods::verify(db.conn())?;
        crate::db::proposals::verify_resolutions(db.conn())?;
        crate::db::spend_limits::verify(db.conn())?;
        crate::db::verification::verify_accounting(db.conn())?;
        crate::db::audit::verify_creations(db.conn())?;

        // Foreign key integrity check
        let fk_errors: Vec<String> = {
            let mut stmt = db.conn().prepare("PRAGMA foreign_key_check")?;
            let rows = stmt.query_map([], |row| {
                let table: String = row.get(0)?;
                let rowid: i64 = row.get(1)?;
                Ok(format!("{table} row {rowid}"))
            })?;
            let mut errs = Vec::new();
            for row in rows {
                errs.push(row?);
            }
            errs
        };

        if !fk_errors.is_empty() {
            return Err(CliError::Database(format!(
                "foreign key violations: {}",
                fk_errors.join(", ")
            )));
        }

        // SQLite integrity check
        let integrity: String = db
            .conn()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;

        if integrity != "ok" {
            return Err(CliError::Database(format!(
                "integrity check failed: {integrity}"
            )));
        }

        Ok(())
    })?;

    Ok(version)
}

/// Verify an open ledger without writing to stdout.
///
/// # Errors
///
/// Returns database errors for invalid ledgers.
pub fn execute_json(db: &Db) -> Result<serde_json::Value, CliError> {
    let version = verify(db)?;
    super::company::json_data(&crate::output::json::render_verify(
        version,
        crate::output::json::meta("verify", None),
    )?)
}
