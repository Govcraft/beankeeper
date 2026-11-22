//! `bk budget` command handlers.

use beankeeper::types::Currency;

use crate::cli::{BudgetCommand, BudgetMonth, BudgetSetArgs, Cli, OutputFormat, resolve_format};
use crate::commands::txn::parse_amount_to_minor;
use crate::confirmation::confirm_deletion;
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::identity::{Budget, Granted};
use crate::operation::Operation;
use crate::output;

/// Run a `bk budget` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &BudgetCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let actor = cli.principal();
    let Some(result) = compute(&db, company, sub, &actor)? else {
        eprintln!("aborted");
        return Ok(());
    };
    render(cli, company, command(sub), &result)?;
    if !cli.verbosity.quiet {
        match result {
            BudgetData::Set { request, .. } => request.print_notice(),
            BudgetData::Rows(rows) => eprintln!("[ok] {} budget entries", rows.len()),
            BudgetData::Deleted(count) if !cli.is_json() => {
                eprintln!("[ok] deleted {count} budget entries");
            }
            BudgetData::Deleted(_) => {}
        }
    }
    Ok(())
}

enum BudgetData<'a> {
    Set {
        rows: Vec<db::BudgetRow>,
        request: BudgetSetRequest<'a>,
    },
    Rows(Vec<db::BudgetRow>),
    Deleted(usize),
}

fn compute<'a>(
    db: &Db,
    company: &str,
    sub: &'a BudgetCommand,
    actor: &db::Actor,
) -> Result<Option<BudgetData<'a>>, CliError> {
    db::companies::get_company(db.read(), &company.parse()?)?;
    match sub {
        BudgetCommand::Set(args) => {
            let request = BudgetSetRequest::from_cli(args)?;
            let rows = set(db, company, &request, actor)?;
            Ok(BudgetData::Set { rows, request })
        }
        BudgetCommand::List {
            year,
            account,
            month,
        } => db::list_budgets(
            &crate::db::companies::Company::bind(db.read(), company.parse()?)?,
            &db::ListBudgetParams {
                year: *year,
                account_code: account.as_deref(),
                month: month.map(BudgetMonth::calendar_month),
            },
        )
        .map(BudgetData::Rows),
        BudgetCommand::Delete {
            account,
            year,
            month,
            currency,
            force,
        } => {
            let scope = month.map_or_else(
                || format!("{account} for all of {year}"),
                |month| format!("{account} for {year}-{:02}", month.get()),
            );
            let Some(confirmed) = confirm_deletion(*force, &format!("budget for {scope}"))? else {
                return Ok(None);
            };
            Granted::<Budget>::with_db(db, actor, |grant| {
                db::delete_budget(
                    grant,
                    &db::budgets::DeleteBudgetParams {
                        company_slug: &company.parse()?,
                        account_code: account,
                        currency: currency.resolve()?,
                        year: *year,
                        month: month.map(BudgetMonth::calendar_month),
                    },
                    confirmed,
                )
            })
            .map(BudgetData::Deleted)
        }
    }
    .map(Some)
}

#[derive(Clone, Copy)]
pub(crate) enum BudgetScope<'a> {
    Monthly { month: BudgetMonth, amount: &'a str },
    Annual(&'a str),
}

impl<'a> BudgetScope<'a> {
    pub(crate) fn from_flags(
        month: Option<BudgetMonth>,
        amount: Option<&'a str>,
        annual: Option<&'a str>,
    ) -> Result<Self, CliError> {
        match (month, amount, annual) {
            (Some(month), Some(amount), None) => Ok(Self::Monthly { month, amount }),
            (None, None, Some(amount)) => Ok(Self::Annual(amount)),
            _ => Err(CliError::Usage(
                "provide --month and --amount together, or --annual".into(),
            )),
        }
    }
}

pub(crate) struct BudgetSetRequest<'a> {
    pub(crate) account: &'a str,
    pub(crate) year: i32,
    pub(crate) scope: BudgetScope<'a>,
    pub(crate) currency: Currency,
    pub(crate) notes: Option<&'a str>,
}

impl<'a> BudgetSetRequest<'a> {
    fn from_cli(args: &'a BudgetSetArgs) -> Result<Self, CliError> {
        Ok(Self {
            account: &args.account,
            year: args.year,
            currency: args.currency.resolve()?,
            scope: BudgetScope::from_flags(
                args.month,
                args.amount.as_deref(),
                args.annual.as_deref(),
            )?,
            notes: args.notes.as_deref(),
        })
    }

