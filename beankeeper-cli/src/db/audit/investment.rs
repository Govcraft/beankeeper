//! Investment records appended inside the authorized trade or pricing savepoint.

use chrono::NaiveDate;
use rusqlite::params;

use super::{Actor, Creation, record_insert, savepoint::AuditedTx};
use crate::db::investments::{
    Lot, Price, Relief, ReliefId, Security, SecurityParams, StoredAtoms, get_security, minor,
};
use crate::error::CliError;

pub(in crate::db) fn store_security(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    params: &SecurityParams,
) -> Result<Security, CliError> {
    let conn = tx.connection();
    conn.execute(
        "INSERT INTO securities(company_slug,symbol,name,currency) VALUES (?1,?2,?3,?4)",
        params![company, params.symbol, params.name, params.currency.code()],
    )?;
    let security = get_security(
        &crate::db::companies::Company::bind(tx.read(), company.clone())?,
        &params.symbol,
    )?;
    record_insert(
        conn,
        actor,
        company,
        Creation::Security,
        &params.symbol,
        &serde_json::json!(&security),
    )?;
    Ok(security)
}

pub(in crate::db) fn store_price(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    price: &Price,
    minor_units: i64,
) -> Result<(), CliError> {
    let conn = tx.connection();
    conn.execute(
        "INSERT INTO security_prices(id,company_slug,symbol,date,price) VALUES (?1,?2,?3,?4,?5)",
        params![price.id, company, price.symbol, price.date, minor_units],
    )?;
    record_insert(
        conn,
        actor,
        company,
        Creation::Price,
        price.id.as_str(),
        &serde_json::json!(price),
    )
}

pub(in crate::db) fn store_lot(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    lot: &Lot,
    minor_units: i64,
) -> Result<(), CliError> {
    let conn = tx.connection();
    conn.execute(
        "INSERT INTO investment_lots(id,company_slug,symbol,asset,quantity,cost_basis,acquired_date,transaction_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![lot.id, company, lot.symbol, lot.asset, StoredAtoms(lot.quantity), minor_units,
            lot.acquired_date, lot.transaction_id],
    )?;
    record_insert(
        conn,
        actor,
        company,
        Creation::Lot,
        lot.id.as_str(),
        &serde_json::json!(lot),
    )
}

pub(in crate::db) fn store_reliefs(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    date: NaiveDate,
    transaction_id: crate::db::ids::TransactionId,
    relief: &[Relief],
) -> Result<(), CliError> {
    let conn = tx.connection();
    for allocation in relief {
        let id = ReliefId::new();
        conn.execute(
            "INSERT INTO lot_reliefs(id,company_slug,lot_id,quantity,cost_basis,date,transaction_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![id, company, allocation.lot_id, StoredAtoms(allocation.quantity),
                minor(allocation.cost_basis)?, date, transaction_id],
        )?;
        record_insert(
            conn,
            actor,
            company,
            Creation::Lot,
            id.as_str(),
            &serde_json::json!({"relief": allocation, "date": date, "transaction_id": transaction_id}),
        )?;
    }
    Ok(())
}
