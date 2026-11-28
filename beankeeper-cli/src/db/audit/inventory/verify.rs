//! Verify inventory journals, FIFO partitions and link receipts against the audit chain.

use beankeeper::types::{AccountCode, DebitOrCredit, Sha256Hex};
use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::{
    db::{
        companies::Company,
        connection::ReadConn,
        inventory::{
            InventoryLotId, Movement, MovementKind, StockPartition, decode, get_item, total_basis,
        },
        transactions::get_transaction,
    },
    error::CliError,
};

pub(in crate::db) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let audit = reader.bookkeeping_audit();
    for (table, entity, id) in [
        ("inventory_items", "inventory_item", "r.id"),
        ("stock_movements", "stock_movement", "r.id"),
        (
            "stock_document_links",
            "stock_document_link",
            "r.movement_id",
        ),
    ] {
        let mismatched=reader.query_row(&format!(
            "SELECT (SELECT COUNT(*) FROM {table})<>(SELECT COUNT(*) FROM {audit} WHERE entity=?1)
             OR EXISTS(SELECT 1 FROM {table} r WHERE NOT EXISTS(
                 SELECT 1 FROM {audit} e WHERE e.id=r.receipt AND e.company_slug=r.company_slug
                   AND e.entity=?1 AND e.action='create' AND e.entity_id={id} AND e.after IS r.details))"),
            [entity],|row|row.get::<_,bool>(0),
        )?;
        if mismatched {
            return Err(stored(format!(
                "inventory audit evidence differs for {entity}"
            )));
        }
    }
    let mut query=reader.prepare("SELECT company_slug,quantity,intent,details FROM stock_movements ORDER BY company_slug,receipt")?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut company: Option<Company<'_>> = None;
    for row in rows {
        let (slug, units, intent, details) = row?;
        if company.as_ref().is_none_or(|book| book.slug() != &slug) {
            company = Some(Company::bind_retained(reader, slug)?);
        }
        let scope = company
            .as_ref()
            .ok_or_else(|| stored("inventory company could not be bound"))?;
        let movement: Movement = decode(&details)?;
        if movement.quantity.atoms() != units
            || movement.intent_digest != Sha256Hex::from_digest(Sha256::digest(intent).into())
        {
            return Err(stored(format!(
                "stock movement {} differs from its intent",
                movement.id
            )));
        }
        check_stock_rows(scope, &movement)?;
        check_posting(scope, &movement)?;
    }
    check_remaining(reader)
}

fn check_stock_rows(company: &Company<'_>, movement: &Movement) -> Result<(), CliError> {
    use rusqlite::OptionalExtension as _;
    let observed=company.read().query_row(
        "SELECT id,quantity,cost_minor FROM stock_lots WHERE company_slug=?1 AND movement_id=?2",
        params![company.slug(),movement.id],
        |row|Ok((row.get::<_,InventoryLotId>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?)),
    ).optional()?;
    let expected = movement
        .new_lot
        .as_ref()
        .map(|lot| (lot, movement.quantity.atoms(), movement.cost_minor));
    if observed
        .as_ref()
        .map(|(id, units, cost)| (id, *units, *cost))
        != expected
    {
        return Err(stored(format!(
            "stock lot does not match movement {}",
            movement.id
        )));
    }
    let mut query=company.read().prepare_cached(
        "SELECT lot_id,quantity,cost_minor,mode FROM stock_partitions WHERE company_slug=?1 AND movement_id=?2 ORDER BY lot_id"
    )?;
    let rows = query.query_map(params![company.slug(), movement.id], |row| {
        Ok((
            row.get::<_, InventoryLotId>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut expected: Vec<&StockPartition> = movement.partitions.iter().collect();
    expected.sort_by_key(|part| &part.lot_id);
    let mode = if movement.kind == MovementKind::CustomerReturn {
        "recover"
    } else {
        "consume"
    };
    let checked = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if checked.len() != expected.len()
        || !checked
            .iter()
            .zip(expected)
            .all(|((id, units, cost, k), p)| {
                id == &p.lot_id
                    && *units == p.quantity.atoms()
                    && *cost == p.cost_minor
                    && k == mode
            })
    {
        return Err(stored(format!(
            "inventory partitions do not match movement {}",
            movement.id
        )));
    }
    if !movement.partitions.is_empty() {
        let units = movement
            .partitions
            .iter()
            .map(|s| i128::from(s.quantity.atoms()))
            .sum::<i128>();
        if units != i128::from(movement.quantity.atoms())
            || total_basis(&movement.partitions)? != movement.cost_minor
        {
            return Err(stored(format!(
                "unbalanced inventory lot relief at {}",
                movement.id
            )));
        }
    }
    Ok(())
}

fn check_posting(company: &Company<'_>, movement: &Movement) -> Result<(), CliError> {
    let Some(id) = movement.posting_id else {
        return Ok(());
    };
    let item = get_item(company, &movement.item)?;
    let terms = &item.terms;
    let (transaction, entries) = get_transaction(company, id)?;
    let (side, offset) = match movement.kind {
        MovementKind::Receipt => (DebitOrCredit::Debit, &terms.receiving_clearing),
        MovementKind::Issue => (DebitOrCredit::Credit, &terms.cogs_account),
        MovementKind::CustomerReturn => (DebitOrCredit::Debit, &terms.cogs_account),
        MovementKind::SupplierReturn => (DebitOrCredit::Credit, &terms.receiving_clearing),
        MovementKind::CountIn => (DebitOrCredit::Debit, &terms.adjustment_account),
        MovementKind::CountOut => (DebitOrCredit::Credit, &terms.adjustment_account),
    };
    let line = |account: &AccountCode, direction| {
        entries.iter().any(|entry| {
            entry.account_code == account.as_str()
                && entry.direction == direction
                && entry.amount.minor_units() == movement.cost_minor
        })
    };
    if transaction.date != movement.on
        || transaction.currency != terms.currency
        || entries.len() != 2
        || !line(&terms.stock_account, side)
        || !line(offset, side.opposite())
    {
        return Err(stored(format!(
            "journal #{id} does not match inventory movement {}",
            movement.id
        )));
    }
    Ok(())
}

fn check_remaining(reader: ReadConn<'_>) -> Result<(), CliError> {
    let invalid=reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM stock_lots lot JOIN stock_partitions p ON p.company_slug=lot.company_slug AND p.lot_id=lot.id AND p.mode='consume'
            GROUP BY lot.company_slug,lot.id HAVING SUM(p.quantity)>lot.quantity OR SUM(p.cost_minor)>lot.cost_minor
               OR ((SUM(p.quantity)=lot.quantity) AND SUM(p.cost_minor)<>lot.cost_minor))",
        [],|row|row.get::<_,bool>(0),
    )?;
    if invalid {
        return Err(stored(
            "FIFO quantity or basis was consumed beyond the original lot",
        ));
    }
    Ok(())
}

fn stored(message: impl Into<String>) -> CliError {
    CliError::Database(message.into())
}
