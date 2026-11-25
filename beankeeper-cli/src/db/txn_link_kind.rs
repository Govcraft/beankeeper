//! The kinds of relationship users can record between transactions.

use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// The purpose of a same-book transaction link, independent of the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxnLinkKind {
    /// Records the other leg of a transfer.
    Transfer,
    /// Links a refund to its original charge.
    Refund,
    /// Links a payment to its invoice.
    Payment,
    /// Links an adjustment to the original transaction.
    Adjustment,
    /// Links transactions associated with a change of category.
    Recategorization,
    /// Records a general relationship.
    Related,
}

impl TxnLinkKind {
    const ALL: [Self; 6] = [
        Self::Transfer,
        Self::Refund,
        Self::Payment,
        Self::Adjustment,
        Self::Recategorization,
        Self::Related,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Transfer => "transfer",
            Self::Refund => "refund",
            Self::Payment => "payment",
            Self::Adjustment => "adjustment",
            Self::Recategorization => "recategorization",
            Self::Related => "related",
        }
    }
}

impl fmt::Display for TxnLinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TxnLinkKind {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or_else(|| {
            CliError::Validation(
                "link kind must be transfer, refund, payment, adjustment, recategorization, or related"
                    .into(),
            )
        })
    }
}

impl JsonSchema for TxnLinkKind {
    fn schema_name() -> Cow<'static, str> {
        "TxnLinkKind".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "enum": Self::ALL.map(Self::as_str)})
    }
}

impl rusqlite::types::FromSql for TxnLinkKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}
