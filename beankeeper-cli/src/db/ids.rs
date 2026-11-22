//! Numeric ledger identifiers shared by SQLite, the terminal, and MCP.

use std::{borrow::Cow, fmt, num::NonZeroI64, str::FromStr};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::error::CliError;

macro_rules! row_ids {
    ($($(#[$doc:meta])* $name:ident => $label:literal),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroI64);

        impl $name {
            /// Positive numeric identifier stored in SQLite.
            #[must_use]
            pub const fn get(self) -> i64 {
                self.0.get()
            }
        }

        impl TryFrom<i64> for $name {
            type Error = CliError;

            fn try_from(raw: i64) -> Result<Self, Self::Error> {
                match NonZeroI64::new(raw) {
                    Some(id) if raw > 0 => Ok(Self(id)),
                    _ => Err(CliError::Validation(concat!($label, " must be positive").into())),
                }
            }
        }

        impl FromStr for $name {
            type Err = CliError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                raw.parse::<i64>()
                    .map_err(|error| CliError::Validation(format!("invalid {}: {error}", $label)))?
                    .try_into()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_i64(self.get())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::try_from(i64::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "integer", "format": "int64", "minimum": 1})
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                Self::try_from(value.as_i64()?)
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.get()))
            }
        }
    )+};
}

row_ids! {
    /// An immutable posted transaction identifier.
    TransactionId => "transaction ID",
    /// A transaction identifier used as the other side of a correlation.
    PartnerTransactionId => "partner transaction ID",
    /// An entry identifier within a posted transaction.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::ids::{EntryId, TransactionId};
    /// fn wrong_id(entry: EntryId) -> TransactionId { entry }
    /// ```
    EntryId => "entry ID",
    /// An identifier for a clearing applied to a posted entry.
    ClearingId => "clearing ID",
    /// A monthly budget row identifier.
    BudgetId => "budget ID",
    /// An attachment record identifier.
    AttachmentId => "attachment ID",
    /// An audit log row identifier.
    AuditRowId => "audit row ID",
    /// A frozen statement batch awaiting or bearing owner approval.
    StatementBatchId => "statement batch ID",
}

impl From<TransactionId> for PartnerTransactionId {
    fn from(id: TransactionId) -> Self {
        Self(id.0)
    }
}

#[cfg(test)]
pub(crate) fn fixture_id<T: TryFrom<i64, Error = CliError>>(id: i64) -> T {
    id.try_into().expect("positive fixture identifier")
}
