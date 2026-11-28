//! Deterministically project cash from reviewed, dated cash movements.

use std::{collections::BTreeMap, fmt};

use chrono::{Datelike, Days, Months, NaiveDate};

use crate::types::{Money, MoneyError};

/// Future dates covered by a projection, immediately after the balance date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForecastWindow {
    as_of: NaiveDate,
    through: NaiveDate,
}

impl ForecastWindow {
    /// Specify the date of the opening balance and the last included date.
    ///
    /// # Errors
    /// The end date must follow the balance date by at most 1,830 days.
    pub fn new(as_of: NaiveDate, through: NaiveDate) -> Result<Self, ForecastError> {
        if !(1..=1830).contains(&through.signed_duration_since(as_of).num_days()) {
            return Err(ForecastError::Window);
        }
        Ok(Self { as_of, through })
    }
}

/// How forecast days are collected in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastCadence {
    /// Seven days from the first day of the projection.
    Weekly,
    /// Calendar months, with partial first and last months.
    Monthly,
}

impl ForecastCadence {
    fn period_end(self, from: NaiveDate, through: NaiveDate) -> NaiveDate {
        match self {
            Self::Weekly => from.checked_add_days(Days::new(6)),
            Self::Monthly => from
                .with_day(1)
                .and_then(|first| first.checked_add_months(Months::new(1)))
                .and_then(|next_month| next_month.pred_opt()),
        }
        .map_or(through, |end| end.min(through))
    }
}

/// An expected movement. A missing date leaves the amount outside the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastEvent<S> {
    /// Source key determined by the caller's domain.
    pub source: S,
    /// Expected cash date, independent of journal date.
    pub on: Option<NaiveDate>,
    /// Positive for inflows and negative for outflows.
    pub movement: Money,
}

/// A reviewed scenario amendment for one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastOverride {
    /// Change the expected date without changing the amount.
    Reschedule(NaiveDate),
    /// Change the signed amount, preserving the date.
    ChangeAmount(Money),
    /// Replace both date and amount.
    Replace {
        /// Replacement expected cash date.
        on: NaiveDate,
        /// Replacement signed cash movement.
        movement: Money,
    },
    /// Keep a source visible outside the projected cash balance.
    Unscheduled,
    /// Remove the source from this scenario.
    Omit,
}

/// One projected interval. Outflows are negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CashPeriod {
    /// First day included in this period.
    pub from: NaiveDate,
    /// Last day included in this period.
    pub through: NaiveDate,
    /// Balance before this period's movement.
    pub opening: Money,
    /// Expected positive movements.
    pub inflows: Money,
    /// Expected negative movements.
    pub outflows: Money,
    /// Balance after this period's movement.
    pub closing: Money,
}

/// A cash projection with evidence by source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashProjection<S> {
    /// Projected periods in date order.
    pub periods: Vec<CashPeriod>,
    /// Events considered, in source order. Some may lie outside the window.
    pub events: Vec<ForecastEvent<S>>,
    /// Lowest balance after netting activity per day, including the opening balance.
    pub minimum: Money,
    /// First date ending below zero, or the opening balance date if already below zero.
    pub first_shortfall: Option<NaiveDate>,
}

/// A rejected projection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForecastError {
    /// The period must end within the supported horizon and after its opening date.
    Window,
    /// A source appeared more than once before scenario amendments.
    DuplicateSource,
    /// A scenario referred to a source missing from the inputs.
    UnknownSource,
    /// Money arithmetic failed.
    Money(MoneyError),
}

impl fmt::Display for ForecastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window => f.write_str("forecast must cover between 1 and 1,830 days"),
            Self::DuplicateSource => f.write_str("a cash movement source appears more than once"),
            Self::UnknownSource => {
                f.write_str("scenario refers to an unknown cash movement source")
            }
            Self::Money(error) => write!(f, "cash forecast amount: {error}"),
        }
    }
}

