//! Bounded staging and review of read-only bank feeds.

mod model;
mod provider;

use std::str::FromStr;

use beankeeper::types::{Currency, Money};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(super) use self::model::SignedMinor;
use self::model::SyncCursor;
pub use self::model::{FeedConnectionId, FeedError, FeedItemId, ProviderAccountKey};
pub use self::provider::PlaidEnvironment;
use crate::db::{
    Actor, Db, companies::CompanySlug, connection::ReadConn, ids::TransactionId,
    proposals::ProposalId,
};
use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemStatus {
    Pending,
    Posted,
    Removed,
}

impl ItemStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Posted => "posted",
            Self::Removed => "removed",
        }
    }
}

impl FromStr for ItemStatus {
    type Err = FeedError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "posted" => Ok(Self::Posted),
            "removed" => Ok(Self::Removed),
            _ => Err(FeedError::InvalidResponse),
        }
    }
}

/// Company-scoped configuration for one externally authorized Plaid account.
#[derive(Debug, Clone, clap::Args)]
pub struct ConnectArgs {
    /// Existing ledger asset or liability account code.
    #[arg(long)]
    pub account: String,
    /// Account identifier obtained via your Plaid Link host.
    #[arg(long)]
    pub provider_account: ProviderAccountKey,
    /// ISO 4217 currency of the connected account.
    #[arg(long)]
    pub currency: Currency,
    /// Plaid endpoint used by its host credentials.
    #[arg(long, value_enum, default_value_t)]
    pub environment: PlaidEnvironment,
}

#[derive(Debug, Serialize)]
pub(crate) struct Connection {
    pub(crate) id: FeedConnectionId,
    pub(crate) account: String,
    #[serde(skip)]
    pub(crate) provider_account: ProviderAccountKey,
    pub(crate) currency: Currency,
    pub(crate) environment: PlaidEnvironment,
    pub(crate) revision: i64,
    pub(crate) connected: bool,
    #[serde(skip)]
    pub(crate) cursor: Option<SyncCursor>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Item {
    pub(crate) id: FeedItemId,
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    #[serde(skip)]
    pub(crate) amount: SignedMinor,
    pub(crate) amount_minor: i64,
    pub(crate) amount_display: String,
    pub(crate) status: ItemStatus,
    pub(crate) revision: i64,
    pub(crate) correction_required: bool,
    pub(crate) transaction_id: Option<TransactionId>,
    pub(crate) proposal_id: Option<ProposalId>,
    proposal_status: Option<String>,
    #[serde(skip)]
    pub(crate) resolved_revision: Option<i64>,
}

#[derive(Debug, Clone, Default, clap::Args, Deserialize, Serialize, JsonSchema)]
pub struct PageArgs {
    /// Last feed item ID from the previous page.
    #[arg(long)]
    pub after: Option<FeedItemId>,
}

const PAGE_SIZE: usize = 50;

fn decode_parse<T>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    row.get::<_, String>(index)?.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

pub(crate) fn connection(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: &FeedConnectionId,
) -> Result<Connection, CliError> {
    reader.connection().query_row(
        "SELECT account_code, provider_account, currency, environment, revision, connected, cursor
         FROM bank_feed_connections WHERE company_slug = ?1 AND id = ?2",
        params![company, id],
        |row| {
            let cursor = row.get::<_, Option<String>>(6)?.map(|s| s.parse()).transpose()
                .map_err(|error| rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error)))?;
            Ok(Connection {
                id: id.clone(),
                account: row.get(0)?,
                provider_account: decode_parse(row, 1)?,
                currency: decode_parse(row, 2)?,
                environment: decode_parse(row, 3)?,
                revision: row.get(4)?,
                connected: row.get(5)?,
                cursor,
            })
        },
    ).optional()?.ok_or_else(|| CliError::NotFound(format!("bank feed {id} not found")))
}

fn decode_item(row: &rusqlite::Row<'_>, currency: Currency) -> rusqlite::Result<Item> {
    let revision = row.get(5)?;
    let resolved_revision: Option<i64> = row.get(8)?;
    let amount_minor = row.get(3)?;
    let amount = SignedMinor::new(amount_minor).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(Item {
        id: row.get(0)?,
        date: row.get(1)?,
        description: row.get(2)?,
        amount,
        amount_minor,
        amount_display: Money::from_minor(i128::from(amount_minor), currency).to_string(),
        status: decode_parse(row, 4)?,
        revision,
        correction_required: resolved_revision.is_some_and(|resolved| resolved != revision),
        transaction_id: row.get(6)?,
        proposal_id: row.get(7)?,
        proposal_status: row.get(9)?,
        resolved_revision,
    })
}

