//! Pure calendar calculations for scheduled transaction templates.

use core::{fmt, str::FromStr};

use chrono::{Days, Months, NaiveDate};

/// How often a transaction template becomes due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frequency {
    /// A single obligation on the start date.
    Once,
    /// Every calendar day.
    Daily,
    /// Every seven calendar days.
    Weekly,
    /// Every calendar month, anchored to the start date's day.
    Monthly,
    /// Every calendar year, anchored to the start date's month and day.
    Yearly,
}

impl Frequency {
    /// Canonical form stored in a ledger and emitted on the wire.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Frequency {
    type Err = RecurrenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "once" => Ok(Self::Once),
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "monthly" => Ok(Self::Monthly),
            "yearly" => Ok(Self::Yearly),
            _ => Err(RecurrenceError::InvalidFrequency {
                value: value.to_owned(),
            }),
        }
    }
}

/// Invalid recurrence input or an occurrence outside the supported date range.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecurrenceError {
    /// The provided frequency name is unsupported.
    InvalidFrequency { value: String },
    /// Calculating the occurrence exceeds the supported calendar range.
    DateOverflow,
}

impl fmt::Display for RecurrenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrequency { value } => write!(
                f,
                "invalid frequency {value:?}; expected once, daily, weekly, monthly, or yearly"
            ),
            Self::DateOverflow => {
                f.write_str("scheduled occurrence exceeds the supported date range")
            }
        }
    }
}

impl std::error::Error for RecurrenceError {}

/// Calculate an occurrence from its original start date and zero-based index.
///
/// Monthly and yearly occurrences keep the start date's day, clamping to the
/// last day of shorter months. Each calculation uses the original start date,
/// so January 31 becomes February 28 and then March 31 rather than March 28.
/// A one-time schedule returns `None` after its first occurrence.
///
/// # Errors
/// Returns [`RecurrenceError::DateOverflow`] if the occurrence cannot be
/// represented by [`NaiveDate`].
pub fn occurrence_date(
    start: NaiveDate,
    frequency: Frequency,
    occurrence: u32,
) -> Result<Option<NaiveDate>, RecurrenceError> {
    let date = match frequency {
        Frequency::Once => return Ok((occurrence == 0).then_some(start)),
        Frequency::Daily => start.checked_add_days(Days::new(u64::from(occurrence))),
        Frequency::Weekly => start.checked_add_days(Days::new(u64::from(occurrence) * 7)),
        Frequency::Monthly => start.checked_add_months(Months::new(occurrence)),
        Frequency::Yearly => start.checked_add_months(Months::new(
            occurrence
                .checked_mul(12)
                .ok_or(RecurrenceError::DateOverflow)?,
        )),
    };
    date.map(Some).ok_or(RecurrenceError::DateOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap_or_else(|| panic!("invalid test date {year}-{month}-{day}"))
    }

    #[test]
    fn once_has_only_one_occurrence() {
        let start = date(2026, 1, 31);
        assert_eq!(occurrence_date(start, Frequency::Once, 0), Ok(Some(start)));
        assert_eq!(occurrence_date(start, Frequency::Once, 1), Ok(None));
        assert_eq!(occurrence_date(start, Frequency::Once, u32::MAX), Ok(None));
    }

    #[test]
    fn monthly_uses_original_day_after_a_short_month() {
        let start = date(2026, 1, 31);
        for (index, expected) in [
            (0, start),
            (1, date(2026, 2, 28)),
            (2, date(2026, 3, 31)),
            (3, date(2026, 4, 30)),
            (12, date(2027, 1, 31)),
            (25, date(2028, 2, 29)),
        ] {
            assert_eq!(
                occurrence_date(start, Frequency::Monthly, index),
                Ok(Some(expected))
            );
        }
    }

    #[test]
    fn yearly_restores_leap_day_when_available() {
        let start = date(2024, 2, 29);
        for (index, expected) in [
            (0, start),
            (1, date(2025, 2, 28)),
            (2, date(2026, 2, 28)),
            (4, date(2028, 2, 29)),
            (76, date(2100, 2, 28)),
        ] {
            assert_eq!(
                occurrence_date(start, Frequency::Yearly, index),
                Ok(Some(expected))
            );
        }
    }

    #[test]
    fn daily_and_weekly_cross_month_and_year_boundaries() {
        let start = date(2023, 12, 31);
        assert_eq!(
            occurrence_date(start, Frequency::Daily, 60),
            Ok(Some(date(2024, 2, 29)))
        );
        assert_eq!(
            occurrence_date(start, Frequency::Weekly, 9),
            Ok(Some(date(2024, 3, 3)))
        );
    }

    #[test]
    fn out_of_range_occurrences_return_errors() {
        for frequency in [
            Frequency::Daily,
            Frequency::Weekly,
            Frequency::Monthly,
            Frequency::Yearly,
        ] {
            assert_eq!(
                occurrence_date(NaiveDate::MAX, frequency, 1),
                Err(RecurrenceError::DateOverflow)
            );
            assert_eq!(
                occurrence_date(date(2026, 1, 1), frequency, u32::MAX),
                Err(RecurrenceError::DateOverflow)
            );
        }
    }

    #[test]
    fn frequency_names_round_trip_and_invalid_names_fail() {
        for frequency in [
            Frequency::Once,
            Frequency::Daily,
            Frequency::Weekly,
            Frequency::Monthly,
            Frequency::Yearly,
        ] {
            assert_eq!(frequency.to_string().parse(), Ok(frequency));
        }
        for invalid in ["", "Monthly", "monthly ", "fortnightly"] {
            assert!(matches!(
                invalid.parse::<Frequency>(),
                Err(RecurrenceError::InvalidFrequency { .. })
            ));
        }
    }
}
