//! Typed company operations and CLI rendering.

use serde::{Serialize, Serializer};

use crate::cli::{Cli, CompanyCommand, OutputFormat, resolve_format};
use crate::confirmation::{Confirmed, confirm_deletion};
use crate::db::{
    Actor, CompanyRow, Db,
    companies::{self, CompanyName, CompanySlug},
};
use crate::error::CliError;
use crate::identity::{Admin, Granted};
use crate::operation::Operation;
use crate::output;

/// An operation on the company catalog.
pub enum CompanyRequest {
    Create {
        slug: CompanySlug,
        name: CompanyName,
        description: Option<String>,
    },
    List,
    Show(CompanySlug),
    Delete(CompanySlug, Confirmed),
}

impl CompanyRequest {
    /// Canonical operation for capability checks and output metadata.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Create { .. } => Operation::CompanyCreate,
            Self::List => Operation::CompanyList,
            Self::Show(_) => Operation::CompanyShow,
            Self::Delete(..) => Operation::CompanyDelete,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<CompanyResponse, CliError> {
        let operation = self.operation();
        let data = match self {
            Self::Create {
                slug,
                name,
                description,
            } => {
                let row = Granted::<Admin>::with_db(db, actor, |grant| {
                    companies::create_company(grant, &slug, &name, description.as_deref())
                })?;
                CompanyData::Rows(vec![row])
            }
            Self::List => CompanyData::Rows(companies::list_companies(db.read())?),
            Self::Show(slug) => CompanyData::Rows(vec![companies::get_company(db.read(), &slug)?]),
            Self::Delete(slug, confirmed) => {
                Granted::<Admin>::with_db(db, actor, |grant| {
                    crate::db::delete_company(grant, &slug, confirmed)
                })?;
                CompanyData::Deleted(slug)
            }
        };
        Ok(CompanyResponse { operation, data })
    }
}

enum CompanyData {
    Rows(Vec<CompanyRow>),
    Deleted(CompanySlug),
}

/// Result of a company operation, with the existing JSON data shape.
pub struct CompanyResponse {
    operation: Operation,
    data: CompanyData,
}

impl Serialize for CompanyResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.data {
            CompanyData::Rows(rows) => rows.serialize(serializer),
            CompanyData::Deleted(slug) => {
                output::json::deleted_data(slug.as_str()).serialize(serializer)
            }
        }
    }
}

impl CompanyResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if self.operation == Operation::CompanyCreate
            && !cli.verbosity.quiet
            && let CompanyData::Rows(rows) = &self.data
            && let Some(row) = rows.first()
        {
            eprintln!("[ok] Created company: {} ({})", row.slug, row.name);
        }
        match (format, &self.data) {
            (OutputFormat::Json, _) => println!(
                "{}",
                output::json::envelope_ok(cli.meta(self.operation, None), self)?
            ),
            (OutputFormat::Table, CompanyData::Rows(rows)) => println!(
                "{}",
                output::table::render_companies(
                    rows,
                    output::should_use_color(cli.verbosity.no_color)
                )
            ),
            (OutputFormat::Csv, CompanyData::Rows(rows)) => {
                print!("{}", output::csv::render_companies(rows)?);
            }
            (_, CompanyData::Deleted(_)) => {}
        }
        if !cli.verbosity.quiet {
            match &self.data {
                CompanyData::Rows(rows) if self.operation == Operation::CompanyList => {
                    let count = rows.len();
                    let noun = if count == 1 { "company" } else { "companies" };
                    eprintln!("{count} {noun}");
                }
                CompanyData::Deleted(slug) => eprintln!("[ok] Deleted company: {slug}"),
                CompanyData::Rows(_) => {}
            }
        }
        Ok(())
    }
}

impl TryFrom<&CompanyCommand> for Option<CompanyRequest> {
    type Error = CliError;

    fn try_from(command: &CompanyCommand) -> Result<Self, Self::Error> {
        Ok(Some(match command {
            CompanyCommand::Create {
                slug,
                name,
                description,
            } => CompanyRequest::Create {
                slug: slug.clone(),
                name: name.clone(),
                description: description.clone(),
            },
            CompanyCommand::List => CompanyRequest::List,
            CompanyCommand::Show { slug } => CompanyRequest::Show(slug.clone()),
            CompanyCommand::Delete { slug, force } => {
                let Some(confirmed) = confirm_deletion(*force, &format!("company '{slug}'"))?
                else {
                    return Ok(None);
                };
                CompanyRequest::Delete(slug.clone(), confirmed)
            }
        }))
    }
}
