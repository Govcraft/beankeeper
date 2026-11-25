//! A persistent identity for one creation of a transaction link.

use std::{fmt, str::FromStr};

use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use serde::Serialize;

use crate::error::CliError;

/// The globally unique `TypeID` of a transaction link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnLinkId(MagicTypeId);

impl TxnLinkId {
    pub(super) fn generate() -> Self {
        Self("txnlink".create_type_id::<V7>())
    }
}

impl fmt::Display for TxnLinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for TxnLinkId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value).map_err(|error| {
            CliError::Validation(format!("invalid transaction link ID: {error}"))
        })?;
        if id.prefix_str().as_deref() != Ok("txnlink") {
            return Err(CliError::Validation(
                "transaction link ID must have prefix 'txnlink'".into(),
            ));
        }
        Ok(Self(id))
    }
}

impl Serialize for TxnLinkId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl rusqlite::types::FromSql for TxnLinkId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}
