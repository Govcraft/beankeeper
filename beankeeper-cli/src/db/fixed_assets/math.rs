use std::{fmt, num::NonZeroU16, str::FromStr};

use chrono::{Datelike, Months, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AssetError {
    InvalidLife,
    DateOverflow,
    InvalidBasis,
    Overflow,
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidLife => "useful life must be between 1 and 1200 months",
            Self::DateOverflow => "depreciation date is outside the supported calendar",
            Self::InvalidBasis => {
                "cost must be positive and accumulated depreciation plus salvage cannot exceed cost"
            }
            Self::Overflow => "fixed asset amount exceeds the supported range",
        })
    }
}

impl std::error::Error for AssetError {}

/// Remaining useful life over which the asset will be depreciated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct UsefulLifeMonths(NonZeroU16);

impl UsefulLifeMonths {
    pub(super) const fn get(self) -> u16 {
        self.0.get()
    }

    pub(super) fn checked(value: u16) -> Result<Self, AssetError> {
        NonZeroU16::new(value)
            .filter(|v| v.get() <= 1200)
            .map(Self)
            .ok_or(AssetError::InvalidLife)
    }
}

impl<'de> serde::Deserialize<'de> for UsefulLifeMonths {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        Self::checked(u16::deserialize(decoder)?).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<u16> for UsefulLifeMonths {
    type Error = crate::error::CliError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Ok(Self::checked(value)?)
    }
}

impl FromStr for UsefulLifeMonths {
    type Err = crate::error::CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u16>()
            .ok()
            .and_then(|v| Self::checked(v).ok())
            .ok_or_else(|| AssetError::InvalidLife.into())
    }
}

impl From<AssetError> for crate::error::CliError {
    fn from(value: AssetError) -> Self {
        Self::Validation(value.to_string())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AssetBasis {
    pub(super) cost: i64,
    pub(super) opening_depreciation: i64,
    pub(super) salvage: i64,
}

impl AssetBasis {
    fn depreciable(self) -> Result<i64, AssetError> {
        if self.cost <= 0 || self.opening_depreciation < 0 || self.salvage < 0 {
            return Err(AssetError::InvalidBasis);
        }
        self.cost
            .checked_sub(self.opening_depreciation)
            .and_then(|value| value.checked_sub(self.salvage))
            .filter(|value| *value >= 0)
            .ok_or(AssetError::InvalidBasis)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Charge {
    pub(super) date: NaiveDate,
    pub(super) amount: i64,
}

/// Charge each complete calendar month after starting service.
pub(super) fn depreciation_schedule(
    basis: AssetBasis,
    start: NaiveDate,
    life: UsefulLifeMonths,
) -> Result<Vec<Charge>, AssetError> {
    let total = i128::from(basis.depreciable()?);
    let months = i128::from(life.get());
    let first = start.with_day(1).ok_or(AssetError::DateOverflow)?;
    let partial = u32::from(start.day() != 1);
    (1..=u32::from(life.get()))
        .filter_map(|month| {
            let month = i128::from(month);
            let amount = (total * month / months) - (total * (month - 1) / months);
            (amount != 0).then_some((month, amount))
        })
        .map(|(month, amount)| {
            let offset = u32::try_from(month).map_err(|_| AssetError::DateOverflow)? + partial;
            let date = first
                .checked_add_months(Months::new(offset))
                .and_then(|date| date.pred_opt())
                .ok_or(AssetError::DateOverflow)?;
            Ok(Charge {
                date,
                amount: i64::try_from(amount).map_err(|_| AssetError::Overflow)?,
            })
        })
        .collect()
}

pub(super) fn disposal_gain(cost: i64, accumulated: i64, proceeds: i64) -> Result<i64, AssetError> {
    if cost <= 0 || accumulated < 0 || accumulated > cost || proceeds < 0 {
        return Err(AssetError::InvalidBasis);
    }
    Ok(proceeds - (cost - accumulated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_month_leap_year_and_odd_cents_leave_salvage() -> Result<(), Box<dyn std::error::Error>> {
        let terms = AssetBasis {
            cost: 106,
            opening_depreciation: 1,
            salvage: 5,
        };
        let start = "2024-01-15".parse()?;
        let charges = depreciation_schedule(terms, start, UsefulLifeMonths::checked(3)?)?;
        assert_eq!(
            charges,
            [
                Charge {
                    date: "2024-02-29".parse()?,
                    amount: 33
                },
                Charge {
                    date: "2024-03-31".parse()?,
                    amount: 33
                },
                Charge {
                    date: "2024-04-30".parse()?,
                    amount: 34
                },
            ]
        );
        assert_eq!(disposal_gain(106, 101, 6)?, 1);
        Ok(())
    }

    #[test]
    fn fully_written_down_asset_has_no_charge_and_can_be_scrapped()
    -> Result<(), Box<dyn std::error::Error>> {
        let terms = AssetBasis {
            cost: 100,
            opening_depreciation: 89,
            salvage: 11,
        };
        let charges =
            depreciation_schedule(terms, "2026-01-01".parse()?, UsefulLifeMonths::checked(12)?)?;
        assert_eq!(charges, []);
        assert_eq!(disposal_gain(100, 89, 0)?, -11);
        Ok(())
    }
}
