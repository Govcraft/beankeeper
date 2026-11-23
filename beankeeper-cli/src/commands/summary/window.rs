//! Current summary bounds before defaults are resolved.

use chrono::{Datelike, NaiveDate};

use super::{DateRange, Period, date_error};
use crate::error::CliError;

/// A summary window with optional bounds. If both dates are supplied, they
/// must already form an ordered, inclusive range.
#[derive(Clone, Copy, Debug, Default)]
pub enum SummaryWindow {
    /// Month to date, using the local date when the request is constructed.
    #[default]
    MonthToDate,
    /// Start on the given date and end on the local date.
    StartingOn(NaiveDate),
    /// Start on the first day of the given date's month.
    Through(NaiveDate),
    /// An explicit period with both bounds.
    Exact(DateRange),
}

impl TryFrom<(Option<NaiveDate>, Option<NaiveDate>)> for SummaryWindow {
    type Error = CliError;

    fn try_from((from, to): (Option<NaiveDate>, Option<NaiveDate>)) -> Result<Self, Self::Error> {
        match (from, to) {
            (None, None) => Ok(Self::default()),
            (Some(from), None) => Ok(Self::StartingOn(from)),
            (None, Some(to)) => Ok(Self::Through(to)),
            (Some(from), Some(to)) => DateRange::new(from, to).map(Self::Exact),
        }
    }
}

impl SummaryWindow {
    /// The explicitly supplied start date, if present.
    #[must_use]
    pub const fn start(self) -> Option<NaiveDate> {
        match self {
            Self::StartingOn(from) => Some(from),
            Self::Exact(range) => Some(range.from()),
            Self::MonthToDate | Self::Through(_) => None,
        }
    }

    pub(super) fn period(self, today: NaiveDate) -> Result<Period, CliError> {
        let to = match self {
            Self::Exact(range) => return Ok(range.into()),
            Self::StartingOn(from) => return DateRange::new(from, today).map(Into::into),
            Self::Through(to) => to,
            Self::MonthToDate => today,
        };
        let from = to.with_day(1).ok_or_else(date_error)?;
        Ok(Period { from, to })
    }
}
