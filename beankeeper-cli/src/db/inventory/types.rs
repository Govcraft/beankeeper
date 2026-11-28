//! Exact quantity, movement proof, and FIFO basis calculations.

use std::{collections::BTreeSet, fmt, str::FromStr};

use beankeeper::{
    core::investments::{PositiveQuantity, allocate_cost},
    types::{AccountCode, AccountName, Currency, Money, NonNegativeMoney, Sha256Hex},
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{InventoryItemId, InventoryLotId, StockMovementId};
use crate::{
    db::{amounts::PositiveMinorUnits, counterparties::CounterpartyId, ids::TransactionId},
    error::CliError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockError {
    Quantity,
    InsufficientQuantity,
    Amount,
    Sku,
    Unit,
    Backdated,
    Document,
    Id,
}

impl fmt::Display for StockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Quantity => "stock quantity must be positive with at most six decimal places",
            Self::InsufficientQuantity => "insufficient remaining inventory quantity",
            Self::Amount => "inventory amount is outside the supported range",
            Self::Sku => "SKU must contain 1 to 64 ASCII letters, digits, periods, underscores, or hyphens",
            Self::Unit => "stock unit must contain 1 to 24 printable characters",
            Self::Backdated => "a stock movement cannot predate this item's latest movement",
            Self::Document => "document line does not match the stock movement and has insufficient unassigned quantity",
            Self::Id => "expected a TypeID with the required inventory prefix",
        })
    }
}

impl std::error::Error for StockError {}

impl From<StockError> for CliError {
    fn from(value: StockError) -> Self {
        Self::Validation(value.to_string())
    }
}

macro_rules! text {
    ($name:ident, $error:ident, $body:expr) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(Box<str>);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = StockError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                ($body)(raw.trim())
                    .map(|value| Self(value.into()))
                    .ok_or(StockError::$error)
            }
        }

        impl TryFrom<String> for $name {
            type Error = StockError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.into()
            }
        }
    };
}

text!(Sku, Sku, |value: &str| {
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    .then(|| value.to_ascii_uppercase())
});
text!(StockUnit, Unit, |value: &str| (!value.is_empty()
    && value.len() <= 24
    && !value.chars().any(char::is_control))
.then(|| value.to_owned()));

/// Positive quantity expressed in millionths of a stock unit internally.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct StockQuantity(i64);

impl StockQuantity {
    pub(in crate::db) const fn atoms(self) -> i64 {
        self.0
    }

    pub(in crate::db) fn investment_quantity(self) -> Result<PositiveQuantity, StockError> {
        PositiveQuantity::from_atoms(i128::from(self.0) * 100).map_err(|_| StockError::Quantity)
    }
}

impl fmt::Display for StockQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Decimal::new(self.0, 6).normalize().fmt(f)
    }
}

impl FromStr for StockQuantity {
    type Err = StockError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = Decimal::from_str_exact(raw).map_err(|_| StockError::Quantity)?;
        if value.scale() > 6 {
            return Err(StockError::Quantity);
        }
        let scaled = value
            .mantissa()
            .checked_mul(10_i128.pow(6 - value.scale()))
            .ok_or(StockError::Quantity)?;
        i64::try_from(scaled)
            .map_err(|_| StockError::Quantity)?
            .try_into()
    }
}

impl TryFrom<i64> for StockQuantity {
    type Error = StockError;

    fn try_from(atoms: i64) -> Result<Self, Self::Error> {
        if atoms <= 0 {
            return Err(StockError::Quantity);
        }
        Ok(Self(atoms))
    }
}

