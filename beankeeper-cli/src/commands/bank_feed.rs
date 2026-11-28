//! Shared CLI and MCP requests for staged bank feeds.

use clap::Subcommand;
use serde::Serialize;
use serde_json::Value;

use crate::{
    cli::{Cli, OutputFormat, require_company},
    db::{
        Actor, Db,
        audit::bank_feed as writes,
        bank_feed::{
            self, ConnectArgs, FeedConnectionId, FeedItemId, LinkArgs, PageArgs, PostArgs,
        },
        companies::CompanySlug,
        connection::ReadConn,
    },
    error::CliError,
    operation::Operation,
    output::json,
};

/// Connect and review provider transactions.
#[derive(Debug, Subcommand)]
pub enum BankFeedCommand {
    /// Map an externally authorized Plaid account to the ledger.
    Connect(ConnectArgs),
    /// Disconnect a feed without changing the ledger.
    Disconnect { feed: FeedConnectionId },
    /// Pull Plaid transactions using host environment credentials.
    Sync { feed: FeedConnectionId },
    /// Read connection status.
    Show { feed: FeedConnectionId },
    /// List staged provider transactions.
    List {
        feed: FeedConnectionId,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Review a staged item and its candidates.
    Review {
        feed: FeedConnectionId,
        item: FeedItemId,
    },
    /// Submit the reviewed revision to the posting or approval workflow.
    Post(PostArgs),
    /// Match a reviewed revision with an existing posting.
    Link(LinkArgs),
}

impl BankFeedCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Connect(_) => Operation::BankFeedConnect,
            Self::Disconnect { .. } => Operation::BankFeedDisconnect,
            Self::Sync { .. } => Operation::BankFeedSync,
            Self::Show { .. } => Operation::BankFeedShow,
            Self::List { .. } => Operation::BankFeedList,
            Self::Review { .. } => Operation::BankFeedReview,
            Self::Post(_) => Operation::BankFeedPost,
            Self::Link(_) => Operation::BankFeedLink,
        }
    }

    pub(crate) fn request(&self) -> Option<BankFeedRequest> {
        match self {
            Self::Show { feed } => Some(BankFeedRequest::Show(feed.clone())),
            Self::List { feed, page } => Some(BankFeedRequest::List(feed.clone(), page.clone())),
            Self::Review { feed, item } => {
                Some(BankFeedRequest::Review(feed.clone(), item.clone()))
            }
            Self::Post(args) => Some(BankFeedRequest::Post(args.clone())),
            Self::Link(args) => Some(BankFeedRequest::Link(args.clone())),
            Self::Connect(_) | Self::Disconnect { .. } | Self::Sync { .. } => None,
        }
    }
}

/// A transport-independent bank feed request.
pub enum BankFeedRequest {
    Show(FeedConnectionId),
    List(FeedConnectionId, PageArgs),
    Review(FeedConnectionId, FeedItemId),
    Post(PostArgs),
    Link(LinkArgs),
}

impl BankFeedRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Show(_) => Operation::BankFeedShow,
            Self::List(..) => Operation::BankFeedList,
            Self::Review(..) => Operation::BankFeedReview,
            Self::Post(_) => Operation::BankFeedPost,
            Self::Link(_) => Operation::BankFeedLink,
        }
    }

    pub(crate) fn execute(
        self,
        db: &Db,
        reader: ReadConn<'_>,
        actor: &Actor,
        company: &CompanySlug,
    ) -> Result<BankFeedResponse, CliError> {
        match self {
            Self::Show(id) => encode(bank_feed::connection(reader, company, &id)?),
            Self::List(id, page) => {
                let feed = bank_feed::connection(reader, company, &id)?;
                encode(bank_feed::list(reader, company, &feed, &page)?)
            }
            Self::Review(id, item) => {
                let feed = bank_feed::connection(reader, company, &id)?;
                let item = bank_feed::item(reader, company, &feed, &item)?;
                encode(bank_feed::review(reader, company, &feed, item)?)
            }
            Self::Post(args) => encode(writes::post(db, actor, company, &args)?),
            Self::Link(args) => encode(writes::link(db, actor, company, &args)?),
        }
    }
}

/// Bank-feed data suitable for structured output.
#[derive(Serialize)]
#[serde(transparent)]
pub struct BankFeedResponse(Value);

fn encode(value: impl Serialize) -> Result<BankFeedResponse, CliError> {
    serde_json::to_value(value)
        .map(BankFeedResponse)
        .map_err(|error| CliError::General(format!("cannot serialize bank feed result: {error}")))
}

impl BankFeedResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        if cli.is_json() {
            println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), &self.0)?
            );
        } else {
            let mut table = comfy_table::Table::new();
            table.set_header(["Field", "Value"]);
            if let Some(fields) = self.0.as_object() {
                for (key, value) in fields {
                    let cell = match value {
                        Value::String(text) => text.clone(),
                        other => serde_json::to_string_pretty(other)
                            .map_err(|error| CliError::General(error.to_string()))?,
                    };
                    table.add_row([key, &cell]);
                }
            }
            println!("{table}");
        }
        Ok(())
    }
}

pub(crate) fn check_output(cli: &Cli) -> Result<(), CliError> {
    if !cli.is_json() && cli.output.format == Some(OutputFormat::Csv) {
        return Err(CliError::Usage(
            "bank feed output supports table and JSON formats".into(),
        ));
    }
    Ok(())
}

pub(crate) fn run(cli: &Cli, command: &BankFeedCommand) -> Result<(), CliError> {
    let db = cli.command_db()?;
    let actor = cli.principal();
    let company = require_company(cli)?;
    let result = match command {
        BankFeedCommand::Connect(args) => encode(writes::connect(&db, &actor, company, args)?),
        BankFeedCommand::Disconnect { feed } => {
            encode(writes::disconnect(&db, &actor, company, feed)?)
        }
        BankFeedCommand::Sync { feed } => encode(bank_feed::poll(&db, &actor, company, feed)?),
        _ => Err(CliError::General(
            "bank feed request should use shared dispatch".into(),
        )),
    }?;
    result.render(cli, command.operation(), company)
}
