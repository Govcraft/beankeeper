//! Bounded provider identifiers and normalized bank feed data.

use std::{fmt, str::FromStr};

use beankeeper::types::Currency;
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! id {
    ($name:ident, $prefix:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(MagicTypeId);

        impl $name {
            pub(in crate::db) fn generate() -> Self {
                Self($prefix.create_type_id::<V7>())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = FeedError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let id = value
                    .parse::<MagicTypeId>()
                    .map_err(|_| FeedError::InvalidIdentifier($prefix))?;
                if id.prefix_str().as_deref() == Ok($prefix) {
                    Ok(Self(id))
                } else {
                    Err(FeedError::InvalidIdentifier($prefix))
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = FeedError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.to_string()
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value
                    .as_str()?
                    .parse()
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.to_string()))
            }
        }
    };
}

id!(
    FeedConnectionId,
    "feed",
    "Stable identity of a host-managed bank feed."
);
id!(
    FeedItemId,
    "feeditem",
    "Stable identity of a staged provider transaction."
);

macro_rules! opaque {
    ($name:ident, $kind:literal, $allow_empty:literal, $max:literal) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(String);

        impl $name {
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = FeedError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.to_owned().try_into()
            }
        }

        impl TryFrom<String> for $name {
            type Error = FeedError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if (value.is_empty() && !$allow_empty)
                    || value.len() > $max
                    || value.chars().any(char::is_control)
                {
                    return Err(FeedError::InvalidIdentifier($kind));
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

opaque!(ProviderAccountKey, "provider account key", false, 512);
opaque!(
    ProviderTransactionKey,
    "provider transaction key",
    false,
    512
);
opaque!(SyncCursor, "sync cursor", true, 4096);

/// Bank movement sign, with inflows positive and outflows negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignedMinor(i64);

impl SignedMinor {
    pub(crate) const fn get(self) -> i64 {
        self.0
    }

    pub(crate) fn new(value: i64) -> Result<Self, FeedError> {
        match value {
            0 | i64::MIN => Err(FeedError::InvalidAmount),
            value => Ok(Self(value)),
        }
    }
}

/// A validated staging row. Provider-specific direction is normalized at intake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedTransaction {
    pub(crate) account: ProviderAccountKey,
    pub(crate) key: ProviderTransactionKey,
    pub(crate) pending_key: Option<ProviderTransactionKey>,
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) amount: SignedMinor,
    pub(crate) currency: Currency,
    pub(crate) pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovedTransaction {
    pub(crate) account: Option<ProviderAccountKey>,
    pub(crate) key: ProviderTransactionKey,
}

#[derive(Debug, Default)]
pub(crate) struct FeedDelta {
    pub(crate) rows: Vec<FeedTransaction>,
    pub(crate) removed: Vec<RemovedTransaction>,
}

impl FeedDelta {
    pub(crate) fn count(&self) -> usize {
        self.rows.len().saturating_add(self.removed.len())
    }
}

/// A closed set of provider and feed errors. Provider-supplied text stays private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedError {
    InvalidIdentifier(&'static str),
    InvalidAmount,
    InvalidResponse,
    Credentials,
    Transport,
    PaginationChanged,
    PaginationLimit,
    Stale,
    Disconnected,
    InvalidMapping,
    UnavailableItem,
    ExistingCandidates,
}

impl fmt::Display for FeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(kind) => write!(f, "invalid bank feed {kind}"),
            Self::InvalidAmount => {
                f.write_str("bank feed amount is zero, out of range, or exceeds currency precision")
            }
            Self::InvalidResponse => f.write_str("bank provider returned an invalid response"),
            Self::Credentials => {
                f.write_str("bank provider credentials are missing, expired, or invalid")
            }
            Self::Transport => f.write_str("bank provider is temporarily unavailable"),
            Self::PaginationChanged => f.write_str(
                "bank provider changed results while paging; retry from the committed cursor",
            ),
            Self::PaginationLimit => f.write_str("bank feed sync exceeded its bounded window"),
            Self::Stale => f.write_str("bank feed changed; retrieve the latest version and retry"),
            Self::Disconnected => f.write_str("bank feed is disconnected"),
            Self::InvalidMapping => f.write_str("invalid bank feed account or currency mapping"),
            Self::UnavailableItem => {
                f.write_str("bank feed item cannot be resolved in its current state")
            }
            Self::ExistingCandidates => {
                f.write_str("matching ledger entries already exist; review and link an entry")
            }
        }
    }
}

impl std::error::Error for FeedError {}

impl From<FeedError> for crate::error::CliError {
    fn from(error: FeedError) -> Self {
        Self::Validation(error.to_string())
    }
}