    fn print_notice(&self) {
        let Self {
            account,
            year,
            scope,
            ..
        } = self;
        match scope {
            BudgetScope::Annual(_) => {
                eprintln!("[ok] annual budget set for {account} in {year} (12 months)");
            }
            BudgetScope::Monthly { month, .. } => {
                eprintln!("[ok] budget set for {account} in {year}-{:02}", month.get());
            }
        }
    }
}

fn set(
    db: &Db,
    company: &str,
    request: &BudgetSetRequest<'_>,
    actor: &db::Actor,
) -> Result<Vec<db::BudgetRow>, CliError> {
    let BudgetSetRequest {
        account,
        year,
        scope,
        currency,
        notes,
    } = *request;
    if !db::account_exists(
        &crate::db::companies::Company::bind(db.read(), company.parse()?)?,
        account,
    )? {
        return Err(CliError::NotFound(format!(
            "account '{account}' not found in company '{company}'"
        )));
    }
    Granted::<Budget>::with_db(db, actor, |grant| match scope {
        BudgetScope::Annual(annual) => db::set_annual_budget(
            grant,
            &db::SetAnnualBudgetParams {
                company_slug: &company.parse()?,
                account_code: account,
                currency,
                year,
                annual_amount: parse_amount_to_minor(annual, currency)?,
                notes,
            },
        ),
        BudgetScope::Monthly { month, amount } => db::set_budget(
            grant,
            &db::SetBudgetParams {
                company_slug: &company.parse()?,
                account_code: account,
                currency,
                year,
                month: month.calendar_month(),
                amount: parse_amount_to_minor(amount, currency)?,
                notes,
            },
        )
        .map(|row| vec![row]),
    })
}

fn command(sub: &BudgetCommand) -> Operation {
    match sub {
        BudgetCommand::Set(_) => Operation::BudgetSet,
        BudgetCommand::List { .. } => Operation::BudgetList,
        BudgetCommand::Delete { .. } => Operation::BudgetDelete,
    }
}

fn json(data: &BudgetData<'_>, meta: output::json::Meta) -> Result<String, CliError> {
    match data {
        BudgetData::Set { rows, .. } | BudgetData::Rows(rows) => {
            output::json::render_budgets(rows, meta)
        }
        BudgetData::Deleted(count) => output::json::render_budget_deleted(*count, meta),
    }
}

fn render(
    cli: &Cli,
    company: &str,
    operation: Operation,
    data: &BudgetData<'_>,
) -> Result<(), CliError> {
    let format = resolve_format(None, cli);
    if format == OutputFormat::Json {
        println!("{}", json(data, cli.meta(operation, Some(company)))?);
    } else if let BudgetData::Set { rows, .. } | BudgetData::Rows(rows) = data {
        match format {
            OutputFormat::Table => println!(
                "{}",
                output::table::render_budgets(
                    rows,
                    output::should_use_color(cli.verbosity.no_color),
                )
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
    let Some(result) = compute(db, company, sub, actor)? else {
        return Ok(serde_json::json!({"aborted": true}));
    };
    super::company::json_data(&json(
        &result,
        output::json::meta(command(sub), Some(company)),
    )?)
}

pub(crate) fn execute_set_json(
    db: &Db,
    company: &str,
    request: &BudgetSetRequest<'_>,
    actor: &db::Actor,
) -> Result<serde_json::Value, CliError> {
    db::companies::get_company(db.read(), &company.parse()?)?;
    let rows = set(db, company, request, actor)?;
    super::company::json_data(&output::json::render_budgets(
        &rows,
        output::json::meta(Operation::BudgetSet, Some(company)),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AccountCommand, CompanyCommand};
    use crate::identity::{Admin, Granted};

    fn ledger() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            db::companies::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        super::super::account::execute_json(
            &db,
            &"acme".parse()?,
            &AccountCommand::Create {
                code: "5000".into(),
                name: "Rent".into(),
                account_type: beankeeper::types::AccountType::Expense,
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
        let actor = db::Actor::new("mcp:test-client".parse()?);
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
            &db::ListAuditParams {
                actor: None,
                agent_uri: None,
                company_slug: Some(&"acme".parse()?),
                entity: None,
                action: None,
                limit: 50.try_into()?,
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
        let actor = db::Actor::new("mcp:test-client".parse()?);
        let log = db::ListAuditParams {
            actor: None,
            agent_uri: None,
            company_slug: None,
            entity: None,
            action: None,
            limit: 50.try_into()?,
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
