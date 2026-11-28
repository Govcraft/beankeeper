//! Typed CLI and MCP inventory entry points.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::inventory::{self as writes, LinkRequest, MovementRequest},
        companies::{Company, CompanySlug},
        documents::DocumentId,
        inventory::{
            self, DocumentLineNumber, DocumentLink, InventoryItemId, InventoryValuation, Item,
            ItemTerms, Movement, StockMovementId,
        },
    },
    error::CliError,
    operation::Operation,
    output::json::envelope_ok,
    request::CalendarDateArg,
};

pub(crate) mod wire;
use wire::MoveTerms;

/// Register stock, record physical activity, and report historical inventory value.
#[derive(Debug, Args)]
pub struct InventoryArgs {
    /// Company whose stock to manage.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
    #[command(subcommand)]
    pub command: InventoryCommand,
}

/// Commands on one company-owned inventory register.
#[derive(Debug, Subcommand)]
pub enum InventoryCommand {
    /// Register a SKU and its stock, COGS, receiving and adjustment accounts.
    Register {
        #[arg(long)]
        terms: PathBuf,
    },
    /// List up to 100 registered SKUs.
    List,
    /// Show a registered item.
    Show { id: InventoryItemId },
    /// Record a receipt, issue, return, or count adjustment using a JSON request.
    Move {
        #[arg(long)]
        request: PathBuf,
    },
    /// Show a physical stock movement and its journal.
    Movement { id: StockMovementId },
    /// List up to 100 item movements through a date.
    Movements {
        item: InventoryItemId,
        #[arg(long)]
        as_of: CalendarDateArg,
    },
    /// Match a receipt to a bill line or an issue to an invoice line.
    MatchDocument {
        movement: StockMovementId,
        #[arg(long)]
        document: DocumentId,
        #[arg(long)]
        line: DocumentLineNumber,
        #[arg(long)]
        date: CalendarDateArg,
    },
    /// Value remaining FIFO lots as of a date and compare with stock accounts.
    Valuation {
        #[arg(long)]
        as_of: CalendarDateArg,
    },
}

impl InventoryCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Register { .. } => Operation::StockRegister,
            Self::List => Operation::StockList,
            Self::Show { .. } => Operation::StockShow,
            Self::Move { .. } => Operation::StockMove,
            Self::Movement { .. } => Operation::StockMovement,
            Self::Movements { .. } => Operation::StockMovements,
            Self::MatchDocument { .. } => Operation::StockMatchDocument,
            Self::Valuation { .. } => Operation::StockValuation,
        }
    }
}

/// Inventory operation shared by both transport layers.
pub struct InventoryRequest {
    pub(crate) company: CompanySlug,
    pub(crate) work: Work,
}

pub(crate) enum Work {
    Register(Box<ItemTerms>),
    List,
    Show(InventoryItemId),
    Move(Box<MovementRequest>),
    Movement(StockMovementId),
    Movements(InventoryItemId, NaiveDate),
    MatchDocument(LinkRequest),
    Valuation(NaiveDate),
}

impl TryFrom<(&CompanySlug, &InventoryCommand)> for InventoryRequest {
    type Error = CliError;

    fn try_from(
        (company, command): (&CompanySlug, &InventoryCommand),
    ) -> Result<Self, Self::Error> {
        let work = match command {
            InventoryCommand::Register { terms } => Work::Register(Box::new(json_input(terms)?)),
            InventoryCommand::List => Work::List,
            InventoryCommand::Show { id } => Work::Show(id.clone()),
            InventoryCommand::Move { request } => {
                Work::Move(Box::new(json_input::<MoveTerms>(request)?.into()))
            }
            InventoryCommand::Movement { id } => Work::Movement(id.clone()),
            InventoryCommand::Movements { item, as_of } => {
                Work::Movements(item.clone(), (*as_of).into())
            }
            InventoryCommand::MatchDocument {
                movement,
                document,
                line,
                date,
            } => Work::MatchDocument(LinkRequest {
                movement: movement.clone(),
                document: document.clone(),
                line: *line,
                on: (*date).into(),
            }),
            InventoryCommand::Valuation { as_of } => Work::Valuation((*as_of).into()),
        };
        Ok(Self {
            company: company.clone(),
            work,
        })
    }
}

