use std::io::IsTerminal;

use crate::cli::{AccountCommand, Cli, OutputFormat, resolve_format};
use crate::db::accounts;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run a `bk account` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &AccountCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        AccountCommand::Create {
            code,
            name,
            account_type,
        } => {
            let type_str = format!("{account_type:?}").to_lowercase();
            let row = accounts::create_account(db.conn(), company, code, name, &type_str)?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Created account: {} ({})", row.code, row.name);
            }
            render_accounts(&[row], format, use_color)?;
        }
        AccountCommand::List { account_type } => {
            let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
            let rows = accounts::list_accounts(db.conn(), company, type_filter.as_deref())?;
            render_accounts(&rows, format, use_color)?;
            if !cli.verbosity.quiet {
                let count = rows.len();
                eprintln!(
                    "{count} {noun}",
                    noun = if count == 1 { "account" } else { "accounts" }
                );
            }
        }
        AccountCommand::Show { code } => {
            let row = accounts::get_account(db.conn(), company, code)?;
            render_accounts(&[row], format, use_color)?;
        }
        AccountCommand::Delete { code, force } => {
            if !force {
                if !std::io::stdin().is_terminal() {
                    return Err(CliError::Usage(
                        "use --force to confirm deletion when stdin is not a terminal".into(),
                    ));
                }
                eprint!("Delete account '{code}'? [y/N] ");
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    eprintln!("Aborted.");
                    return Ok(());
                }
            }
            accounts::delete_account(db.conn(), company, code)?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Deleted account: {code}");
            }
        }
    }

    Ok(())
}

/// Render account rows in the requested format.
fn render_accounts(
    rows: &[crate::db::AccountRow],
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_accounts(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let rendered = output::json::render_accounts(rows)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_accounts(rows)?;
            print!("{rendered}");
        }
    }
    Ok(())
}
