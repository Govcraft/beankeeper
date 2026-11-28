use super::math::disposal_gain;
use super::{
    Asset, Movement, MovementKind, accumulated, check_acquisition, has_reversal, movements,
    register_view::read_assets,
};
use crate::{
    db::{EntryRow, companies::Company, connection::ReadConn, holds, transactions},
    error::CliError,
};
use beankeeper::types::{AccountCode, DebitOrCredit};
use chrono::NaiveDate;
use rusqlite::params;
use std::collections::BTreeMap;
fn check_record(
    company: &Company<'_>,
    entity: &str,
    id: &str,
    expected: &serde_json::Value,
) -> Result<(), CliError> {
    let view = company.read().bookkeeping_audit();
    let mut stmt = company.read().prepare(&format!(
        "SELECT after FROM {view} WHERE company_slug=?1 AND entity=?2 AND entity_id=?3 AND action='create'"
    ))?;
    let mut rows = stmt.query(params![company.slug(), entity, id])?;
    let matches = rows
        .next()?
        .map(|row| row.get::<_, String>(0))
        .transpose()?
        .map(|raw| {
            serde_json::from_str::<serde_json::Value>(&raw)
                .map_err(|error| CliError::Database(format!("fixed asset audit data: {error}")))
        })
        .transpose()?
        .as_ref()
        == Some(expected);
    if !matches || rows.next()?.is_some() {
        return Err(CliError::Database(format!(
            "fixed asset audit evidence does not match {entity} {id}"
        )));
    }
    Ok(())
}

fn check_movement(
    company: &Company<'_>,
    asset: &Asset,
    history: &[Movement],
) -> Result<(), CliError> {
    let expected: BTreeMap<_, _> = asset
        .terms
        .schedule()?
        .into_iter()
        .map(|charge| (charge.date, charge.amount))
        .collect();
    let disposal = history
        .iter()
        .find(|item| item.kind == MovementKind::Disposal);
    for movement in history {
        let id = movement.transaction_id;
        check_record(
            company,
            "fixed_asset_movement",
            &id.to_string(),
            &serde_json::json!(movement),
        )?;
        let (journal, entries) = transactions::get_transaction(company, id)?;
        if journal.date != movement.date
            || journal.currency != asset.terms.currency
            || journal.status() != holds::EntryStatus::Posted
            || has_reversal(company, id)?
        {
            return Err(CliError::Database(format!(
                "fixed asset journal #{id} has invalid date, currency, or state"
            )));
        }
        if movement.kind == MovementKind::Depreciation {
            verify_charge(asset, movement, &expected, disposal, &entries)?;
        } else {
            verify_disposal(asset, movement, history, &entries)?;
        }
    }
    Ok(())
}

fn verify_charge(
    asset: &Asset,
    movement: &Movement,
    expected: &BTreeMap<NaiveDate, i64>,
    disposal: Option<&Movement>,
    entries: &[EntryRow],
) -> Result<(), CliError> {
    let accounts = &asset.terms.accounts;
    let amount = movement.amount;
    let valid = expected.get(&movement.date) == Some(&amount)
        && disposal.is_none_or(|value| movement.date < value.date)
        && entries.len() == 2
        && has_line(entries, &accounts.expense, DebitOrCredit::Debit, amount)
        && has_line(
            entries,
            &accounts.accumulated,
            DebitOrCredit::Credit,
            amount,
        );
    if !valid {
        return Err(CliError::Database(format!(
            "invalid fixed asset charge #{}",
            movement.transaction_id
        )));
    }
    Ok(())
}

fn has_line(
    entries: &[EntryRow],
    account: &AccountCode,
    direction: DebitOrCredit,
    amount: i64,
) -> bool {
    if amount == 0 {
        entries
            .iter()
            .all(|entry| entry.account_code != account.as_str())
    } else {
        entries.iter().any(|entry| {
            entry.account_code == account.as_str()
                && entry.direction == direction
                && entry.amount.minor_units() == amount
        })
    }
}

fn verify_disposal(
    asset: &Asset,
    movement: &Movement,
    history: &[Movement],
    entries: &[EntryRow],
) -> Result<(), CliError> {
    let terms = &asset.terms;
    let depreciation = accumulated(asset, history, movement.date)?;
    let gain = disposal_gain(terms.cost.minor_units(), depreciation, movement.amount)?;
    let arguments = (
        terms.cost.minor_units(),
        depreciation,
        movement.amount,
        gain,
    );
    let expected_len = 1
        + usize::from(arguments.1 > 0)
        + usize::from(arguments.2 > 0)
        + usize::from(arguments.3 != 0);
    if movement.date < terms.in_service
        || entries.len() != expected_len
        || !has_line(
            entries,
            &terms.accounts.cost,
            DebitOrCredit::Credit,
            arguments.0,
        )
        || !has_line(
            entries,
            &terms.accounts.accumulated,
            DebitOrCredit::Debit,
            arguments.1,
        )
    {
        return Err(CliError::Database(format!(
            "invalid fixed asset disposal #{}",
            movement.transaction_id
        )));
    }
    Ok(())
}

pub(in crate::db) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let view = reader.bookkeeping_audit();
    for (table, entity) in [
        ("fixed_assets", "fixed_asset"),
        ("fixed_asset_movements", "fixed_asset_movement"),
    ] {
        let counts: (i64,i64) = reader.query_row(
            &format!("SELECT (SELECT COUNT(*) FROM {table}),(SELECT COUNT(*) FROM {view} WHERE entity=?1)"),
            [entity], |row| Ok((row.get(0)?,row.get(1)?)),
        )?;
        if counts.0 != counts.1 {
            return Err(CliError::Database(format!(
                "unmatched {entity} records and audit events"
            )));
        }
    }
    let mut statement = reader.prepare("SELECT DISTINCT company_slug FROM fixed_assets")?;
    for slug in statement.query_map([], |row| row.get(0))? {
        let company = Company::bind_retained(reader, slug?)?;
        for asset in read_assets(&company, None)? {
            check_record(
                &company,
                "fixed_asset",
                &asset.id.to_string(),
                &serde_json::json!(asset),
            )?;
            check_acquisition(&company, &asset.terms, asset.acquisition_id).map_err(|error| {
                CliError::Database(format!("invalid fixed asset acquisition: {error}"))
            })?;
            check_movement(&company, &asset, &movements(&company, &asset.id)?)?;
        }
    }
    Ok(())
}
