use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use serde::{Serialize, Serializer};

/// The canonical payload format used to hash an audit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashVersion {
    /// The original row payload.
    V1 = 1,
    /// Adds verified identity fields.
    V2,
    /// Introduces the capability policy writer barrier.
    V3,
    /// Introduces the period cutoff writer barrier.
    V4,
    /// Adds the nullable client hint.
    V5,
    /// Adds the nullable undo and related posting links.
    V6,
    /// Introduces standing settings and rule supersession in audit snapshots.
    V7,
}

impl HashVersion {
    pub(super) const fn domain(self) -> &'static str {
        match self {
            Self::V1 => "beankeeper.audit.v1",
            Self::V2 => "beankeeper.audit.v2",
            Self::V3 => "beankeeper.audit.v3",
            Self::V4 => "beankeeper.audit.v4",
            Self::V5 => "beankeeper.audit.v5",
            Self::V6 => "beankeeper.audit.v6",
            Self::V7 => "beankeeper.audit.v7",
        }
    }

    pub(super) const fn includes_identity(self) -> bool {
        !matches!(self, Self::V1)
    }

    pub(super) const fn includes_client_hint(self) -> bool {
        matches!(self, Self::V5 | Self::V6 | Self::V7)
    }

    pub(super) const fn includes_receipt_links(self) -> bool {
        matches!(self, Self::V6 | Self::V7)
    }

    pub(super) const fn for_schema(version: i64) -> Self {
        match version {
            super::super::schema::STANDING_SETTINGS_SCHEMA_VERSION.. => Self::V7,
            super::super::schema::RECEIPT_SCHEMA_VERSION.. => Self::V6,
            super::super::schema::CLIENT_HINT_SCHEMA_VERSION.. => Self::V5,
            20.. => Self::V4,
            18.. => Self::V3,
            _ => Self::V2,
        }
    }
}

/// An unrecognized audit hash version read from storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedHashVersion(i64);

impl fmt::Display for UnsupportedHashVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid audit hash version {}", self.0)
    }
}

impl std::error::Error for UnsupportedHashVersion {}

impl TryFrom<i64> for HashVersion {
    type Error = UnsupportedHashVersion;

    fn try_from(version: i64) -> Result<Self, Self::Error> {
        match version {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            5 => Ok(Self::V5),
            6 => Ok(Self::V6),
            7 => Ok(Self::V7),
            _ => Err(UnsupportedHashVersion(version)),
        }
    }
}

impl From<HashVersion> for i64 {
    fn from(version: HashVersion) -> Self {
        version as Self
    }
}

impl FromSql for HashVersion {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Self::try_from(value.as_i64()?).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl Serialize for HashVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64((*self).into())
    }
}