pub(crate) fn item(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    feed: &Connection,
    id: &FeedItemId,
) -> Result<Item, CliError> {
    reader
        .connection()
        .query_row(
            "SELECT i.id, i.date, i.description, i.amount_minor, i.status, i.revision,
        COALESCE(i.transaction_id, p.transaction_id), i.proposal_id, i.resolved_revision, p.status
        FROM bank_feed_items i LEFT JOIN txn_proposals p ON p.id = i.proposal_id
        WHERE i.company_slug = ?1 AND i.connection_id = ?2 AND i.id = ?3",
            params![company, feed.id, id],
            |row| decode_item(row, feed.currency),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("bank feed item {id} not found")))
}

#[derive(Serialize)]
pub(crate) struct ItemPage {
    rows: Vec<Item>,
    next_after: Option<FeedItemId>,
}

pub(crate) fn list(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    feed: &Connection,
    page: &PageArgs,
) -> Result<ItemPage, CliError> {
    let mut statement = reader.connection().prepare_cached(
        "SELECT i.id, i.date, i.description, i.amount_minor, i.status, i.revision,
        COALESCE(i.transaction_id, p.transaction_id), i.proposal_id, i.resolved_revision, p.status
        FROM bank_feed_items i LEFT JOIN txn_proposals p ON p.id = i.proposal_id
        WHERE i.company_slug = ?1 AND i.connection_id = ?2 AND i.id > COALESCE(?3, '')
        ORDER BY i.id LIMIT 51",
    )?;
    let mut rows = statement
        .query_map(params![company, feed.id, page.after], |row| {
            decode_item(row, feed.currency)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let next_after = has_more
        .then(|| rows.last().map(|row| row.id.clone()))
        .flatten();
    Ok(ItemPage { rows, next_after })
}

pub(crate) fn poll(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    id: &FeedConnectionId,
) -> Result<Connection, CliError> {
    super::companies::Company::bind(db.read(), company.clone())?;
    let before = connection(db.read(), company, id)?;
    if !before.connected {
        return Err(FeedError::Disconnected.into());
    }
    let provider =
        provider::PlaidProvider::from_host(before.environment, before.provider_account.clone())?;
    let (cursor, delta) = provider::fetch_complete(&provider, before.cursor.as_ref())?;
    super::audit::bank_feed::commit_sync(db, actor, company, &before, &cursor, &delta)
}

pub(super) use model::FeedDelta as SyncDelta;
pub(super) use model::SyncCursor as Cursor;
pub(crate) use model::{FeedTransaction, ProviderTransactionKey};

/// One explicitly reviewed provider revision.
#[derive(Debug, Clone, clap::Args, Deserialize, Serialize, JsonSchema)]
pub struct ItemSelection {
    /// Bank feed connection ID.
    pub feed: FeedConnectionId,
    /// Staged bank feed item ID.
    pub item: FeedItemId,
    /// Exact item revision to post, propose or link.
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    pub revision: i64,
}

#[derive(Debug, Clone, clap::Args, Deserialize, Serialize, JsonSchema)]
pub struct PostArgs {
    #[command(flatten)]
    #[serde(flatten)]
    pub selection: ItemSelection,
    /// Offset account. If omitted, an active categorization rule must match.
    #[arg(long)]
    pub contra: Option<String>,
    /// Explicitly create a distinct posting even though similar entries exist.
    #[arg(long)]
    #[serde(default)]
    pub confirm_new: bool,
}

#[derive(Debug, Clone, clap::Args, Deserialize, Serialize, JsonSchema)]
pub struct LinkArgs {
    #[command(flatten)]
    #[serde(flatten)]
    pub selection: ItemSelection,
    /// Existing posted transaction shown by review.
    #[arg(long)]
    pub transaction: TransactionId,
}

#[derive(Debug, Serialize)]
pub(crate) struct Candidate {
    pub(crate) transaction: TransactionId,
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) already_linked: bool,
}

#[derive(Serialize)]
pub(crate) struct Review {
    pub(crate) item: Item,
    pub(crate) candidate_transactions: Vec<Candidate>,
    pub(crate) suggested_account: Option<String>,
    pub(crate) additional_candidates: bool,
}

pub(crate) fn direction(amount: SignedMinor) -> beankeeper::types::DebitOrCredit {
    if amount.get() > 0 {
        beankeeper::types::DebitOrCredit::Debit
    } else {
        beankeeper::types::DebitOrCredit::Credit
    }
}

pub(crate) fn review(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    feed: &Connection,
    item: Item,
) -> Result<Review, CliError> {
    let candidates = candidate_transactions(reader, company, feed, &item, None)?;
    let scope = super::companies::Company::bind(reader, company.clone())?;
    let rules = super::rules::list_active_rules(&scope)?;
    Ok(plan_feed_review(feed, item, candidates, &rules))
}

pub(crate) fn candidate_transactions(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    feed: &Connection,
    item: &Item,
    target: Option<TransactionId>,
) -> Result<Vec<Candidate>, CliError> {
    let mut statement = reader.connection().prepare_cached(
        "SELECT t.id, t.date, t.description,
            EXISTS(SELECT 1 FROM bank_feed_items i LEFT JOIN txn_proposals p ON p.id = i.proposal_id
              WHERE i.connection_id = ?2 AND COALESCE(i.transaction_id, p.transaction_id) = t.id)
         FROM entries e JOIN transactions t ON t.id = e.transaction_id
         WHERE e.company_slug = ?1 AND e.account_code = ?3 AND t.currency = ?4
           AND t.posting_status = 'posted'
           AND (?7 IS NULL OR t.id = ?7)
           AND t.date BETWEEN date(?6, '-3 days') AND date(?6, '+3 days')
           AND NOT EXISTS(SELECT 1 FROM posting_reversals r WHERE r.company_slug = ?1
               AND (r.original_id = CAST(t.id AS TEXT) OR r.reversal_id = t.id))
         GROUP BY t.id, t.date, t.description
         HAVING SUM(CASE e.direction WHEN 'debit' THEN e.amount ELSE -e.amount END) = ?5
         ORDER BY t.date DESC, t.id DESC LIMIT 51",
    )?;
    let candidates = statement
        .query_map(
            params![
                company,
                feed.id,
                feed.account,
                feed.currency.code(),
                item.amount.get(),
                item.date,
                target
            ],
            |row| {
                Ok(Candidate {
                    transaction: row.get(0)?,
                    date: row.get(1)?,
                    description: row.get(2)?,
                    already_linked: row.get(3)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(candidates)
}

fn plan_feed_review(
    feed: &Connection,
    item: Item,
    mut candidates: Vec<Candidate>,
    rules: &[super::rules::Rule],
) -> Review {
    let additional_candidates = candidates.len() > PAGE_SIZE;
    candidates.truncate(PAGE_SIZE);
    let suggested_account = super::rules::matching_rule(rules, &item.description)
        .filter(|rule| rule.account_code().as_str() != feed.account)
        .map(|rule| rule.account_code().as_str().into());
    Review {
        item,
        candidate_transactions: candidates,
        suggested_account,
        additional_candidates,
    }
}

pub(super) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let foreign_source: bool = reader.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM bank_feed_items i
            LEFT JOIN transactions t ON t.id = i.transaction_id
            LEFT JOIN txn_proposals p ON p.id = i.proposal_id
            JOIN bank_feed_connections c ON c.id = i.connection_id
            JOIN accounts a ON a.company_slug = c.company_slug AND a.code = c.account_code
            WHERE a.type NOT IN ('asset', 'liability')
               OR (i.transaction_id IS NOT NULL AND (t.company_slug IS NOT i.company_slug
                   OR t.currency IS NOT c.currency OR t.posting_status <> 'posted'))
               OR (i.proposal_id IS NOT NULL AND (p.company_slug IS NOT i.company_slug
                   OR p.currency IS NOT c.currency))
         )",
        [],
        |row| row.get(0),
    )?;
    if foreign_source {
        return Err(CliError::Database(
            "bank feed resolution points outside its ledger scope".into(),
        ));
    }

    let mut statement = reader.prepare("SELECT id, company_slug FROM bank_feed_connections")?;
    let connections = statement.query_map([], |row| {
        Ok((
            row.get::<_, FeedConnectionId>(0)?,
            row.get::<_, CompanySlug>(1)?,
        ))
    })?;
    for entry in connections {
        let (id, company) = entry?;
        connection(reader, &company, &id)?;
    }
    let mut statement =
        reader.prepare("SELECT id, provider_key, pending_key FROM bank_feed_items")?;
    let items = statement.query_map([], |row| {
        let _: FeedItemId = row.get(0)?;
        let _: ProviderTransactionKey = decode_parse(row, 1)?;
        if let Some(key) = row.get::<_, Option<String>>(2)? {
            let _: ProviderTransactionKey = key.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        }
        Ok(())
    })?;
    for entry in items {
        entry?;
    }
    Ok(())
}
