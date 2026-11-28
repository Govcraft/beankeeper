//! Stable typed identifiers carried across ledger, CLI, and MCP boundaries.

use std::{fmt, str::FromStr};

use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ProjectError;

macro_rules! project_ids {
    ($($(#[$doc:meta])* $name:ident => $prefix:literal),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
            type Err = ProjectError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let id = raw.parse::<MagicTypeId>().map_err(|_| ProjectError::InvalidId($prefix))?;
                if id.prefix_str().as_deref() != Ok($prefix) {
                    return Err(ProjectError::InvalidId($prefix));
                }
                Ok(Self(id))
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProjectError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                raw.parse()
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.to_string()
            }
        }

        impl FromSql for $name {
            fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
                raw.as_str()?
                    .parse()
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(self.to_string().into())
            }
        }
    )+};
}

project_ids! {
    /// Stable identity of a project in a company.
    ProjectId => "project",
    /// Immutable submitted work record.
    TimeEntryId => "timeentry",
    /// One allocation of a journal entry to a project.
    ProjectSplitId => "projectsplit",
}
