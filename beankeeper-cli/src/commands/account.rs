use crate::cli::{AccountCommand, Cli, CurrencyArg, OutputFormat, resolve_format};
use crate::confirmation::confirm_deletion;
use crate::db::connection::Db;
use crate::db::{self, AccountBalanceWindow, CreateAccountParams, accounts};
use crate::error::CliError;
use crate::output;

/// Run a `bk account` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &AccountCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    crate::db::companies::get_company(db.conn(), company)?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    if let AccountCommand::Delete { code, force } = sub {
        return run_delete(cli, &db, company, code, *force, format);
    }
    let result = compute(&db, company, sub, &cli.principal())?;
    match &result {
        AccountData::Rows(rows) => {
            render_accounts(rows, operation(sub), company, format, use_color, cli)?;
        }
        AccountData::Balances(rows) => {
            render_accounts_with_balances(rows, company, format, use_color, cli)?;
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

fn compute(
    db: &Db,
    company: &str,
    sub: &AccountCommand,
    actor: &db::Actor,
) -> Result<AccountData, CliError> {
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
                actor,
                &CreateAccountParams {
                    company_slug: company,
                    code,
                    name,
                    account_type: &account_type,
                    default_tax_category: default_tax_category.as_deref(),
                },
            )?;
            Ok(AccountData::Rows(vec![row]))
        }
        AccountCommand::List {
            account_type,
            name,
            with_balances,
            from,
            to,
            currency,
        } => {
            let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
            let filter = accounts::ListAccountParams {
                company_slug: company,
                type_filter: type_filter.as_deref(),
                name_filter: name.as_deref(),
            };
            let conn = db.conn();
            if *with_balances {
                let window = AccountBalanceWindow::new(*from, *to)?;
                let currency = currency.as_ref().map(CurrencyArg::resolve).transpose()?;
                db::audit::atomically(conn, || {
                    let currency = match currency {
                        Some(currency) => currency,
                        None => super::report_currency::infer_company_currency(
                            conn,
                            company,
                            CliError::Validation,
                        )?,
                    };
                    let rows = db::list_accounts_with_balances(conn, &filter, window, currency)?;
                    Ok(AccountData::Balances(rows))
                })
            } else {
                Ok(AccountData::Rows(accounts::list_accounts(conn, &filter)?))
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

/// Execute an account operation with the default named principal.
///
/// # Errors
/// Returns validation or database errors.
pub fn execute_json(
    db: &Db,
    company: &str,
    sub: &AccountCommand,
) -> Result<serde_json::Value, CliError> {
    execute_json_with_actor(db, company, sub, &db::Actor::new("cli"))
}

/// Execute an account operation without writing to stdout.
///
/// # Errors
///
/// Returns the same validation and database errors as the CLI.
pub fn execute_json_with_actor(
    db: &Db,
    company: &str,
    sub: &AccountCommand,
    actor: &db::Actor,
) -> Result<serde_json::Value, CliError> {
    let meta = output::json::meta(operation(sub), Some(company));
    let rendered = match compute(db, company, sub, actor)? {
        AccountData::Rows(rows) => output::json::render_accounts(&rows, meta)?,
        AccountData::Balances(rows) => return output::json::account_balances_payload(&rows),
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
    let Some(confirmed) = confirm_deletion(force, &format!("account '{code}'"))? else {
        eprintln!("Aborted.");
        return Ok(());
    };
    let actor = cli.principal();
    crate::db::delete_account(db.conn(), &actor, company, code, confirmed)?;
    if format == OutputFormat::Json {
        let meta = cli.meta("account.delete", Some(company));
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
    cli: &Cli,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_accounts(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = cli.meta(command, Some(company));
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
    cli: &Cli,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_accounts_with_balances(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = cli.meta("account.list", Some(company));
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
