use super::asset_from_row;
use super::{Asset, AssetView, Depreciation, movements};
use crate::{db::companies::Company, error::CliError, output::amounts::MoneyField};
use beankeeper::types::{AccountCode, Currency};
use chrono::NaiveDate;
use rusqlite::params;
use serde::Serialize;
use std::{collections::BTreeMap, fmt};
#[derive(Debug, Serialize)]
pub(crate) struct RegisterView {
    as_of: NaiveDate,
    assets: Vec<AssetView>,
    control_accounts: Vec<ControlAccount>,
}

#[derive(Debug, Serialize)]
struct ControlAccount {
    code: AccountCode,
    currency: Currency,
    #[serde(flatten)]
    register: MoneyField,
    #[serde(flatten)]
    journal: MoneyField,
    #[serde(flatten)]
    difference: MoneyField,
}

impl RegisterView {
    pub(crate) fn rows(&self) -> &[AssetView] {
        &self.assets
    }
}

impl fmt::Display for RegisterView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for asset in &self.assets {
            writeln!(f, "{asset}")?;
        }
        for control in &self.control_accounts {
            writeln!(
                f,
                "{} {}  register {}  journal {}  difference {}",
                control.code,
                control.currency.code(),
                control.register.decimal(),
                control.journal.decimal(),
                control.difference.decimal()
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for AssetView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}  {}  {}  book value {}  accumulated depreciation {}",
            self.asset.id,
            self.asset.terms.name,
            self.asset.terms.currency.code(),
            self.book_value.decimal(),
            self.accumulated_depreciation.decimal()
        )?;
        if let Some(gain) = self.disposal_gain_loss {
            write!(f, "  disposal gain/loss {}", gain.decimal())?;
        }
        Ok(())
    }
}

impl AssetView {
    pub(crate) fn csv_row(&self) -> impl Serialize {
        (
            self.asset.id.to_string(),
            self.asset.terms.name.to_string(),
            self.as_of,
            self.asset.terms.currency,
            self.asset.terms.cost.minor_units(),
            self.accumulated_depreciation.minor_units(),
            self.book_value.minor_units(),
            self.disposal_gain_loss.map(MoneyField::minor_units),
        )
    }
}

impl Depreciation {
    pub(crate) fn csv_row(&self) -> impl Serialize {
        (
            self.date,
            self.amount.money().currency(),
            self.amount.minor_units(),
            self.transaction_id,
        )
    }
}

impl fmt::Display for Depreciation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}  {} {}",
            self.date,
            self.amount.decimal(),
            self.amount.money().currency().code()
        )?;
        if let Some(id) = self.transaction_id {
            write!(f, "  transaction #{id}")?;
        }
        Ok(())
    }
}

pub(super) fn read_assets(company: &Company<'_>, as_of: NaiveDate) -> Result<Vec<Asset>, CliError> {
    let mut query = company.read().prepare(
        "SELECT id,acquisition_id,details FROM fixed_assets WHERE company_slug=?1 AND (?2 IS NULL OR acquired_on<=?2) ORDER BY acquired_on,id"
    )?;
    Ok(query
        .query_map(params![company.slug(), as_of], asset_from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn list(company: &Company<'_>, as_of: NaiveDate) -> Result<RegisterView, CliError> {
    let assets: Vec<_> = read_assets(company, Some(as_of))?
        .into_iter()
        .map(|asset| AssetView::with_history(&asset, &movements(company, &asset.id)?, as_of))
        .collect::<Result<_, _>>()?;
    let mut totals = BTreeMap::new();
    for asset in &assets {
        let terms = &asset.asset.terms;
        let active = asset.disposal_gain_loss.is_none();
        let amount = if active {
            i128::from(terms.cost.minor_units())
        } else {
            0
        };
        let contra = if active {
            -asset.accumulated_depreciation.minor_units()
        } else {
            0
        };
        for (code, value) in [
            (&terms.accounts.cost, amount),
            (&terms.accounts.accumulated, contra),
        ] {
            totals
                .entry((terms.currency.code(), code))
                .or_insert((terms.currency, 0))
                .1 += value;
        }
    }
    let control_accounts = totals
        .into_iter()
        .map(|((_, code), (currency, register))| {
            let (debit, credit) = crate::db::compute_account_balance(
                company,
                code.as_str(),
                None,
                Some(as_of),
                currency,
            )?;
            let journal = i128::from(debit) - i128::from(credit);
            let amount = |key, value| MoneyField::from_minor(key, value, currency);
            Ok(ControlAccount {
                code: code.clone(),
                currency,
                register: amount("register_minor", register),
                journal: amount("journal_minor", journal),
                difference: amount("difference_minor", journal - register),
            })
        })
        .collect::<Result<_, CliError>>()?;
    Ok(RegisterView {
        as_of,
        assets,
        control_accounts,
    })
}
