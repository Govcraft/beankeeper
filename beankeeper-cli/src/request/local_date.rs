//! Calendar dates whose default uses the local clock.

use std::str::FromStr;

use chrono::{NaiveDate, ParseError};

/// A calendar date, defaulting to the current day in the local time zone.
#[derive(Debug, Clone, Copy)]
pub struct LocalDate(NaiveDate);

impl Default for LocalDate {
    fn default() -> Self {
        Self(chrono::Local::now().date_naive())
    }
}

impl FromStr for LocalDate {
    type Err = ParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse().map(Self)
    }
}

impl From<NaiveDate> for LocalDate {
    fn from(date: NaiveDate) -> Self {
        Self(date)
    }
}

impl From<Option<NaiveDate>> for LocalDate {
    /// Use the supplied date or sample the local date when it is omitted.
    fn from(date: Option<NaiveDate>) -> Self {
        date.map(Self::from).unwrap_or_default()
    }
}

impl From<LocalDate> for NaiveDate {
    fn from(date: LocalDate) -> Self {
        date.0
    }
}
