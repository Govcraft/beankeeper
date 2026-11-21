//! `bk budget` command handlers.

use std::io::IsTerminal;

use crate::cli::{BudgetCommand, BudgetMonth, BudgetSetArgs, Cli, OutputFormat, resolve_format};
use crate::commands::txn::parse_amount_to_minor;
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;

/// Run a `bk budget` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &BudgetCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
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
            |month| format!("{account} for {year}-{:02}", month.get()),
        );
        eprint!("Delete budget for {scope}? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("aborted");
            return Ok(());
        }
    }
    let actor = cli.principal();
    let result = compute(&db, company, sub, &actor)?;
    render(cli, company, sub, &result)?;
    if !cli.verbosity.quiet {
        match sub {
            BudgetCommand::Set(BudgetSetArgs {
                account,
                year,
                annual: Some(_),
                ..
            }) => {
                eprintln!("[ok] annual budget set for {account} in {year} (12 months)");
            }
            BudgetCommand::Set(BudgetSetArgs {
                account,
                year,
                month: Some(month),
                ..
            }) => {
                eprintln!("[ok] budget set for {account} in {year}-{:02}", month.get());
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
        BudgetCommand::Set(args) => set(db, company, args, actor).map(BudgetData::Rows),
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
                month: month.map(BudgetMonth::get),
            },
        )
        .map(BudgetData::Rows),
        BudgetCommand::Delete {
            account,
            year,
            month,
            currency,
            ..
        } => db::delete_budget(
            db.conn(),
            actor,
            company,
            account,
            currency.resolve()?,
            *year,
            month.map(BudgetMonth::get),
        )
        .map(BudgetData::Deleted),
    }
}

fn set(
    db: &Db,
    company: &str,
    args: &BudgetSetArgs,
    actor: &db::Actor,
) -> Result<Vec<db::BudgetRow>, CliError> {
    let BudgetSetArgs {
        account,
        year,
        month,
        amount,
        annual,
        currency,
        notes,
    } = args;
    if !db::account_exists(db.conn(), company, account)? {
        return Err(CliError::NotFound(format!(
            "account '{account}' not found in company '{company}'"
        )));
    }
    let currency = currency.resolve()?;
    let scope = match (month, amount.as_deref(), annual.as_deref()) {
        (Some(month), Some(amount), None) => BudgetScope::Monthly {
            month: *month,
            amount,
        },
        (None, None, Some(amount)) => BudgetScope::Annual(amount),
        _ => {
            return Err(CliError::Usage(
                "provide --month and --amount together, or --annual".into(),
            ));
        }
    };
    match scope {
        BudgetScope::Annual(annual) => db::set_annual_budget(
            db.conn(),
            actor,
            &db::SetAnnualBudgetParams {
                company_slug: company,
                account_code: account,
                currency,
                year: *year,
                annual_amount: parse_amount_to_minor(annual, currency)?,
                notes: notes.as_deref(),
            },
        ),
        BudgetScope::Monthly { month, amount } => db::set_budget(
            db.conn(),
            actor,
            &db::SetBudgetParams {
                company_slug: company,
                account_code: account,
                currency,
                year: *year,
                month: month.get(),
                amount: parse_amount_to_minor(amount, currency)?,
                notes: notes.as_deref(),
            },
        )
        .map(|row| vec![row]),
    }
}

enum BudgetScope<'a> {
    Monthly { month: BudgetMonth, amount: &'a str },
    Annual(&'a str),
}

fn command(sub: &BudgetCommand) -> &'static str {
    match sub {
        BudgetCommand::Set(_) => "budget.set",
        BudgetCommand::List { .. } => "budget.list",
        BudgetCommand::Delete { .. } => "budget.delete",
    }
}

fn json(data: &BudgetData, meta: output::json::Meta) -> Result<String, CliError> {
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
        println!("{}", json(data, cli.meta(command(sub), Some(company)))?);
    } else if let BudgetData::Rows(rows) = data {
        match format {
            OutputFormat::Table => println!(
                "{}",
                output::table::render_budgets(
                    rows,
                    output::should_use_color(cli.verbosity.no_color),
                )?
            ),
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
    super::company::json_data(&json(
        &result,
        output::json::meta(command(sub), Some(company)),
    )?)
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

    fn monthly(amount: &str) -> BudgetCommand {
        BudgetCommand::Set(BudgetSetArgs {
            account: "5000".into(),
            year: 2026,
            month: Some("3".parse().unwrap_or_else(|error| panic!("{error}"))),
            amount: Some(amount.into()),
            annual: None,
            currency: beankeeper::types::Currency::USD.into(),
            notes: Some("Office".into()),
        })
    }

    #[test]
    fn json_operations_share_rows_and_attribute_mutations() -> Result<(), CliError> {
        let db = ledger()?;
        let actor = db::Actor::new("mcp:test-client");
        let companies = super::super::company::execute_json(&db, &CompanyCommand::List)?;
        assert_eq!(companies[0]["slug"], "acme");
        let set = execute_json(&db, "acme", &monthly("12.34"), &actor)?;
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
                agent: None,
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
                currency: beankeeper::types::Currency::USD.into(),
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
        let log = AuditCommand::Log {
            agent: None,
            company: None,
            entity: None,
            action: None,
            limit: 50,
            offset: 0,
        };
        let before = super::super::audit::execute_json(&db, &log)?;
        assert!(matches!(
            execute_json(&db, "acme", &monthly("999999999999999999999999999"), &actor),
            Err(CliError::Validation(_))
        ));
        assert!(matches!(
            execute_json(&db, "unknown", &monthly("12.34"), &actor),
            Err(CliError::NotFound(_))
        ));
        let after = super::super::audit::execute_json(&db, &log)?;
        assert_eq!(after, before);
        Ok(())
    }
}
