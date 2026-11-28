use super::asset_from_row;
use super::{Asset, AssetView, Depreciation, MovementKind, movements};
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
    rollforward: Option<Rollforward>,
}

#[derive(Debug, Serialize)]
struct Rollforward {
    currency: Currency,
    #[serde(flatten)]
    additions: MoneyField,
    #[serde(flatten)]
    opening_depreciation: MoneyField,
    #[serde(flatten)]
    depreciation: MoneyField,
    #[serde(flatten)]
    disposed_book_value: MoneyField,
    #[serde(flatten)]
    ending_book_value: MoneyField,
}

impl Rollforward {
    fn from_assets(assets: &[AssetView]) -> Option<Self> {
        let currency = assets.first()?.asset.terms.currency;
        let amounts = assets.iter().fold([0_i128; 5], |mut sum, asset| {
            let terms = &asset.asset.terms;
            let cost = i128::from(terms.cost.minor_units());
            let opening = i128::from(terms.opening_depreciation_minor);
            sum[0] += cost;
            sum[1] += opening;
            sum[2] += asset
                .movements
                .iter()
                .filter(|m| m.kind == MovementKind::Depreciation)
                .map(|m| i128::from(m.amount))
                .sum::<i128>();
            if asset.disposal_gain_loss.is_some() {
                sum[3] += cost - asset.accumulated_depreciation.minor_units();
            }
            sum[4] += asset.book_value.minor_units();
            sum
        });
        let amount = |key, value| MoneyField::from_minor(key, value, currency);
        Some(Self {
            currency,
            additions: amount("additions_minor", amounts[0]),
            opening_depreciation: amount("opening_depreciation_minor", amounts[1]),
            depreciation: amount("depreciation_minor", amounts[2]),
            disposed_book_value: amount("disposed_book_value_minor", amounts[3]),
            ending_book_value: amount("ending_book_value_minor", amounts[4]),
        })
    }
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

#[derive(Serialize)]
pub(crate) struct AssetCsvRow<'a> {
    id: String,
    name: &'a str,
    as_of: NaiveDate,
    currency: Currency,
    cost_minor: i64,
    accumulated_depreciation_minor: i128,
    book_value_minor: i128,
    disposal_gain_loss_minor: Option<i128>,
}

impl AssetView {
    pub(crate) fn csv_row(&self) -> AssetCsvRow<'_> {
        AssetCsvRow {
            id: self.asset.id.to_string(),
            name: self.asset.terms.name.as_ref(),
            as_of: self.as_of,
            currency: self.asset.terms.currency,
            cost_minor: self.asset.terms.cost.minor_units(),
            accumulated_depreciation_minor: self.accumulated_depreciation.minor_units(),
            book_value_minor: self.book_value.minor_units(),
            disposal_gain_loss_minor: self.disposal_gain_loss.map(MoneyField::minor_units),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct DepreciationCsvRow {
    date: NaiveDate,
    currency: Currency,
    amount_minor: i128,
    transaction_id: Option<crate::db::ids::TransactionId>,
}

impl Depreciation {
    pub(crate) const fn csv_row(&self) -> DepreciationCsvRow {
        DepreciationCsvRow {
            date: self.date,
            currency: self.amount.money().currency(),
            amount_minor: self.amount.minor_units(),
            transaction_id: self.transaction_id,
        }
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

pub(super) fn read_assets(
    company: &Company<'_>,
    as_of: Option<NaiveDate>,
) -> Result<Vec<Asset>, CliError> {
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
        rollforward: Rollforward::from_assets(&assets),
        as_of,
        assets,
        control_accounts,
    })
}
