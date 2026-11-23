//! Shared inclusive date defaults for report commands.

use chrono::{Datelike, NaiveDate};
use serde::Serialize;

use crate::error::CliError;
use crate::request::LocalDate;

/// Effective dates, resolved once at the report boundary.
#[derive(Debug, Clone, Copy)]
pub(super) struct DateWindow {
    from: Option<NaiveDate>,
    to: LocalDate,
}

impl DateWindow {
    pub(super) const fn from(self) -> Option<NaiveDate> {
        self.from
    }

    pub(super) fn to(self) -> NaiveDate {
        self.to.into()
    }
}

impl TryFrom<(Option<NaiveDate>, Option<NaiveDate>)> for DateWindow {
    type Error = CliError;

    fn try_from((from, to): (Option<NaiveDate>, Option<NaiveDate>)) -> Result<Self, Self::Error> {
        let to = LocalDate::from(to);
        let end = NaiveDate::from(to);
        if let Some(from) = from
            && from > end
        {
            return Err(CliError::Validation(format!(
                "--from ({from}) must be on or before --to ({end})"
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
        let to = window.to();
        let from = match window.from {
            Some(from) => from,
            None => to.with_ordinal(1).ok_or_else(|| {
                CliError::Validation("date is out of the supported range".to_string())
            })?,
        };
        Ok(Self { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::{DateRange, DateWindow};
    use crate::error::CliError;
    use chrono::Local;

    #[test]
    fn defaults_use_effective_end_year_and_preserve_point_in_time_history() -> Result<(), CliError>
    {
        let before = Local::now().date_naive();
        let point = DateWindow::try_from((None, None))?;
        let after = Local::now().date_naive();
        assert_eq!(point.from(), None);
        assert!((before..=after).contains(&point.to()));

        let last = "2024-02-29".parse::<crate::cli::CalendarDateArg>()?.into();
        let period = DateRange::try_from(DateWindow::try_from((None, Some(last)))?)?;
        assert_eq!(period.from().to_string(), "2024-01-01");
        assert_eq!(period.to(), last);
        Ok(())
    }

    #[test]
    fn explicit_future_end_is_allowed_but_reversed_dates_fail() -> Result<(), chrono::ParseError> {
        let start = "2030-01-02".parse()?;
        let end = "2030-01-01".parse()?;
        assert!(DateWindow::try_from((None, Some(end))).is_ok());
        assert!(DateWindow::try_from((Some(start), Some(end))).is_err());
        Ok(())
    }
}
