use std::io::IsTerminal;

use crate::cli::{AccountCommand, Cli, OutputFormat, resolve_format};
use crate::db::connection::Db;
use crate::db::{self, accounts};
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
    let db = Db::open_existing(&cli.db, pp.as_ref())?;
    crate::db::companies::get_company(db.conn(), company)?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    if let AccountCommand::Delete { code, force } = sub {
        return run_delete(cli, &db, company, code, *force, format);
    }
    let result = compute(&db, company, sub)?;
    match &result {
        AccountData::Rows(rows) => {
            render_accounts(rows, operation(sub), company, format, use_color)?;
        }
        AccountData::Balances(rows) => {
            render_accounts_with_balances(rows, company, format, use_color)?;
        }
    }
    if !cli.verbosity.quiet {
        match sub {
            AccountCommand::Create { code, name, .. } => {
                eprintln!("[ok] Created account: {code} ({name})");
            }
            AccountCommand::List { .. } => {
                let count = match &result {
                    AccountData::Rows(rows) => rows.len(),
                    AccountData::Balances(rows) => rows.len(),
                };
                eprintln!(
                    "{count} {noun}",
                    noun = if count == 1 { "account" } else { "accounts" }
                );
            }
            _ => {}
        }
    }
    Ok(())
}

enum AccountData {
    Rows(Vec<db::AccountRow>),
    Balances(Vec<db::AccountWithBalanceRow>),
}

fn operation(sub: &AccountCommand) -> &'static str {
    match sub {
        AccountCommand::Create { .. } => "account.create",
        AccountCommand::Show { .. } => "account.show",
        _ => "account.list",
    }
}

fn compute(db: &Db, company: &str, sub: &AccountCommand) -> Result<AccountData, CliError> {
    db::companies::get_company(db.conn(), company)?;
    match sub {
        AccountCommand::Create {
            code,
            name,
            account_type,
            default_tax_category,
        } => {
            let account_type = format!("{account_type:?}").to_lowercase();
            let row = accounts::create_account(
                db.conn(),
                company,
                code,
                name,
                &account_type,
                default_tax_category.as_deref(),
            )?;
            Ok(AccountData::Rows(vec![row]))
        }
        AccountCommand::List {
            account_type,
            name,
            with_balances,
            from,
            to,
        } => {
            let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
            if *with_balances {
                Ok(AccountData::Balances(db::list_accounts_with_balances(
                    db.conn(),
                    company,
                    type_filter.as_deref(),
                    name.as_deref(),
                    from.as_deref(),
                    to.as_deref(),
                )?))
            } else {
                Ok(AccountData::Rows(accounts::list_accounts(
                    db.conn(),
                    &accounts::ListAccountParams {
                        company_slug: company,
                        type_filter: type_filter.as_deref(),
                        name_filter: name.as_deref(),
                    },
                )?))
            }
        }
        AccountCommand::Show { code } => Ok(AccountData::Rows(vec![accounts::get_account(
            db.conn(),
            company,
            code,
        )?])),
        AccountCommand::Delete { .. } => Err(CliError::Usage(
            "account deletion is unavailable through MCP".into(),
        )),
    }
}

/// Execute an account operation without writing to stdout.
///
/// # Errors
///
/// Returns the same validation and database errors as the CLI.
pub fn execute_json(
    db: &Db,
    company: &str,
    sub: &AccountCommand,
) -> Result<serde_json::Value, CliError> {
    let meta = output::json::meta(operation(sub), Some(company));
    let rendered = match compute(db, company, sub)? {
        AccountData::Rows(rows) => output::json::render_accounts(&rows, meta)?,
        AccountData::Balances(rows) => output::json::render_accounts_with_balances(&rows, meta)?,
    };
    super::company::json_data(&rendered)
}

/// Execute the `account delete` subcommand.
fn run_delete(
    cli: &Cli,
    db: &Db,
    company: &str,
    code: &str,
    force: bool,
    format: OutputFormat,
) -> Result<(), CliError> {
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
    let actor = crate::db::Actor::resolve(cli.actor.as_deref());
    crate::db::delete_account(db.conn(), &actor, company, code)?;
    if format == OutputFormat::Json {
        let meta = output::json::meta("account.delete", Some(company));
        let rendered = output::json::render_deleted(code, meta)?;
        println!("{rendered}");
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] Deleted account: {code}");
    }
    Ok(())
}

/// Render account rows in the requested format.
fn render_accounts(
    rows: &[crate::db::AccountRow],
    command: &str,
    company: &str,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_accounts(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta(command, Some(company));
            let rendered = output::json::render_accounts(rows, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_accounts(rows)?;
            print!("{rendered}");
        }
    }
    Ok(())
}

/// Render account-with-balance rows in the requested format.
fn render_accounts_with_balances(
    rows: &[db::AccountWithBalanceRow],
    company: &str,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_accounts_with_balances(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("account.list", Some(company));
            let rendered = output::json::render_accounts_with_balances(rows, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_accounts_with_balances(rows)?;
            print!("{rendered}");
        }
    }
    Ok(())
}
