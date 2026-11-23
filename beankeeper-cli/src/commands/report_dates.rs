//! Shared inclusive date defaults for report commands.

use chrono::{Datelike, NaiveDate};
use serde::Serialize;

use crate::cli::CalendarDateArg;
use crate::error::CliError;

use crate::request::LocalDate;

/// Effective dates, resolved once at the report boundary.
#[derive(Debug, Clone, Copy)]
pub(super) struct DateWindow {
    from: Option<NaiveDate>,
    to: NaiveDate,
}

impl DateWindow {
    pub(super) const fn from(self) -> Option<NaiveDate> {
        self.from
    }

    pub(super) const fn to(self) -> NaiveDate {
        self.to
    }

    /// Resolve an optional start and required end against an explicit local today.
    pub fn resolve(
        from: Option<&CalendarDateArg>,
        to: Option<&CalendarDateArg>,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let to = to
            .map(|date| resolve_date("--to", date))
            .transpose()?
            .unwrap_or(today);
        let from = from.map(|date| resolve_date("--from", date)).transpose()?;
        Self::try_from((from, Some(to)))
    }
}

impl TryFrom<(Option<NaiveDate>, Option<NaiveDate>)> for DateWindow {
    type Error = CliError;

    fn try_from((from, to): (Option<NaiveDate>, Option<NaiveDate>)) -> Result<Self, Self::Error> {
        let to = to.unwrap_or_else(|| LocalDate::default().into());
        if let Some(from) = from
            && from > to
        {
            return Err(CliError::Validation(format!(
                "--from ({from}) must be on or before --to ({to})"
            )));
        }
        Ok(Self { from, to })
    }
}

/// A report window with both validated, ordered endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DateRange {
    from: NaiveDate,
    to: NaiveDate,
}

impl DateRange {
    /// Validate an inclusive date range.
    ///
    /// # Errors
    /// Returns [`CliError::Validation`] when the start is later than the end.
    pub fn new(from: NaiveDate, to: NaiveDate) -> Result<Self, CliError> {
        if from > to {
            return Err(CliError::Validation(format!(
                "--from ({from}) must be on or before --to ({to})"
            )));
        }
        Ok(Self { from, to })
    }

    pub(super) fn resolve(
        from: Option<&CalendarDateArg>,
        to: Option<&CalendarDateArg>,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let window = DateWindow::resolve(from, to, today)?;
        Self::try_from(window)
    }

    /// First date included in the report.
    #[must_use]
    pub const fn from(self) -> NaiveDate {
        self.from
    }

    /// Last date included in the report.
    #[must_use]
    pub const fn to(self) -> NaiveDate {
        self.to
    }
}

impl TryFrom<DateWindow> for DateRange {
    type Error = CliError;

    fn try_from(window: DateWindow) -> Result<Self, Self::Error> {
        let from = match window.from {
            Some(from) => from,
            None => window.to.with_ordinal(1).ok_or_else(|| {
                CliError::Validation("date is out of the supported range".to_string())
            })?,
        };
        Ok(Self {
            from,
            to: window.to,
        })
    }
}

pub(super) fn resolve_date(flag: &str, date: &CalendarDateArg) -> Result<NaiveDate, CliError> {
    date.resolve(|value, _| invalid_date(flag, value))
}

fn invalid_date(flag: &str, value: &str) -> CliError {
    CliError::Validation(format!(
        "invalid {flag} date {value:?}: expected YYYY-MM-DD"
    ))
}

#[cfg(test)]
mod tests {
    use super::{CalendarDateArg, DateRange, DateWindow};
    use chrono::NaiveDate;

    fn arg(date: &str) -> CalendarDateArg {
        CalendarDateArg::decode(date)
    }

    #[test]
    fn defaults_use_effective_end_year_and_preserve_point_in_time_history() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap_or_default();
        let point = DateWindow::resolve(None, None, today).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(point.from, None);
        assert_eq!(point.to, today);
        let period = DateRange::resolve(None, Some(&arg("2024-02-29")), today)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(period.from().to_string(), "2024-01-01");
        assert_eq!(period.to().to_string(), "2024-02-29");
    }

    #[test]
    fn explicit_future_end_is_allowed_but_bad_and_reversed_dates_fail() {
        let today = NaiveDate::default();
        assert!(DateWindow::resolve(None, Some(&arg("2030-01-01")), today).is_ok());
        for (from, to) in [
            (Some(arg("bad")), None),
            (None, Some(arg("2026-02-30"))),
            (Some(arg("2030-01-02")), Some(arg("2030-01-01"))),
        ] {
            assert!(DateWindow::resolve(from.as_ref(), to.as_ref(), today).is_err());
        }
    }
}
