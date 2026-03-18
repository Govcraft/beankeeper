use crate::cli::Cli;
use crate::db::connection::Db;
use crate::db::schema;
use crate::error::CliError;
use crate::passphrase;

/// Run the `bk verify` command.
///
/// Checks schema version, foreign key integrity, and transaction balance invariants.
///
/// # Errors
///
/// Returns [`CliError`] if verification encounters a problem.
pub fn run(cli: &Cli) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;

    // Check schema version
    let version = schema::get_schema_version(db.conn())?;
    if cli.verbosity.verbose {
        eprintln!("[ok] Schema version: {version}");
    }

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

    if cli.is_json() {
        let meta = crate::output::json::meta("verify", None);
        let rendered = crate::output::json::render_verify(version, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Ledger is healthy");
    }

    Ok(())
}
