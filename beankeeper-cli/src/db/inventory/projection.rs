//! Dated stock balances, open receipt evidence, and general-ledger controls.

use std::collections::BTreeMap;

use beankeeper::types::{AccountCode, Currency};
use chrono::NaiveDate;
use rusqlite::params;
use rust_decimal::Decimal;
use serde::Serialize;

use super::{
    InventoryItemId, InventoryLotId, Item, Movement, StockError, read_payloads,
    types::StockPosition,
};
use crate::{
    db::{companies::Company, compute_account_balance},
    error::CliError,
    output::amounts::MoneyField,
};

pub(in crate::db) fn positions(
    company: &Company<'_>,
    item: &InventoryItemId,
    through: NaiveDate,
    only: Option<&InventoryLotId>,
) -> Result<Vec<StockPosition>, CliError> {
    let mut query=company.read().prepare_cached(
        "WITH eligible AS (
            SELECT l.id,l.quantity,l.cost_minor,origin.movement_date,origin.receipt
            FROM stock_lots l
            JOIN stock_movements origin ON origin.company_slug=l.company_slug AND origin.id=l.movement_id
            WHERE l.company_slug=?1 AND origin.item_id=?2 AND origin.movement_date<=?3
              AND (?4 IS NULL OR l.id=?4)
        ), remaining AS (
            SELECT lot.id, lot.quantity-COALESCE(SUM(CASE WHEN m.id IS NULL THEN 0 ELSE p.quantity END),0) AS units,
                   lot.cost_minor-COALESCE(SUM(CASE WHEN m.id IS NULL THEN 0 ELSE p.cost_minor END),0) AS basis,
                   lot.movement_date,lot.receipt
            FROM eligible lot
            LEFT JOIN stock_partitions p ON p.company_slug=?1 AND p.lot_id=lot.id AND p.mode='consume'
            LEFT JOIN stock_movements m ON m.company_slug=p.company_slug AND m.id=p.movement_id AND m.movement_date<=?3
            GROUP BY lot.id
        )
        SELECT id,units,basis FROM remaining WHERE units>0 ORDER BY movement_date,receipt",
    )?;
    let rows = query.query_map(params![company.slug(), item, through, only], |row| {
        Ok((row.get(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
    })?;
    rows.map(|row| {
        let (lot_id, atoms, cost_minor) = row?;
        if cost_minor < 0 {
            return Err(CliError::Database(
                "a stock lot has negative remaining basis".into(),
            ));
        }
        Ok(StockPosition {
            lot_id,
            quantity: atoms.try_into()?,
            cost_minor,
        })
    })
    .collect()
}

#[derive(Serialize)]
pub(crate) struct InventoryValuation {
    as_of: NaiveDate,
    positions: Vec<ItemValue>,
    control_accounts: Vec<InventoryControl>,
    unlinked_receipts: Vec<Movement>,
}

#[derive(Serialize)]
struct ItemValue {
    item: Item,
    quantity: String,
    #[serde(flatten)]
    value: MoneyField,
    lots: Vec<StockPosition>,
}

#[derive(Serialize)]
struct InventoryControl {
    account: AccountCode,
    currency: Currency,
    #[serde(flatten)]
    register: MoneyField,
    #[serde(flatten)]
    ledger: MoneyField,
    #[serde(flatten)]
    difference: MoneyField,
}

impl InventoryValuation {
    pub(crate) fn summary(&self) -> impl Iterator<Item = String> + '_ {
        self.positions
            .iter()
            .map(|item| {
                format!(
                    "{}  {} {}  {} {}",
                    item.item.terms.sku,
                    item.quantity,
                    item.item.terms.unit,
                    item.value.decimal(),
                    item.item.terms.currency.code()
                )
            })
            .chain(self.control_accounts.iter().map(|account| {
                format!(
                    "{} {}  register {}  ledger {}  difference {}",
                    account.account,
                    account.currency.code(),
                    account.register.decimal(),
                    account.ledger.decimal(),
                    account.difference.decimal()
                )
            }))
    }
}

pub(crate) fn valuation(
    company: &Company<'_>,
    as_of: NaiveDate,
) -> Result<InventoryValuation, CliError> {
    let items: Vec<Item> = read_payloads(
        company,
        "SELECT details FROM inventory_items WHERE company_slug=?1 ORDER BY sku",
        [company.slug()],
    )?;
    let mut totals = BTreeMap::<(AccountCode, &'static str), (Currency, i128)>::new();
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let lots = positions(company, &item.id, as_of, None)?;
        let quantity = lots.iter().try_fold(0_i64, |sum, lot| {
            sum.checked_add(lot.quantity.atoms())
                .ok_or(StockError::Quantity)
        })?;
        let amount = lots
            .iter()
            .map(|lot| i128::from(lot.cost_minor))
            .sum::<i128>();
        let currency = item.terms.currency;
        totals
            .entry((item.terms.stock_account.clone(), currency.code()))
            .or_insert((currency, 0))
            .1 += amount;
        values.push(ItemValue {
            item,
            quantity: Decimal::new(quantity, 6).normalize().to_string(),
            value: MoneyField::from_minor("value_minor", amount, currency),
            lots,
        });
    }
    let control_accounts = totals
        .into_iter()
        .map(|((account, _), (currency, register))| {
            let (debit, credit) =
                compute_account_balance(company, account.as_str(), None, Some(as_of), currency)?;
            let ledger = i128::from(debit) - i128::from(credit);
            let amount = |key, value| MoneyField::from_minor(key, value, currency);
            Ok(InventoryControl {
                account,
                currency,
                register: amount("register_minor", register),
                ledger: amount("ledger_minor", ledger),
                difference: amount("difference_minor", ledger - register),
            })
        })
        .collect::<Result<_, CliError>>()?;
    let unlinked_receipts=read_payloads(company,
        "SELECT m.details FROM stock_movements m WHERE m.company_slug=?1 AND m.kind='receipt' AND m.movement_date<=?2
          AND NOT EXISTS(SELECT 1 FROM stock_document_links l WHERE l.company_slug=m.company_slug AND l.movement_id=m.id AND l.linked_on<=?2)
          ORDER BY m.movement_date,m.receipt",
        params![company.slug(),as_of])?;
    Ok(InventoryValuation {
        as_of,
        positions: values,
        control_accounts,
        unlinked_receipts,
    })
}

#[derive(Serialize)]
struct ValuationCsv<'a> {
    row_type: &'static str,
    reference: String,
    quantity: Option<&'a str>,
    currency: Option<Currency>,
    value_minor: i128,
    ledger_minor: Option<i128>,
    difference_minor: Option<i128>,
}

