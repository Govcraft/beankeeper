//! Calendar dates decoded from CLI and MCP input.

use std::{fmt, str::FromStr};

use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::CliError;

/// A real calendar date. Text input accepts the CLI's legacy unpadded dates.
#[derive(Clone, Copy, Debug, JsonSchema, PartialEq, Eq, Serialize)]
#[schemars(transparent)]
#[serde(transparent)]
pub struct CalendarDateArg(#[schemars(with = "String")] NaiveDate);

/// The input was malformed or did not name a real calendar day.
#[derive(Debug)]
pub struct CalendarDateError(Box<str>);

impl CalendarDateError {
    /// Return the rejected input for transport-specific diagnostics.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CalendarDateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid date {:?}: expected YYYY-MM-DD", self.0)
    }
}

impl std::error::Error for CalendarDateError {}

impl From<CalendarDateError> for CliError {
    fn from(error: CalendarDateError) -> Self {
        Self::Validation(error.to_string())
    }
}

impl FromStr for CalendarDateArg {
    type Err = CalendarDateError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(Self)
            .map_err(|_| CalendarDateError(raw.into()))
    }
}

impl From<NaiveDate> for CalendarDateArg {
    fn from(date: NaiveDate) -> Self {
        Self(date)
    }
}

impl From<CalendarDateArg> for NaiveDate {
    fn from(date: CalendarDateArg) -> Self {
        date.0
    }
}

impl<'de> Deserialize<'de> for CalendarDateArg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::CalendarDateArg;
    use chrono::NaiveDate;

    #[test]
    fn parser_and_serde_accept_legacy_spellings_of_real_dates()
    -> Result<(), Box<dyn std::error::Error>> {
        let date = "2024-02-29".parse::<CalendarDateArg>()?;
        assert_eq!(
            Some(NaiveDate::from(date)),
            NaiveDate::from_ymd_opt(2024, 2, 29)
        );
        let encoded = serde_json::to_value(date)?;
        assert_eq!(encoded, "2024-02-29");
        assert_eq!(serde_json::from_value::<CalendarDateArg>(encoded)?, date);
        assert_eq!("2024-2-29".parse::<CalendarDateArg>()?, date);
        assert_eq!(
            serde_json::from_value::<CalendarDateArg>("2024-2-29".into())?,
            date
        );

        for invalid in ["2026-02-29", "2024-04-31", "2024-02-29T00:00:00Z"] {
            assert!(invalid.parse::<CalendarDateArg>().is_err(), "{invalid}");
            assert!(
                serde_json::from_value::<CalendarDateArg>(invalid.into()).is_err(),
                "{invalid}"
            );
        }
        Ok(())
    }
}
