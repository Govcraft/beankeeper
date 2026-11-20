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
    let db = Db::open_existing(&cli.db, pp.as_ref())?;
    db::companies::get_company(db.conn(), company)?;
    if let BudgetCommand::Delete {
        account,
        year,
        month,
        force: false,
        ..
    } = sub
        && std::io::stdin().is_terminal()
    {
        let scope = month.map_or_else(
            || format!("{account} for all of {year}"),
            |month| format!("{account} for {year}-{month:02}"),
        );
        eprint!("Delete budget for {scope}? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("aborted");
            return Ok(());
        }
    }
    let actor = db::Actor::resolve(cli.actor.as_deref());
    let result = compute(&db, company, sub, &actor)?;
    render(cli, company, sub, &result)?;
    if !cli.verbosity.quiet {
        match sub {
            BudgetCommand::Set {
                account,
                year,
                annual: Some(_),
                ..
            } => {
                eprintln!("[ok] annual budget set for {account} in {year} (12 months)");
            }
            BudgetCommand::Set {
                account,
                year,
                month: Some(month),
                ..
            } => {
                eprintln!("[ok] budget set for {account} in {year}-{month:02}");
            }
            BudgetCommand::List { .. } => {
                if let BudgetData::Rows(rows) = &result {
                    eprintln!("[ok] {} budget entries", rows.len());
                }
            }
            BudgetCommand::Delete { .. } if !cli.is_json() => {
                if let BudgetData::Deleted(count) = result {
                    eprintln!("[ok] deleted {count} budget entries");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

enum BudgetData {
    Rows(Vec<db::BudgetRow>),
    Deleted(usize),
}

fn compute(
    db: &Db,
    company: &str,
    sub: &BudgetCommand,
    actor: &db::Actor,
) -> Result<BudgetData, CliError> {
    db::companies::get_company(db.conn(), company)?;
    match sub {
        BudgetCommand::Set { .. } => set(db, company, sub, actor).map(BudgetData::Rows),
        BudgetCommand::List {
            year,
            account,
            month,
        } => db::list_budgets(
            db.conn(),
            &db::ListBudgetParams {
                company_slug: company,
                year: *year,
                account_code: account.as_deref(),
                month: *month,
            },
        )
        .map(BudgetData::Rows),
        BudgetCommand::Delete {
            account,
            year,
            month,
            currency,
            ..
        } => db::delete_budget(db.conn(), actor, company, account, currency, *year, *month)
            .map(BudgetData::Deleted),
    }
}

fn set(
    db: &Db,
    company: &str,
    sub: &BudgetCommand,
    actor: &db::Actor,
) -> Result<Vec<db::BudgetRow>, CliError> {
    let BudgetCommand::Set {
        account,
        year,
        month,
        amount,
        annual,
        currency,
        notes,
    } = sub
    else {
        return Err(CliError::Usage("expected budget set".into()));
    };
    if !db::account_exists(db.conn(), company, account)? {
        return Err(CliError::NotFound(format!(
            "account '{account}' not found in company '{company}'"
        )));
    }
    let parsed_currency = Currency::from_code(currency)
        .map_err(|_| CliError::Validation(format!("unknown currency: {currency}")))?;
    if annual.is_some() && (month.is_some() || amount.is_some()) {
        return Err(CliError::Usage(
            "annual conflicts with month and amount".into(),
        ));
    }
    if let Some(annual) = annual {
        return db::set_annual_budget(
            db.conn(),
            actor,
            &db::SetAnnualBudgetParams {
                company_slug: company,
                account_code: account,
                currency,
                year: *year,
                annual_amount: parse_amount_to_minor(annual, parsed_currency)?,
                notes: notes.as_deref(),
            },
        );
    }
    let Some(amount) = amount else {
        return Err(CliError::Usage(
            "either --month/--amount or --annual must be provided".into(),
        ));
    };
    let month =
        month.ok_or_else(|| CliError::Usage("--month is required when using --amount".into()))?;
    if !(1..=12).contains(&month) {
        return Err(CliError::Validation(format!(
            "month must be 1-12, got {month}"
        )));
    }
    let row = db::set_budget(
        db.conn(),
        actor,
        &db::SetBudgetParams {
            company_slug: company,
            account_code: account,
            currency,
            year: *year,
            month,
            amount: parse_amount_to_minor(amount, parsed_currency)?,
            notes: notes.as_deref(),
        },
    )?;
    Ok(vec![row])
}

fn command(sub: &BudgetCommand) -> &'static str {
    match sub {
        BudgetCommand::Set { .. } => "budget.set",
        BudgetCommand::List { .. } => "budget.list",
        BudgetCommand::Delete { .. } => "budget.delete",
    }
}

fn json(company: &str, sub: &BudgetCommand, data: &BudgetData) -> Result<String, CliError> {
    let meta = output::json::meta(command(sub), Some(company));
    match data {
        BudgetData::Rows(rows) => output::json::render_budgets(rows, meta),
        BudgetData::Deleted(count) => output::json::render_budget_deleted(*count, meta),
    }
}

fn render(
    cli: &Cli,
    company: &str,
    sub: &BudgetCommand,
    data: &BudgetData,
) -> Result<(), CliError> {
    let format = resolve_format(None, cli);
    if format == OutputFormat::Json {
        println!("{}", json(company, sub, data)?);
    } else if let BudgetData::Rows(rows) = data {
        match format {
            OutputFormat::Table => {
                let currency = rows
                    .first()
                    .and_then(|row| Currency::from_code(&row.currency).ok())
                    .unwrap_or(Currency::USD);
                println!(
                    "{}",
                    output::table::render_budgets(
                        rows,
                        currency.minor_units(),
                        output::should_use_color(cli.verbosity.no_color)
                    )
                );
            }
            OutputFormat::Csv => print!("{}", output::csv::render_budgets(rows)?),
            OutputFormat::Json => {}
        }
    }
    Ok(())
}

/// Execute a budget operation without writing to stdout or prompting.
///
/// The supplied actor attributes every mutation in the audit log.
///
/// # Errors
///
/// Returns the same validation and database errors as the CLI.
pub fn execute_json(
    db: &Db,
    company: &str,
    sub: &BudgetCommand,
    actor: &db::Actor,
) -> Result<serde_json::Value, CliError> {
    let result = compute(db, company, sub, actor)?;
    super::company::json_data(&json(company, sub, &result)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AccountCommand, AccountTypeArg, AuditCommand, CompanyCommand};

    fn ledger() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        db::companies::create_company(db.conn(), "acme", "Acme", None)?;
        super::super::account::execute_json(
            &db,
            "acme",
            &AccountCommand::Create {
                code: "5000".into(),
                name: "Rent".into(),
                account_type: AccountTypeArg::Expense,
                default_tax_category: None,
            },
        )?;
        Ok(db)
    }

    fn monthly(month: i32) -> BudgetCommand {
        BudgetCommand::Set {
            account: "5000".into(),
            year: 2026,
            month: Some(month),
            amount: Some("12.34".into()),
            annual: None,
            currency: "USD".into(),
            notes: Some("Office".into()),
        }
    }

    #[test]
    fn json_operations_share_rows_and_attribute_mutations() -> Result<(), CliError> {
        let db = ledger()?;
        let actor = db::Actor::new("mcp:test-client");
        let companies = super::super::company::execute_json(&db, &CompanyCommand::List)?;
        assert_eq!(companies[0]["slug"], "acme");
        let set = execute_json(&db, "acme", &monthly(3), &actor)?;
        assert_eq!(set[0]["amount"], 1234);
        let listed = execute_json(
            &db,
            "acme",
            &BudgetCommand::List {
                year: 2026,
                account: None,
                month: None,
            },
            &actor,
        )?;
        assert_eq!(set, listed);
        let audit = super::super::audit::execute_json(
            &db,
            &AuditCommand::Log {
                company: Some("acme".into()),
                entity: None,
                action: None,
                limit: 50,
                offset: 0,
            },
        )?;
        assert_eq!(audit[0]["actor"], "mcp:test-client");
        let deleted = execute_json(
            &db,
            "acme",
            &BudgetCommand::Delete {
                account: "5000".into(),
                year: 2026,
                month: None,
                currency: "USD".into(),
                force: true,
            },
            &actor,
        )?;
        assert_eq!(deleted["deleted"], 1);
        let verified = super::super::verify::execute_json(&db)?;
        assert_eq!(verified["status"], "healthy");
        Ok(())
    }

    #[test]
    fn invalid_budget_does_not_write() -> Result<(), CliError> {
        let db = ledger()?;
        let actor = db::Actor::new("mcp:test-client");
        assert!(matches!(
            execute_json(&db, "acme", &monthly(13), &actor),
            Err(CliError::Validation(_))
        ));
        assert!(matches!(
            execute_json(&db, "unknown", &monthly(3), &actor),
            Err(CliError::NotFound(_))
        ));
        let audit = super::super::audit::execute_json(
            &db,
            &AuditCommand::Log {
                company: None,
                entity: None,
                action: None,
                limit: 50,
                offset: 0,
            },
        )?;
        assert_eq!(audit, serde_json::json!([]));
        Ok(())
    }
}
