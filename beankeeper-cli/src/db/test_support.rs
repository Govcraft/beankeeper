//! Ledger fixtures for unit tests outside the persistence module.

use rusqlite::params;

use super::{Actor, CreateAccountParams, Db};
use crate::error::CliError;
use crate::identity::{Admin, Granted};

#[track_caller]
pub(crate) fn fixture_actor(label: &str) -> Actor {
    Actor::new(label.parse().unwrap_or_else(|error| panic!("{error}")))
}

pub(crate) fn create_company(db: &Db, slug: &str, name: &str) -> Result<(), CliError> {
    Granted::<Admin>::with_db(db, &Actor::new("fixture".parse()?), |grant| {
        super::create_company(grant, &slug.parse()?, &name.parse()?, None)
    })?;
    Ok(())
}

pub(crate) fn create_account(db: &Db, params: &CreateAccountParams<'_>) -> Result<(), CliError> {
    Granted::<Admin>::with_db(db, &Actor::new("fixture".parse()?), |grant| {
        super::create_account(grant, params)
    })?;
    Ok(())
}

pub(crate) fn insert_legacy_transaction(
    db: &Db,
    company: &str,
    currency: &str,
) -> Result<(), CliError> {
    db.conn().execute(
        "INSERT INTO transactions (company_slug, description, currency, date)
         VALUES (?1, 'Legacy', ?2, '2024-01-01')",
        params![company, currency],
    )?;
    Ok(())
}
