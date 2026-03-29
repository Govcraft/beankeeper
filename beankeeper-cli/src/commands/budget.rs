//! `bk budget` command handlers.

use std::io::IsTerminal;

use beankeeper::types::Currency;

use crate::cli::{BudgetCommand, Cli, OutputFormat, resolve_format};
use crate::commands::txn::parse_amount_to_minor;
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run a `bk budget` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &BudgetCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;
    let format = resolve_format(None, cli);

    match sub {
        BudgetCommand::Set {
            account,
            year,
            month,
            amount,
            annual,
            currency,
            notes,
        } => run_set(
            cli,
            &db,
            company,
            account,
            *year,
            month.as_ref().copied(),
            amount.as_deref(),
            annual.as_deref(),
            currency,
            notes.as_deref(),
            format,
        ),
        BudgetCommand::List {
            year,
            account,
            month,
        } => run_list(
            cli,
            &db,
            company,
            *year,
            account.as_deref(),
            *month,
            format,
        ),
        BudgetCommand::Delete {
            account,
            year,
            month,
            currency,
            force,
        } => run_delete(cli, &db, company, account, *year, *month, currency, *force),
    }
}

/// Execute the `budget set` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_set(
    cli: &Cli,
    db: &Db,
    company: &str,
    account: &str,
    year: i32,
    month: Option<i32>,
    amount: Option<&str>,
    annual: Option<&str>,
    currency_code: &str,
    notes: Option<&str>,
    format: OutputFormat,
) -> Result<(), CliError> {
    // Validate account exists
    if !db::account_exists(db.conn(), company, account)? {
        return Err(CliError::NotFound(format!(
            "account '{account}' not found in company '{company}'"
        )));
    }

    let currency = Currency::from_code(currency_code)
        .map_err(|_| CliError::Validation(format!("unknown currency: {currency_code}")))?;

    if let Some(annual_str) = annual {
        // Annual budget -- distribute across 12 months
        let annual_minor = parse_amount_to_minor(annual_str, currency)?;
        let rows = db::set_annual_budget(db.conn(), &db::SetAnnualBudgetParams {
            company_slug: company,
            account_code: account,
            currency: currency_code,
            year,
            annual_amount: annual_minor,
            notes,
        })?;

        match format {
            OutputFormat::Table => {
                let rendered = output::table::render_budgets(&rows, currency.minor_units(), true);
                println!("{rendered}");
            }
            OutputFormat::Json => {
                let meta = output::json::meta("budget.set", Some(company));
                let rendered = output::json::render_budgets(&rows, meta)?;
                println!("{rendered}");
            }
            OutputFormat::Csv => {
                let rendered = output::csv::render_budgets(&rows)?;
                print!("{rendered}");
            }
        }

        if !cli.verbosity.quiet {
            eprintln!(
                "[ok] annual budget set for {account} in {year} (12 months)"
            );
        }
    } else if let Some(amount_str) = amount {
        let m = month.ok_or_else(|| {
            CliError::Usage("--month is required when using --amount".to_string())
        })?;
        if !(1..=12).contains(&m) {
            return Err(CliError::Validation(format!("month must be 1-12, got {m}")));
        }

        let minor = parse_amount_to_minor(amount_str, currency)?;
        let row = db::set_budget(db.conn(), &db::SetBudgetParams {
            company_slug: company,
            account_code: account,
            currency: currency_code,
            year,
            month: m,
            amount: minor,
            notes,
        })?;

        match format {
            OutputFormat::Table => {
                let rendered =
                    output::table::render_budgets(&[row], currency.minor_units(), true);
                println!("{rendered}");
            }
            OutputFormat::Json => {
                let meta = output::json::meta("budget.set", Some(company));
                let rendered = output::json::render_budgets(&[row], meta)?;
                println!("{rendered}");
            }
            OutputFormat::Csv => {
                let rendered = output::csv::render_budgets(&[row])?;
                print!("{rendered}");
            }
        }

        if !cli.verbosity.quiet {
            eprintln!("[ok] budget set for {account} in {year}-{m:02}");
        }
    } else {
        return Err(CliError::Usage(
            "either --month/--amount or --annual must be provided".to_string(),
        ));
    }

    Ok(())
}

/// Execute the `budget list` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_list(
    cli: &Cli,
    db: &Db,
    company: &str,
    year: i32,
    account: Option<&str>,
    month: Option<i32>,
    format: OutputFormat,
) -> Result<(), CliError> {
    let params = db::ListBudgetParams {
        company_slug: company,
        year,
        account_code: account,
        month,
    };
    let rows = db::list_budgets(db.conn(), &params)?;

    // Resolve currency from first row or default USD
    let currency = rows
        .first()
        .and_then(|r| Currency::from_code(&r.currency).ok())
        .unwrap_or(Currency::USD);

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_budgets(&rows, currency.minor_units(), true);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("budget.list", Some(company));
            let rendered = output::json::render_budgets(&rows, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_budgets(&rows)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] {} budget entries", rows.len());
    }

    Ok(())
}

/// Execute the `budget delete` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_delete(
    cli: &Cli,
    db: &Db,
    company: &str,
    account: &str,
    year: i32,
    month: Option<i32>,
    currency: &str,
    force: bool,
) -> Result<(), CliError> {
    if !force && std::io::stdin().is_terminal() {
        let scope = if let Some(m) = month {
            format!("{account} for {year}-{m:02}")
        } else {
            format!("{account} for all of {year}")
        };
        eprint!("Delete budget for {scope}? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).map_err(CliError::Io)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("aborted");
            return Ok(());
        }
    }

    let deleted = db::delete_budget(db.conn(), company, account, currency, year, month)?;

    if cli.is_json() {
        let meta = output::json::meta("budget.delete", Some(company));
        let rendered = output::json::render_budget_deleted(deleted, meta)?;
        println!("{rendered}");
    } else if !cli.verbosity.quiet {
        eprintln!("[ok] deleted {deleted} budget entries");
    }

    Ok(())
}
