//! Manage bank import categorization rules.

use crate::cli::{Cli, OutputFormat, RuleCommand, resolve_format};
use crate::confirmation::confirm_deletion;
use crate::db::{audit, companies, rules};
use crate::error::CliError;
use crate::operation::Operation;
use crate::output::json;

/// Execute a company-scoped rule command.
///
/// # Errors
/// Returns validation, storage, or output errors.
pub fn run(cli: &Cli, company: &str, command: &RuleCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    companies::read_company(db.read(), company)?;
    let actor = cli.principal();
    let (name, rows) = match command {
        RuleCommand::Create {
            contains,
            account,
            priority,
        } => (
            Operation::RuleCreate,
            vec![rules::create_rule(
                db.conn(),
                &actor,
                company,
                contains.clone(),
                account,
                *priority,
            )?],
        ),
        RuleCommand::List => (Operation::RuleList, rules::list_rules(db.read(), company)?),
        RuleCommand::Delete { id, force } => {
            let Some(confirmed) = confirm_deletion(*force, &format!("rule {id}"))? else {
                eprintln!("aborted");
                return Ok(());
            };
            audit::delete_rule(db.conn(), &actor, company, id, confirmed)?;
            return render_deleted(cli, company, id);
        }
    };
    if cli.verbosity.quiet && resolve_format(None, cli) != OutputFormat::Json {
        return Ok(());
    }
    match resolve_format(None, cli) {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(cli.meta(name, Some(company)), &rows)?
        ),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer
                .write_record(["id", "contains", "account_code", "priority"])
                .map_err(|e| CliError::General(e.to_string()))?;
            for row in &rows {
                writer
                    .write_record([
                        row.id.to_string(),
                        row.contains.to_string(),
                        row.account_code.clone(),
                        row.priority.to_string(),
                    ])
                    .map_err(|e| CliError::General(e.to_string()))?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => {
            let mut table = comfy_table::Table::new();
            table.set_header(["ID", "Contains", "Account", "Priority"]);
            for row in &rows {
                table.add_row([
                    row.id.to_string(),
                    row.contains.to_string(),
                    row.account_code.clone(),
                    row.priority.to_string(),
                ]);
            }
            println!("{table}");
        }
    }
    Ok(())
}

fn render_deleted(cli: &Cli, company: &str, id: &rules::RuleId) -> Result<(), CliError> {
    if cli.verbosity.quiet && resolve_format(None, cli) != OutputFormat::Json {
        return Ok(());
    }
    match resolve_format(None, cli) {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(
                cli.meta(Operation::RuleDelete, Some(company)),
                serde_json::json!({"deleted": id})
            )?
        ),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            for record in [vec!["deleted".to_string()], vec![id.to_string()]] {
                writer
                    .write_record(record)
                    .map_err(|e| CliError::General(e.to_string()))?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => println!("Deleted rule {id}"),
    }
    Ok(())
}