impl TryFrom<String> for StockQuantity {
    type Error = StockError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<StockQuantity> for String {
    fn from(value: StockQuantity) -> Self {
        value.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemTerms {
    pub sku: Sku,
    #[schemars(with = "String")]
    pub name: AccountName,
    pub unit: StockUnit,
    #[schemars(with = "String")]
    pub currency: Currency,
    #[schemars(with = "String")]
    pub stock_account: AccountCode,
    #[schemars(with = "String")]
    pub cogs_account: AccountCode,
    #[schemars(with = "String")]
    pub receiving_clearing: AccountCode,
    #[schemars(with = "String")]
    pub adjustment_account: AccountCode,
}

impl ItemTerms {
    pub(in crate::db) fn accounts_are_distinct(&self) -> bool {
        BTreeSet::from([
            &self.stock_account,
            &self.cogs_account,
            &self.receiving_clearing,
            &self.adjustment_account,
        ])
        .len()
            == 4
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Item {
    pub(in crate::db) id: InventoryItemId,
    pub(in crate::db) terms: ItemTerms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MovementKind {
    Receipt,
    Issue,
    CustomerReturn,
    SupplierReturn,
    CountIn,
    CountOut,
}

impl MovementKind {
    pub(in crate::db) const fn code(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Issue => "issue",
            Self::CustomerReturn => "customer_return",
            Self::SupplierReturn => "supplier_return",
            Self::CountIn => "count_in",
            Self::CountOut => "count_out",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StockPartition {
    pub(in crate::db) lot_id: InventoryLotId,
    pub(in crate::db) quantity: StockQuantity,
    pub(in crate::db) cost_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Movement {
    pub(in crate::db) id: StockMovementId,
    pub(in crate::db) item: InventoryItemId,
    pub(in crate::db) kind: MovementKind,
    pub(in crate::db) on: NaiveDate,
    pub(in crate::db) quantity: StockQuantity,
    pub(in crate::db) cost_minor: i64,
    pub(in crate::db) posting_id: Option<TransactionId>,
    pub(in crate::db) party: Option<CounterpartyId>,
    pub(in crate::db) source: Option<StockMovementId>,
    pub(in crate::db) reason: Option<String>,
    pub(in crate::db) reference: String,
    pub(in crate::db) intent_digest: Sha256Hex,
    pub(in crate::db) new_lot: Option<InventoryLotId>,
    pub(in crate::db) partitions: Vec<StockPartition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MovementInput {
    Receipt {
        vendor: CounterpartyId,
        #[schemars(with = "i64", range(min = 1))]
        total_minor: PositiveMinorUnits,
    },
    Issue {
        customer: Option<CounterpartyId>,
    },
    CustomerReturn {
        shipment: StockMovementId,
    },
    SupplierReturn {
        receipt: StockMovementId,
    },
    CountIn {
        #[schemars(range(min = 0))]
        total_minor: i64,
        reason: String,
    },
    CountOut {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u16", into = "u16")]
pub struct DocumentLineNumber(u16);

impl DocumentLineNumber {
    pub(in crate::db) const fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for DocumentLineNumber {
    type Error = StockError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if !(1..=1000).contains(&value) {
            return Err(StockError::Document);
        }
        Ok(Self(value))
    }
}

impl From<DocumentLineNumber> for u16 {
    fn from(value: DocumentLineNumber) -> Self {
        value.0
    }
}

impl FromStr for DocumentLineNumber {
    type Err = StockError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .map_err(|_| StockError::Document)?
            .try_into()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StockPosition {
    pub(in crate::db) lot_id: InventoryLotId,
    pub(in crate::db) quantity: StockQuantity,
    pub(in crate::db) cost_minor: i64,
}

pub(in crate::db) fn plan_fifo(
    requested: StockQuantity,
    ordered: impl IntoIterator<Item = StockPosition>,
    currency: Currency,
) -> Result<Vec<StockPartition>, StockError> {
    let mut needed = requested.atoms();
    let mut allocated = Vec::new();
    for lot in ordered {
        if needed == 0 {
            break;
        }
        let quantity = StockQuantity::try_from(needed.min(lot.quantity.atoms()))?;
        let cost = NonNegativeMoney::new(Money::from_minor(i128::from(lot.cost_minor), currency))
            .map_err(|_| StockError::Amount)?;
        let relief = allocate_cost(
            lot.quantity.investment_quantity()?,
            cost,
            quantity.investment_quantity()?,
        )
        .map_err(|_| StockError::Amount)?;
        allocated.push(StockPartition {
            lot_id: lot.lot_id,
            quantity,
            cost_minor: i64::try_from(relief.money().amount().minor_units())
                .map_err(|_| StockError::Amount)?,
        });
        needed -= quantity.atoms();
    }
    if needed != 0 {
        return Err(StockError::InsufficientQuantity);
    }
    Ok(allocated)
}

pub(in crate::db) fn total_basis(partitions: &[StockPartition]) -> Result<i64, StockError> {
    partitions.iter().try_fold(0_i64, |sum, entry| {
        sum.checked_add(entry.cost_minor).ok_or(StockError::Amount)
    })
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}  {}  {}  {}  {}",
            self.id,
            self.terms.sku,
            self.terms.name,
            self.terms.unit,
            self.terms.currency.code()
        )
    }
}

impl fmt::Display for Movement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}  {}  {}  {}  {} minor units",
            self.id,
            self.item,
            self.on,
            self.kind.code(),
            self.cost_minor
        )?;
        if let Some(journal) = self.posting_id {
            write!(f, "  transaction #{journal}")?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub(crate) struct ItemCsv<'a> {
    id: &'a InventoryItemId,
    sku: &'a Sku,
    name: &'a str,
    unit: &'a StockUnit,
    currency: Currency,
    stock_account: &'a AccountCode,
    cogs_account: &'a AccountCode,
    receiving_clearing: &'a AccountCode,
    adjustment_account: &'a AccountCode,
}

impl Item {
    pub(crate) fn csv_row(&self) -> ItemCsv<'_> {
        let t = &self.terms;
        ItemCsv {
            id: &self.id,
            sku: &t.sku,
            name: t.name.as_ref(),
            unit: &t.unit,
            currency: t.currency,
            stock_account: &t.stock_account,
            cogs_account: &t.cogs_account,
            receiving_clearing: &t.receiving_clearing,
            adjustment_account: &t.adjustment_account,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct MovementCsv<'a> {
    id: &'a StockMovementId,
    item: &'a InventoryItemId,
    kind: &'static str,
    date: NaiveDate,
    quantity: StockQuantity,
    cost_minor: i64,
    posting_id: Option<TransactionId>,
    reference: &'a str,
}

impl Movement {
    pub(crate) fn csv_row(&self) -> MovementCsv<'_> {
        MovementCsv {
            id: &self.id,
            item: &self.item,
            kind: self.kind.code(),
            date: self.on,
            quantity: self.quantity,
            cost_minor: self.cost_minor,
            posting_id: self.posting_id,
            reference: &self.reference,
        }
    }
}
