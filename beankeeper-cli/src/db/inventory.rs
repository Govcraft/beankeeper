//! Perpetual inventory, FIFO lot valuation, and physical-to-financial links.

mod ids;
mod projection;
mod types;
pub(crate) use ids::{InventoryItemId, InventoryLotId, StockMovementId};
pub(in crate::db) use projection::positions;
pub(crate) use projection::{InventoryValuation, valuation};
pub use types::{DocumentLineNumber, ItemTerms, StockError, StockQuantity};
pub(crate) use types::{Item, Movement, MovementInput, MovementKind, StockPartition};
pub(in crate::db) use types::{StockPosition, plan_fifo, total_basis};

use beankeeper::types::AccountCode;
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;

use crate::{
    db::{
        companies::Company,
        documents::{DocumentId, DocumentKind},
        ids::TransactionId,
    },
    error::CliError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DocumentLink {
    pub(crate) movement: StockMovementId,
    pub(crate) document: DocumentId,
    pub(crate) line: DocumentLineNumber,
    pub(crate) kind: DocumentKind,
    pub(crate) on: NaiveDate,
}

pub(crate) fn get_item(company: &Company<'_>, id: &InventoryItemId) -> Result<Item, CliError> {
    let text = company
        .read()
        .query_row(
            "SELECT details FROM inventory_items WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("inventory item {id} not found")))?;
    decode(&text)
}

pub(crate) fn get_movement(
    company: &Company<'_>,
    id: &StockMovementId,
) -> Result<Movement, CliError> {
    let text = company
        .read()
        .query_row(
            "SELECT details FROM stock_movements WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("stock movement {id} not found")))?;
    decode(&text)
}

pub(crate) fn list_items(company: &Company<'_>) -> Result<Vec<Item>, CliError> {
    read_payloads(
        company,
        "SELECT details FROM inventory_items WHERE company_slug=?1 ORDER BY sku LIMIT 100",
        [company.slug()],
    )
}

pub(crate) fn list_movements(
    company: &Company<'_>,
    id: &InventoryItemId,
    as_of: NaiveDate,
) -> Result<Vec<Movement>, CliError> {
    get_item(company, id)?;
    read_payloads(company,
        "SELECT details FROM stock_movements WHERE company_slug=?1 AND item_id=?2 AND movement_date<=?3
         ORDER BY movement_date DESC,receipt DESC LIMIT 100",
        params![company.slug(),id,as_of])
}

pub(in crate::db) fn read_payloads<T: DeserializeOwned>(
    company: &Company<'_>,
    query: &str,
    bindings: impl rusqlite::Params,
) -> Result<Vec<T>, CliError> {
    let mut statement = company.read().prepare(query)?;
    let rows = statement.query_map(bindings, |row| row.get::<_, String>(0))?;
    rows.map(|row| decode(&row?)).collect()
}

pub(in crate::db) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_str(value)
        .map_err(|error| CliError::Database(format!("invalid stock record: {error}")))
}

#[derive(Default)]
pub(in crate::db) struct InventoryAccounts {
    pub(in crate::db) stock: BTreeSet<AccountCode>,
    pub(in crate::db) clearing: BTreeSet<AccountCode>,
}

pub(in crate::db) fn plan_roles(company: &Company<'_>) -> Result<InventoryAccounts, CliError> {
    let mut result = InventoryAccounts::default();
    if company.read().schema_version()? < crate::db::schema::INVENTORY_SCHEMA_VERSION {
        return Ok(result);
    }
    let mut statement = company.read().prepare_cached(
        "SELECT DISTINCT stock_account,grni_account FROM inventory_items WHERE company_slug=?1",
    )?;
    let rows = statement.query_map([company.slug()], |row| Ok((row.get(0)?, row.get(1)?)))?;
    for row in rows {
        let (stock, clearing) = row?;
        result.stock.insert(stock);
        result.clearing.insert(clearing);
    }
    Ok(result)
}

pub(in crate::db) fn guard_profile(
    reader: crate::db::connection::ReadConn<'_>,
    company: &crate::db::companies::CompanySlug,
    currency: Option<beankeeper::types::Currency>,
) -> Result<(), CliError> {
    if let Some(currency) = currency
        && reader.schema_version()? >= crate::db::schema::INVENTORY_SCHEMA_VERSION
        && reader.query_row(
            "SELECT EXISTS(SELECT 1 FROM inventory_items WHERE company_slug=?1 AND currency<>?2)",
            params![company, currency],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(CliError::Validation(
            "inventory requires the existing functional currency".into(),
        ));
    }
    Ok(())
}

pub(in crate::db) fn protect_posting(
    company: &Company<'_>,
    id: TransactionId,
) -> Result<(), CliError> {
    if company.read().schema_version()? >= crate::db::schema::INVENTORY_SCHEMA_VERSION
        && company.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM stock_movements WHERE company_slug=?1 AND transaction_id=?2)",
            params![company.slug(),id],|row| row.get::<_,bool>(0),
        )?
    {
        return Err(CliError::Validation("stock journals require an inventory return or count adjustment".into()));
    }
    Ok(())
}
