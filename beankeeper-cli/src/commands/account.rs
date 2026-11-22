use crate::cli::{AccountCommand, Cli, CurrencyArg, OutputFormat, resolve_format};
use crate::confirmation::confirm_deletion;
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::Db;
use crate::db::{self, AccountBalanceWindow, CreateAccountParams, accounts};
use crate::error::CliError;
use crate::identity::{Admin, Granted};
use crate::operation::Operation;
use crate::output;

/// Run a `bk account` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &CompanySlug, sub: &AccountCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    crate::db::companies::get_company(db.read(), company)?;
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

fn operation(sub: &AccountCommand) -> Operation {
    match sub {
        AccountCommand::Create { .. } => Operation::AccountCreate,
        AccountCommand::Show { .. } => Operation::AccountShow,
        _ => Operation::AccountList,
    }
}

fn compute(
    db: &Db,
    company: &CompanySlug,
    sub: &AccountCommand,
    actor: &db::Actor,
) -> Result<AccountData, CliError> {
    let scope = Company::bind(db.read(), company.clone())?;
    match sub {
        AccountCommand::Create {
            code,
            name,
            account_type,
            default_tax_category,
        } => {
            let row = Granted::<Admin>::with_db(db, actor, |grant| {
                accounts::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: company,
                        code,
                        name,
                        account_type: *account_type,
                        default_tax_category: default_tax_category.as_deref(),
                    },
                )
            })?;
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
            let type_filter = *account_type;
            let filter = accounts::ListAccountParams {
                type_filter,
                name_filter: name.as_deref(),
            };
            if *with_balances {
                let window = AccountBalanceWindow::new(*from, *to)?;
                let currency = currency.as_ref().map(CurrencyArg::resolve).transpose()?;
                db.read_snapshot(|reader| {
                    let currency = match currency {
                        Some(currency) => currency,
                        None => super::report_currency::infer_company_currency(
                            reader,
                            company.as_str(),
                            CliError::Validation,
                        )?,
                    };
                    let rows = db::list_accounts_with_balances(&scope, &filter, window, currency)?;
                    Ok(AccountData::Balances(rows))
                })
            } else {
                Ok(AccountData::Rows(accounts::list_accounts(&scope, &filter)?))
            }
        }
        AccountCommand::Show { code } => Ok(AccountData::Rows(vec![accounts::get_account(
            &scope, code,
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
    company: &CompanySlug,
    sub: &AccountCommand,
) -> Result<serde_json::Value, CliError> {
    execute_json_with_actor(db, company, sub, &db::Actor::cli_fallback())
}

/// Execute an account operation without writing to stdout.
///
/// # Errors
///
/// Returns the same validation and database errors as the CLI.
pub fn execute_json_with_actor(
    db: &Db,
    company: &CompanySlug,
    sub: &AccountCommand,
    actor: &db::Actor,
) -> Result<serde_json::Value, CliError> {
    let meta = output::json::meta(operation(sub), Some(company.as_str()));
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
    company: &CompanySlug,
    code: &str,
    force: bool,
    format: OutputFormat,
) -> Result<(), CliError> {
    let Some(confirmed) = confirm_deletion(force, &format!("account '{code}'"))? else {
        eprintln!("Aborted.");
        return Ok(());
    };
    let actor = cli.principal();
    Granted::<Admin>::with_db(db, &actor, |grant| {
        crate::db::delete_account(grant, company, code, confirmed)
    })?;
    if format == OutputFormat::Json {
        let meta = cli.meta(Operation::AccountDelete, Some(company.as_str()));
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
    command: Operation,
    company: &CompanySlug,
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
            let meta = cli.meta(command, Some(company.as_str()));
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
    company: &CompanySlug,
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
            let meta = cli.meta(Operation::AccountList, Some(company.as_str()));
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
