use crate::cli::Cli;
use crate::db::verification::VerifiedLedger;
use crate::error::CliError;
use crate::operation::Operation;

pub(crate) fn render(report: &VerifiedLedger, cli: &Cli) -> Result<(), CliError> {
    if cli.verbosity.verbose && (cli.is_json() || cli.verbosity.quiet) {
        eprintln!("[ok] Schema version: {}", report.schema_version());
    }

    if cli.is_json() {
        let meta = cli.meta(Operation::Verify, None);
        let rendered = crate::output::json::render_verify(report, meta)?;
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
