//! Shared inclusive date defaults for report commands.

use chrono::{Datelike, NaiveDate};

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

fn parse_date(flag: &str, value: &str) -> Result<NaiveDate, CliError> {
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
            period,
            compare,
            ..
        } => {
            resolve_bounds(from, to, period.is_some() || compare.is_some(), today)?;
        }
        ReportCommand::Balance { from, to, .. } | ReportCommand::Ledger { from, to, .. } => {
            resolve_bounds(from, to, false, today)?;
        }
        ReportCommand::IncomeStatement { from, to, .. }
        | ReportCommand::TaxSummary { from, to, .. } => {
            resolve_bounds(from, to, true, today)?;
        }
        ReportCommand::BalanceSheet { to, .. } | ReportCommand::Reconciliation { to, .. } => {
            resolve_bounds(&mut None, to, false, today)?;
        }
        ReportCommand::Aging { as_of, .. } => {
            if let Some(value) = as_of.as_deref() {
                parse_date("--as-of", value)?;
            } else {
                *as_of = Some(today.to_string());
            }
        }
        ReportCommand::ScheduleC(args) => {
            resolve_bounds(&mut args.from, &mut args.to, true, today)?;
        }
        ReportCommand::Vendor1099(args) => {
            resolve_bounds(&mut args.period.from, &mut args.period.to, true, today)?;
        }
        ReportCommand::Trend(args) => {
            resolve_bounds(&mut args.from, &mut args.to, true, today)?;
        }
        ReportCommand::CashFlow(args) => {
            resolve_bounds(&mut args.from, &mut args.to, true, today)?;
        }
        // Budget variance uses an explicit year and month numbers, not dates.
        ReportCommand::BudgetVariance { .. } => {}
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
