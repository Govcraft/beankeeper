//! Explicitly associate an unposted recurrence with an open-item obligation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use super::{Events, ForecastSource};
use crate::{error::CliError, output::amounts::MoneyField};

/// One projected schedule occurrence explicitly designated to pay one open item.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub(crate) struct ForecastLink {
    schedule: ForecastSource,
    document: ForecastSource,
}

impl ForecastLink {
    pub(super) const fn pair(&self) -> (&ForecastSource, &ForecastSource) {
        (&self.schedule, &self.document)
    }
}

impl FromStr for ForecastLink {
    type Err = ForecastLinkError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (occurrence, item) = raw.split_once('=').ok_or(ForecastLinkError::Syntax)?;
        let (
            Ok(schedule @ ForecastSource::Scheduled { .. }),
            Ok(document @ ForecastSource::OpenItem { .. }),
        ) = (occurrence.parse(), item.parse())
        else {
            return Err(ForecastLinkError::Syntax);
        };
        Ok(Self { schedule, document })
    }
}

impl TryFrom<String> for ForecastLink {
    type Error = ForecastLinkError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<ForecastLink> for String {
    fn from(link: ForecastLink) -> Self {
        link.to_string()
    }
}

impl fmt::Display for ForecastLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.schedule, self.document)
    }
}

#[derive(Debug)]
pub(crate) enum ForecastLinkError {
    Syntax,
    DuplicateSchedule(String),
    ScheduleUnavailable(String),
    ItemUnavailable(String),
    InvalidCoverage(String),
}

impl fmt::Display for ForecastLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str(
                "forecast link must use scheduled:TEMPLATE:YYYY-MM-DD=open-item:DOCUMENT",
            ),
            Self::DuplicateSchedule(id) => write!(f, "forecast links reuse occurrence {id}"),
            Self::ScheduleUnavailable(id) => write!(
                f,
                "forecast link requires a projected cash occurrence in the forecast: {id}"
            ),
            Self::ItemUnavailable(id) => write!(
                f,
                "forecast link requires an open document in the forecast: {id}"
            ),
            Self::InvalidCoverage(link) => write!(
                f,
                "scheduled cash must have the open item's direction and fit its remaining balance: {link}"
            ),
        }
    }
}

impl std::error::Error for ForecastLinkError {}

impl From<ForecastLinkError> for CliError {
    fn from(error: ForecastLinkError) -> Self {
        Self::Validation(error.to_string())
    }
}

#[derive(Serialize)]
pub(super) struct ConfirmedLink {
    pub(super) link: ForecastLink,
    #[serde(flatten)]
    pub(super) covered: MoneyField,
}

pub(super) fn apply(
    movements: &[super::ForecastEvent<ForecastSource>],
    open_items: &mut Events,
    links: &[ForecastLink],
) -> Result<Vec<ConfirmedLink>, CliError> {
    if links.is_empty() {
        return Ok(Vec::new());
    }
    let scheduled: BTreeMap<_, _> = movements
        .iter()
        .filter(|event| matches!(event.source, ForecastSource::Scheduled { .. }))
        .map(|event| (&event.source, event.movement))
        .collect();
    let item_indexes: BTreeMap<_, _> = open_items
        .iter()
        .enumerate()
        .map(|(index, event)| (event.source.clone(), index))
        .collect();
    let mut used = BTreeSet::new();
    let mut confirmed = Vec::with_capacity(links.len());
    for link in links {
        if !used.insert(&link.schedule) {
            return Err(ForecastLinkError::DuplicateSchedule(link.schedule.to_string()).into());
        }
        let covered = *scheduled
            .get(&link.schedule)
            .ok_or_else(|| ForecastLinkError::ScheduleUnavailable(link.schedule.to_string()))?;
        let index = *item_indexes
            .get(&link.document)
            .ok_or_else(|| ForecastLinkError::ItemUnavailable(link.document.to_string()))?;
        let remaining = &mut open_items[index].movement;
        let (scheduled_minor, open_minor) = (
            covered.amount().minor_units(),
            remaining.amount().minor_units(),
        );
        if covered.currency() != remaining.currency()
            || scheduled_minor.signum() != open_minor.signum()
            || scheduled_minor.unsigned_abs() > open_minor.unsigned_abs()
        {
            return Err(ForecastLinkError::InvalidCoverage(link.to_string()).into());
        }
        *remaining = remaining.checked_sub(covered).map_err(super::validation)?;
        confirmed.push(ConfirmedLink {
            link: link.clone(),
            covered: MoneyField::new("covered", covered),
        });
    }
    open_items.retain(|event| !event.movement.is_zero());
    Ok(confirmed)
}
