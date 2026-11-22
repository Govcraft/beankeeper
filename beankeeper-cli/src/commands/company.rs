use crate::cli::{Cli, CompanyCommand, OutputFormat, resolve_format};
use crate::confirmation::confirm_deletion;
use crate::db::companies;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::identity::{Admin, Granted};
use crate::operation::Operation;
use crate::output;

/// Run a `bk company` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, sub: &CompanyCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        CompanyCommand::Create {
            slug,
            name,
            description,
        } => {
            let row = Granted::<Admin>::with_db(&db, &cli.principal(), |grant| {
                companies::create_company(grant, slug, name, description.as_deref())
            })?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Created company: {} ({})", row.slug, row.name);
            }
            render_companies(&[row], Operation::CompanyCreate, format, use_color, cli)?;
        }
        CompanyCommand::List => {
            let rows = companies::list_companies(db.read())?;
            render_companies(&rows, Operation::CompanyList, format, use_color, cli)?;
            if !cli.verbosity.quiet {
                let count = rows.len();
                eprintln!(
                    "{count} {noun}",
                    noun = if count == 1 { "company" } else { "companies" }
                );
            }
        }
        CompanyCommand::Show { slug } => {
            let row = companies::get_company(db.read(), slug)?;
            render_companies(&[row], Operation::CompanyShow, format, use_color, cli)?;
        }
        CompanyCommand::Delete { slug, force } => {
            let Some(confirmed) = confirm_deletion(*force, &format!("company '{slug}'"))? else {
                eprintln!("Aborted.");
                return Ok(());
            };
            let actor = cli.principal();
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                crate::db::delete_company(grant, slug, confirmed)
            })?;
            if format == OutputFormat::Json {
                let meta = cli.meta(Operation::CompanyDelete, None);
                let rendered = output::json::render_deleted(slug.as_str(), meta)?;
                println!("{rendered}");
            }
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
    command: Operation,
    format: OutputFormat,
    use_color: bool,
    cli: &Cli,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_companies(rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = cli.meta(command, None);
            let rendered = output::json::render_companies(rows, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_companies(rows)?;
            print!("{rendered}");
        }
    }
    Ok(())
}

/// Execute a company read without writing to stdout.
///
/// # Errors
///
/// Returns the same validation and database errors as the CLI.
pub fn execute_json(db: &Db, sub: &CompanyCommand) -> Result<serde_json::Value, CliError> {
    let (command, rows) = match sub {
        CompanyCommand::List => (
            Operation::CompanyList,
            companies::list_companies(db.read())?,
        ),
        CompanyCommand::Show { slug } => (
            Operation::CompanyShow,
            vec![companies::get_company(db.read(), slug)?],
        ),
        _ => return Err(CliError::Usage("unsupported MCP company operation".into())),
    };
    json_data(&output::json::render_companies(
        &rows,
        output::json::meta(command, None),
    )?)
}

pub(crate) fn json_data(rendered: &str) -> Result<serde_json::Value, CliError> {
    let mut envelope: serde_json::Value = serde_json::from_str(rendered)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
    envelope
        .get_mut("data")
        .map(serde_json::Value::take)
        .ok_or_else(|| CliError::General("JSON response is missing data".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_company_api_never_creates_or_deletes() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let create = CompanyCommand::Create {
            slug: "acme".parse()?,
            name: "Acme".into(),
            description: None,
        };
        assert!(matches!(
            execute_json(&db, &create),
            Err(CliError::Usage(_))
        ));
        assert_eq!(
            execute_json(&db, &CompanyCommand::List)?,
            serde_json::json!([])
        );
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            companies::create_company(grant, &"acme".parse()?, "Acme", None)
        })?;
        let delete = CompanyCommand::Delete {
            slug: "acme".parse()?,
            force: true,
        };
        assert!(matches!(
            execute_json(&db, &delete),
            Err(CliError::Usage(_))
        ));
        let shown = execute_json(
            &db,
            &CompanyCommand::Show {
                slug: "acme".parse()?,
            },
        )?;
        assert_eq!(shown[0]["name"], "Acme");
        Ok(())
    }
}
