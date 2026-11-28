//! Inventory mutations under the shared ledger authorizer and write savepoint.

mod links;
mod movement;
mod verify;

pub(crate) use links::{LinkRequest, match_document};
pub(crate) use movement::{MovementRequest, move_stock};
pub(in crate::db) use verify::verify;

use beankeeper::types::{AccountCode, AccountType};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links,
    grant::PostingGrant, savepoint::AuditedTx,
};
use crate::{
    db::{
        accounts::get_active_account,
        companies::{Company, CompanySlug},
        inventory::{InventoryItemId, Item, ItemTerms, decode},
    },
    error::CliError,
    identity::{Granted, Post},
};

fn direct<T>(
    db: &crate::db::Db,
    actor: &Actor,
    run: impl for<'grant> FnOnce(Granted<'grant, Post>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => run(grant),
        PostingGrant::Proposal(_) => Err(CliError::Forbidden(
            "inventory mutations require direct posting authority".into(),
        )),
    })
}

pub(crate) fn register(
    db: &crate::db::Db,
    actor: &Actor,
    slug: &CompanySlug,
    terms: ItemTerms,
) -> Result<Item, CliError> {
    direct(db, actor, |grant| {
        grant.mutate("sp_inventory_register", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let prior = tx
                .read()
                .query_row(
                    "SELECT details FROM inventory_items WHERE company_slug=?1 AND sku=?2",
                    params![slug, terms.sku.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(prior) = prior {
                let item: Item = decode(&prior)?;
                if item.terms == terms {
                    return Ok(item);
                }
                return Err(invalid(
                    "this SKU is already registered with different terms",
                ));
            }
            validate_item(&company, &terms)?;
            let item = Item {
                id: InventoryItemId::generate(),
                terms,
            };
            persist_item(tx, actor, slug, &item)?;
            Ok(item)
        })
    })
}

fn validate_item(company: &Company<'_>, terms: &ItemTerms) -> Result<(), CliError> {
    if !terms.accounts_are_distinct() {
        return Err(invalid(
            "stock, COGS, receiving clearing and adjustment accounts must be distinct",
        ));
    }
    let profile = crate::db::fx::functional_currency(company.read(), company.slug())?;
    let other_currency = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM inventory_items WHERE company_slug=?1 AND currency<>?2)",
        params![company.slug(), terms.currency],
        |row| row.get::<_, bool>(0),
    )?;
    if profile.is_some_and(|currency| currency != terms.currency) || other_currency {
        return Err(invalid(
            "inventory must use the book's one functional currency",
        ));
    }
    for (account, kind) in [
        (&terms.stock_account, AccountType::Asset),
        (&terms.cogs_account, AccountType::Expense),
        (&terms.receiving_clearing, AccountType::Liability),
        (&terms.adjustment_account, AccountType::Expense),
    ] {
        require_account(company, account, kind)?;
    }
    Ok(())
}

fn require_account(
    company: &Company<'_>,
    account: &AccountCode,
    kind: AccountType,
) -> Result<(), CliError> {
    let account_row = get_active_account(company, account.as_str())?;
    let foreign = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM fx_accounts WHERE company_slug=?1 AND account_code=?2)",
        params![company.slug(), account],
        |row| row.get::<_, bool>(0),
    )?;
    if foreign || account_row.account_type != kind {
        return Err(invalid(format!(
            "account {account} must be a functional-currency {kind:?} account"
        )));
    }
    Ok(())
}

fn persist_item(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    item: &Item,
) -> Result<(), CliError> {
    let data = encoded(item)?;
    let receipt = record(
        tx,
        actor,
        slug,
        AuditEntity::InventoryItem,
        &item.id.to_string(),
        &data,
    )?;
    tx.connection().execute(
        "INSERT INTO inventory_items(company_slug,id,sku,stock_account,grni_account,currency,details,receipt)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![slug,item.id,item.terms.sku.as_str(),item.terms.stock_account,item.terms.receiving_clearing,
                item.terms.currency,data,receipt],
    )?;
    Ok(())
}

fn record(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    entity: AuditEntity,
    id: &str,
    details: &str,
) -> Result<crate::db::ids::ReceiptId, CliError> {
    append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(slug),
            entity,
            entity_id: id,
            action: AuditAction::Create,
            snapshots: (None, Some(details)),
        },
        AuditLinks::None,
    )
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}

fn encoded(data: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(data)
        .map_err(|error| CliError::Database(format!("cannot encode inventory data: {error}")))
}
