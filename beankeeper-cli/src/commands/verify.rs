use crate::cli::Cli;
use crate::db::{Db, verification::VerifiedLedger};
use crate::error::CliError;
use crate::operation::Operation;

/// Run the `bk verify` command.
///
/// # Errors
/// Returns a database error for the first failed check, or an output error.
pub fn run(cli: &Cli) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let report = VerifiedLedger::read(&db)?;
    if cli.verbosity.verbose && (cli.is_json() || cli.verbosity.quiet) {
        eprintln!("[ok] Schema version: {}", report.schema_version());
    }

    if cli.is_json() {
        let meta = cli.meta(Operation::Verify, None);
        let rendered = crate::output::json::render_verify(&report, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Ledger is healthy");
        if !cli.is_json() {
            report.write_summary(&mut std::io::stderr().lock())?;
        }
    }

    Ok(())
}

/// Verify an open ledger and serialize its evidence at the MCP output boundary.
///
/// # Errors
/// Returns a database error for the first failed check, or a serialization error.
pub fn execute_json(db: &Db) -> Result<serde_json::Value, CliError> {
    serde_json::to_value(VerifiedLedger::read(db)?)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}
