//! Shared inclusive date defaults for report commands.

use chrono::{Datelike, NaiveDate};
use serde::Serialize;

use crate::cli::ReportCommand;
use crate::error::CliError;

/// Effective dates, resolved once at the report boundary.
pub(super) struct DateWindow {
    pub from: Option<NaiveDate>,
    pub to: NaiveDate,
}

impl DateWindow {
    /// Resolve point-in-time or period dates against an explicit local today.
    pub fn resolve(
        from: Option<&str>,
        to: Option<&str>,
        period: bool,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let to = to
            .map(|date| parse_date("--to", date))
            .transpose()?
            .unwrap_or(today);
        let from = from.map(|date| parse_date("--from", date)).transpose()?;
        let from = if period && from.is_none() {
            Some(to.with_ordinal(1).ok_or_else(|| {
                CliError::Validation("date is out of the supported range".to_string())
            })?)
        } else {
            from
        };
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
        from: Option<&str>,
        to: Option<&str>,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let window = DateWindow::resolve(from, to, false, today)?;
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

pub(super) fn parse_date(flag: &str, value: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        CliError::Validation(format!(
            "invalid {flag} date {value:?}: expected YYYY-MM-DD"
        ))
    })
}

fn resolve_bounds(
    from: &mut Option<String>,
    to: &mut Option<String>,
    period: bool,
    today: NaiveDate,
) -> Result<(), CliError> {
    let window = DateWindow::resolve(from.as_deref(), to.as_deref(), period, today)?;
    *from = window.from.map(|date| date.to_string());
    *to = Some(window.to.to_string());
    Ok(())
}

/// Apply the same defaults and date validation to every date-based report.
pub(super) fn resolve(sub: &ReportCommand, today: NaiveDate) -> Result<ReportCommand, CliError> {
    let mut sub = sub.clone();
    match &mut sub {
        ReportCommand::TrialBalance {
            from,
            to,
            period: None,
            compare: None,
            ..
        }
        | ReportCommand::Balance { from, to, .. } => {
            resolve_bounds(from, to, false, today)?;
        }
        ReportCommand::TaxSummary { from, to, .. } => {
            resolve_bounds(from, to, true, today)?;
        }
        ReportCommand::BalanceSheet { to, .. } => {
            resolve_bounds(&mut None, to, false, today)?;
        }
        ReportCommand::Reconciliation(args) => {
            resolve_bounds(&mut None, &mut args.to, false, today)?;
        }
        ReportCommand::Aging(args) => {
            let as_of = &mut args.as_of;
            if let Some(value) = as_of.as_deref() {
                parse_date("--as-of", value)?;
            } else {
                *as_of = Some(today.to_string());
            }
        }
        // These commands resolve their own date or month windows.
        ReportCommand::ScheduleC(_)
        | ReportCommand::Vendor1099(_)
        | ReportCommand::IncomeStatement { .. }
        | ReportCommand::TrialBalance { .. }
        | ReportCommand::Trend(_)
        | ReportCommand::Ledger { .. }
        | ReportCommand::CashFlow(_)
        // Budget variance uses an explicit year and month numbers, not dates.
        | ReportCommand::BudgetVariance { .. } => {}
    }
    Ok(sub)
}

#[cfg(test)]
mod tests {
    use super::DateWindow;
    use chrono::NaiveDate;

    #[test]
    fn defaults_use_effective_end_year_and_preserve_point_in_time_history() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap_or_default();
        let point = DateWindow::resolve(None, None, false, today).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(point.from, None);
        assert_eq!(point.to, today);
        let period = DateWindow::resolve(None, Some("2024-02-29"), true, today)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            period.from.map(|d| d.to_string()).as_deref(),
            Some("2024-01-01")
        );
        assert_eq!(period.to.to_string(), "2024-02-29");
    }

    #[test]
    fn explicit_future_end_is_allowed_but_bad_and_reversed_dates_fail() {
        let today = NaiveDate::default();
        assert!(DateWindow::resolve(None, Some("2030-01-01"), false, today).is_ok());
        for (from, to) in [
            (Some("bad"), None),
            (None, Some("2026-02-30")),
            (Some("2030-01-02"), Some("2030-01-01")),
        ] {
            assert!(DateWindow::resolve(from, to, false, today).is_err());
        }
    }
}
