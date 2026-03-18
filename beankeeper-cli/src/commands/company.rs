use std::io::IsTerminal;

use crate::cli::{Cli, CompanyCommand, OutputFormat, resolve_format};
use crate::db::companies;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run a `bk company` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, sub: &CompanyCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        CompanyCommand::Create {
            slug,
            name,
            description,
        } => {
            let row = companies::create_company(db.conn(), slug, name, description.as_deref())?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Created company: {} ({})", row.slug, row.name);
            }
            render_companies(&[row], format, use_color)?;
        }
        CompanyCommand::List => {
            let rows = companies::list_companies(db.conn())?;
            render_companies(&rows, format, use_color)?;
            if !cli.verbosity.quiet {
                let count = rows.len();
                eprintln!(
                    "{count} {noun}",
                    noun = if count == 1 { "company" } else { "companies" }
                );
            }
        }
        CompanyCommand::Show { slug } => {
            let row = companies::get_company(db.conn(), slug)?;
            render_companies(&[row], format, use_color)?;
        }
        CompanyCommand::Delete { slug, force } => {
            if !force {
                if !std::io::stdin().is_terminal() {
                    return Err(CliError::Usage(
                        "use --force to confirm deletion when stdin is not a terminal".into(),
                    ));
                }
                eprint!("Delete company '{slug}'? [y/N] ");
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    eprintln!("Aborted.");
                    return Ok(());
                }
            }
            companies::delete_company(db.conn(), slug)?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Deleted company: {slug}");
            }
        }
    }

    Ok(())
}

/// Render company rows in the requested format.
fn render_companies(
    rows: &[crate::db::CompanyRow],
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_companies(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let rendered = output::json::render_companies(rows)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_companies(rows)?;
            print!("{rendered}");
        }
    }
    Ok(())
}
