//! Append fixed asset evidence within an authorized posting scope.

use rusqlite::params;

use super::{Actor, Creation, record_insert, savepoint::AuditedTx};
use crate::{
    db::{
        companies::CompanySlug,
        fixed_assets::{Asset, Movement},
    },
    error::CliError,
};

pub(in crate::db) fn store_asset(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    asset: &Asset,
) -> Result<(), CliError> {
    let details = serde_json::to_string(&asset.terms)
        .map_err(|error| CliError::Database(format!("fixed asset details: {error}")))?;
    let conn = tx.connection();
    conn.execute(
        "INSERT INTO fixed_assets(id,company_slug,acquisition_id,acquired_on,details) VALUES (?1,?2,?3,?4,?5)",
        params![asset.id.to_string(),company,asset.acquisition_id,asset.terms.acquired_on,details],
    )?;
    record_insert(
        conn,
        actor,
        company,
        Creation::FixedAsset,
        &asset.id.to_string(),
        &serde_json::json!(asset),
    )
}

pub(in crate::db) fn store_movement(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    movement: &Movement,
) -> Result<(), CliError> {
    let conn = tx.connection();
    conn.execute(
        "INSERT INTO fixed_asset_movements(transaction_id,company_slug,asset_id,kind,date,amount)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            movement.transaction_id,
            company,
            movement.asset_id.to_string(),
            movement.kind.key(),
            movement.date,
            movement.amount
        ],
    )?;
    record_insert(
        conn,
        actor,
        company,
        Creation::FixedAssetMovement,
        &movement.transaction_id.to_string(),
        &serde_json::json!(movement),
    )
}