fn json_input<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    if !path.is_file() {
        return Err(CliError::Validation(
            "inventory JSON must be a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(65_537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(CliError::Validation(
            "inventory JSON must fit in 64 KiB".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| CliError::Validation(format!("invalid inventory JSON: {error}")))
}

impl InventoryRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self.work {
            Work::Register(_) => Operation::StockRegister,
            Work::List => Operation::StockList,
            Work::Show(_) => Operation::StockShow,
            Work::Move(_) => Operation::StockMove,
            Work::Movement(_) => Operation::StockMovement,
            Work::Movements(_, _) => Operation::StockMovements,
            Work::MatchDocument(_) => Operation::StockMatchDocument,
            Work::Valuation(_) => Operation::StockValuation,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<InventoryResponse, CliError> {
        let operation = self.operation();
        let company = self.company;
        let copy = || company.clone();
        let data = match self.work {
            Work::Register(terms) => {
                Data::Item(Box::new(writes::register(db, actor, &company, *terms)?))
            }
            Work::List => Data::Items(
                db.read_snapshot(|r| inventory::list_items(&Company::bind(r, copy())?))?,
            ),
            Work::Show(id) => {
                Data::Item(Box::new(db.read_snapshot(|r| {
                    inventory::get_item(&Company::bind(r, copy())?, &id)
                })?))
            }
            Work::Move(input) => {
                Data::Movement(Box::new(writes::move_stock(db, actor, &company, *input)?))
            }
            Work::Movement(id) => {
                Data::Movement(Box::new(db.read_snapshot(|r| {
                    inventory::get_movement(&Company::bind(r, copy())?, &id)
                })?))
            }
            Work::Movements(item, as_of) => Data::Movements(db.read_snapshot(|r| {
                inventory::list_movements(&Company::bind(r, copy())?, &item, as_of)
            })?),
            Work::MatchDocument(request) => {
                Data::Link(writes::match_document(db, actor, &company, &request)?)
            }
            Work::Valuation(as_of) => Data::Valuation(
                db.read_snapshot(|r| inventory::valuation(&Company::bind(r, copy())?, as_of))?,
            ),
        };
        Ok(InventoryResponse {
            operation,
            company,
            data,
        })
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Data {
    Item(Box<Item>),
    Items(Vec<Item>),
    Movement(Box<Movement>),
    Movements(Vec<Movement>),
    Link(DocumentLink),
    Valuation(InventoryValuation),
}

/// Result of a typed inventory operation.
pub struct InventoryResponse {
    operation: Operation,
    company: CompanySlug,
    data: Data,
}

impl InventoryResponse {
    pub(crate) const fn data(&self) -> &Data {
        &self.data
    }

    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => println!(
                "{}",
                envelope_ok(
                    cli.meta(self.operation, Some(self.company.as_str())),
                    &self.data
                )?
            ),
            OutputFormat::Table => self.table(),
            OutputFormat::Csv => self.csv()?,
        }
        Ok(())
    }

    fn table(&self) {
        match &self.data {
            Data::Item(item) => println!("{item}"),
            Data::Items(items) => {
                for item in items {
                    println!("{item}");
                }
            }
            Data::Movement(movement) => println!("{movement}"),
            Data::Movements(movements) => {
                for movement in movements {
                    println!("{movement}");
                }
            }
            Data::Link(link) => println!(
                "{} → {} line {}",
                link.movement,
                link.document,
                u16::from(link.line)
            ),
            Data::Valuation(report) => {
                for row in report.summary() {
                    println!("{row}");
                }
            }
        }
    }

    fn csv(&self) -> Result<(), CliError> {
        let mut writer = csv::Writer::from_writer(std::io::stdout().lock());
        match &self.data {
            Data::Item(item) => writer
                .serialize(item.csv_row())
                .map_err(std::io::Error::other)?,
            Data::Items(items) => {
                for item in items {
                    writer
                        .serialize(item.csv_row())
                        .map_err(std::io::Error::other)?;
                }
            }
            Data::Movement(movement) => writer
                .serialize(movement.csv_row())
                .map_err(std::io::Error::other)?,
            Data::Movements(movements) => {
                for movement in movements {
                    writer
                        .serialize(movement.csv_row())
                        .map_err(std::io::Error::other)?;
                }
            }
            Data::Link(link) => writer.serialize(link).map_err(std::io::Error::other)?,
            Data::Valuation(report) => report.csv(&mut writer).map_err(std::io::Error::other)?,
        }
        writer.flush()?;
        Ok(())
    }
}
