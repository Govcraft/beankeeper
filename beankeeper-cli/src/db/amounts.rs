//! Typed minor-unit values at the SQLite and journal boundaries.

use std::{fmt, num::NonZeroI64};

use beankeeper::types::{Amount, EntryError};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::CliError;

/// A strictly positive amount that fits in the ledger's SQLite integer column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PositiveMinorUnits(NonZeroI64);

impl PositiveMinorUnits {
    /// The original SQLite integer, in minor units of the surrounding currency.
    #[must_use]
    pub const fn minor_units(self) -> i64 {
        self.0.get()
    }
}

impl std::str::FromStr for PositiveMinorUnits {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|error| CliError::Validation(format!("invalid minor units: {error}")))?
            .try_into()
    }
}

impl TryFrom<i64> for PositiveMinorUnits {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        NonZeroI64::new(value)
            .filter(|amount| amount.is_positive())
            .map(Self)
            .ok_or_else(|| {
                let error = if value == 0 {
                    EntryError::ZeroAmount
                } else {
                    EntryError::NegativeAmount {
                        amount: Amount::from(value),
                    }
                };
                CliError::Validation(error.to_string())
            })
    }
}

impl TryFrom<Amount> for PositiveMinorUnits {
    type Error = CliError;

    fn try_from(value: Amount) -> Result<Self, Self::Error> {
        i64::try_from(value.minor_units())
            .map_err(|_| {
                CliError::Validation("entry amount exceeds the stored minor-unit range".into())
            })?
            .try_into()
    }
}

impl From<PositiveMinorUnits> for Amount {
    fn from(value: PositiveMinorUnits) -> Self {
        Self::from(value.minor_units())
    }
}

impl From<PositiveMinorUnits> for i128 {
    fn from(value: PositiveMinorUnits) -> Self {
        Self::from(value.minor_units())
    }
}

impl fmt::Display for PositiveMinorUnits {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.minor_units().fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for PositiveMinorUnits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        i64::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl FromSql for PositiveMinorUnits {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Self::try_from(value.as_i64()?).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for PositiveMinorUnits {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.minor_units()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_and_wire_reject_invalid_entry_amounts() -> Result<(), Box<dyn std::error::Error>> {
        for invalid in [0, -1, i64::MIN] {
            assert!(PositiveMinorUnits::try_from(invalid).is_err());
            assert!(PositiveMinorUnits::column_result(ValueRef::Integer(invalid)).is_err());
            assert!(serde_json::from_str::<PositiveMinorUnits>(&invalid.to_string()).is_err());
        }
        let max = PositiveMinorUnits::try_from(i64::MAX)?;
        assert_eq!(serde_json::to_string(&max)?, i64::MAX.to_string());
        assert_eq!(Amount::from(max), Amount::new(i128::from(i64::MAX)));
        assert!(PositiveMinorUnits::try_from(Amount::new(i128::from(i64::MAX) + 1)).is_err());
        Ok(())
    }
}