impl InventoryValuation {
    pub(crate) fn csv(
        &self,
        writer: &mut csv::Writer<impl std::io::Write>,
    ) -> Result<(), csv::Error> {
        for item in &self.positions {
            writer.serialize(ValuationCsv {
                row_type: "stock_item",
                reference: item.item.terms.sku.to_string(),
                quantity: Some(&item.quantity),
                currency: Some(item.item.terms.currency),
                value_minor: item.value.minor_units(),
                ledger_minor: None,
                difference_minor: None,
            })?;
        }
        for control in &self.control_accounts {
            writer.serialize(ValuationCsv {
                row_type: "stock_control",
                reference: control.account.to_string(),
                quantity: None,
                currency: Some(control.currency),
                value_minor: control.register.minor_units(),
                ledger_minor: Some(control.ledger.minor_units()),
                difference_minor: Some(control.difference.minor_units()),
            })?;
        }
        for receipt in &self.unlinked_receipts {
            let quantity = receipt.quantity.to_string();
            writer.serialize(ValuationCsv {
                row_type: "unlinked_receipt",
                reference: receipt.id.to_string(),
                quantity: Some(&quantity),
                currency: None,
                value_minor: i128::from(receipt.cost_minor),
                ledger_minor: None,
                difference_minor: None,
            })?;
        }
        Ok(())
    }
}