impl std::error::Error for ForecastError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Money(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MoneyError> for ForecastError {
    fn from(error: MoneyError) -> Self {
        Self::Money(error)
    }
}

/// Apply reviewed scenario assumptions and calculate running daily and period balances.
///
/// Events at or before the opening date, events beyond the horizon, and
/// undated events remain visible in the evidence but do not move cash.
/// Multiple events on the same date are netted before measuring shortfalls.
///
/// # Errors
/// Rejects duplicate or unknown sources, mixed currency, or amounts that
/// overflow exact checked arithmetic.
pub fn forecast_cash<S: Ord>(
    window: ForecastWindow,
    cadence: ForecastCadence,
    opening: Money,
    events: impl IntoIterator<Item = ForecastEvent<S>>,
    overrides: &BTreeMap<S, ForecastOverride>,
) -> Result<CashProjection<S>, ForecastError> {
    let mut by_source = BTreeMap::new();
    for event in events {
        if by_source
            .insert(event.source, (event.on, event.movement))
            .is_some()
        {
            return Err(ForecastError::DuplicateSource);
        }
    }
    if overrides
        .keys()
        .any(|source| !by_source.contains_key(source))
    {
        return Err(ForecastError::UnknownSource);
    }
    let events: Vec<_> = by_source
        .into_iter()
        .filter_map(|(source, (mut on, mut movement))| {
            match overrides.get(&source).copied() {
                Some(ForecastOverride::Reschedule(date)) => on = Some(date),
                Some(ForecastOverride::ChangeAmount(next)) => movement = next,
                Some(ForecastOverride::Replace {
                    on: date,
                    movement: next,
                }) => {
                    on = Some(date);
                    movement = next;
                }
                Some(ForecastOverride::Unscheduled) => on = None,
                Some(ForecastOverride::Omit) => return None,
                None => {}
            }
            Some(ForecastEvent {
                source,
                on,
                movement,
            })
        })
        .collect();
    let zero = Money::from_minor(0, opening.currency());
    let mut days = BTreeMap::new();
    for event in &events {
        // Check currency even for evidence outside the running balance.
        zero.checked_add(event.movement)?;
        if let Some(date) = event
            .on
            .filter(|&date| date > window.as_of && date <= window.through)
        {
            let (inflows, outflows) = days.entry(date).or_insert((zero, zero));
            if event.movement.is_negative() {
                *outflows = outflows.checked_add(event.movement)?;
            } else {
                *inflows = inflows.checked_add(event.movement)?;
            }
        }
    }
    let mut result = CashProjection {
        periods: Vec::new(),
        events,
        minimum: opening,
        first_shortfall: opening.is_negative().then_some(window.as_of),
    };
    let mut balance = opening;
    let mut start = window.as_of.succ_opt().ok_or(ForecastError::Window)?;
    loop {
        let end = cadence.period_end(start, window.through);
        let mut period = CashPeriod {
            from: start,
            through: end,
            opening: balance,
            inflows: zero,
            outflows: zero,
            closing: balance,
        };
        for (&date, &(inflows, outflows)) in days.range(start..=end) {
            period.inflows = period.inflows.checked_add(inflows)?;
            period.outflows = period.outflows.checked_add(outflows)?;
            balance = balance.checked_add(inflows.checked_add(outflows)?)?;
            if balance.amount() < result.minimum.amount() {
                result.minimum = balance;
            }
            if result.first_shortfall.is_none() && balance.is_negative() {
                result.first_shortfall = Some(date);
            }
        }
        period.closing = balance;
        result.periods.push(period);
        if end == window.through {
            return Ok(result);
        }
        start = end.succ_opt().ok_or(ForecastError::Window)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescheduling_a_collection_can_cause_a_shortfall_inside_a_month() -> Result<(), ForecastError>
    {
        let date = |day| NaiveDate::from_ymd_opt(2026, 2, day).ok_or(ForecastError::Window);
        let window = ForecastWindow::new(date(1)?, date(20)?)?;
        let events = [
            ForecastEvent {
                source: "invoice",
                on: Some(date(7)?),
                movement: Money::usd(12000),
            },
            ForecastEvent {
                source: "bill",
                on: Some(date(8)?),
                movement: Money::usd(-15_000),
            },
            ForecastEvent {
                source: "late",
                on: None,
                movement: Money::usd(50_000),
            },
        ];
        let baseline = forecast_cash(
            window,
            ForecastCadence::Monthly,
            Money::usd(10_000),
            events.clone(),
            &BTreeMap::new(),
        )?;
        let late = forecast_cash(
            window,
            ForecastCadence::Monthly,
            Money::usd(10_000),
            events,
            &BTreeMap::from([("invoice", ForecastOverride::Reschedule(date(15)?))]),
        )?;

        assert_eq!(baseline.first_shortfall, None);
        assert_eq!(late.first_shortfall, Some(date(8)?));
        assert_eq!(late.minimum, Money::usd(-5_000));
        assert_eq!(late.periods, baseline.periods);
        assert_eq!(late.events.len(), 3);
        Ok(())
    }

    #[test]
    fn a_weekly_projection_includes_partial_final_week_and_nets_payments_on_a_day()
    -> Result<(), ForecastError> {
        let feb = |day| NaiveDate::from_ymd_opt(2028, 2, day).ok_or(ForecastError::Window);
        let mar = |day| NaiveDate::from_ymd_opt(2028, 3, day).ok_or(ForecastError::Window);
        let window = ForecastWindow::new(feb(26)?, mar(8)?)?;
        let events = [
            ForecastEvent {
                source: "included",
                on: Some(feb(29)?),
                movement: Money::usd(200),
            },
            ForecastEvent {
                source: "same day",
                on: Some(feb(29)?),
                movement: Money::usd(-200),
            },
            ForecastEvent {
                source: "beyond",
                on: Some(mar(9)?),
                movement: Money::usd(-4_000),
            },
        ];
        let forecast = forecast_cash(
            window,
            ForecastCadence::Weekly,
            Money::usd(0),
            events,
            &BTreeMap::new(),
        )?;
        assert_eq!(forecast.first_shortfall, None);
        assert_eq!(
            forecast
                .periods
                .iter()
                .map(|p| p.through)
                .collect::<Vec<_>>(),
            [mar(4)?, mar(8)?]
        );
        assert_eq!(forecast.periods[0].closing, Money::usd(0));
        assert_eq!(forecast.periods[1].opening, forecast.periods[0].closing);
        Ok(())
    }
}
