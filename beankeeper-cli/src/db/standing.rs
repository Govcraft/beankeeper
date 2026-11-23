//! Company-scoped bookkeeping policies and owner inform preference.

use std::{fmt, str::FromStr};

use rusqlite::{
    OptionalExtension,
    types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::companies::Company;
use crate::error::CliError;

macro_rules! setting_values {
    ($(#[$doc:meta])* $name:ident, $label:literal { $default:ident => $default_value:literal, $($variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
        pub enum $name {
            #[default]
            #[serde(rename = $default_value)]
            $default,
            $(#[serde(rename = $value)] $variant,)+
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                match self {
                    Self::$default => $default_value,
                    $(Self::$variant => $value,)+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(self.as_ref())
            }
        }

        impl FromStr for $name {
            type Err = CliError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $default_value => Ok(Self::$default),
                    $($value => Ok(Self::$variant),)+
                    _ => Err(CliError::Validation(format!(
                        "invalid {} '{value}'; expected one of {}",
                        $label, [$default_value, $($value),+].join(", ")
                    ))),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

setting_values! {
    /// How the agent should treat sales tax when it keeps these books.
    SalesTaxTreatment, "sales-tax treatment" {
        Unset => "unset",
        TrackLiability => "track-liability",
        MarketplaceRemitted => "marketplace-remitted",
        NotApplicable => "not-applicable",
    }
}

setting_values! {
    /// How the agent should record payment processor fees.
    ProcessorFeeHandling, "processor-fee handling" {
        Unset => "unset",
        SeparateExpense => "separate-expense",
        NetFromRevenue => "net-from-revenue",
    }
}

setting_values! {
    /// How often the owner wants to hear about changes already made.
    InformPreference, "inform preference" {
        PerChange => "per-change",
        DailyDigest => "daily-digest",
        WeeklyDigest => "weekly-digest",
    }
}

/// One standing bookkeeping instruction to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum BookkeepingPolicy {
    /// Set the sales-tax instruction.
    SalesTax(SalesTaxTreatment),
    /// Set the processor-fee instruction.
    ProcessorFees(ProcessorFeeHandling),
}

/// The current standing bookkeeping instructions for a company.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BookkeepingPolicies {
    pub sales_tax: SalesTaxTreatment,
    pub processor_fees: ProcessorFeeHandling,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settings {
    pub(super) bookkeeping: BookkeepingPolicies,
    pub(super) inform: Option<InformPreference>,
}

impl Settings {
    pub(super) fn inform_preference(self) -> InformPreference {
        self.inform.unwrap_or_default()
    }

    pub(super) fn read(company: &Company<'_>) -> Result<Self, CliError> {
        Ok(company
            .read()
            .query_row(
                "SELECT sales_tax, processor_fees, inform_preference FROM standing_settings
             WHERE company_slug = ?1 ORDER BY revision DESC LIMIT 1",
                [company.slug()],
                |row| {
                    Ok(Self {
                        bookkeeping: BookkeepingPolicies {
                            sales_tax: row.get(0)?,
                            processor_fees: row.get(1)?,
                        },
                        inform: row.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }
}

/// Read a company's current standing bookkeeping policies.
///
/// # Errors
/// Returns a database error if the settings cannot be read.
pub fn bookkeeping_policies(company: &Company<'_>) -> Result<BookkeepingPolicies, CliError> {
    Ok(Settings::read(company)?.bookkeeping)
}

/// Read a company's inform preference, using its default when unset.
///
/// # Errors
/// Returns a database error if the settings cannot be read.
pub fn inform_preference(company: &Company<'_>) -> Result<InformPreference, CliError> {
    Ok(Settings::read(company)?.inform_preference())
}
