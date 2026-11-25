use core::{fmt, num::NonZeroU128, str::FromStr};

use num_bigint::BigInt;

use super::{Currency, Money};

/// A positive decimal number of functional major units per native major unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExchangeRate {
    coefficient: NonZeroU128,
    scale: u32,
}

/// A malformed rate or a conversion outside the supported amount range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeRateError {
    /// Expected a positive decimal with at most 38 fractional digits.
    InvalidRate,
    /// The rounded result cannot be represented as money.
    Overflow,
}

impl fmt::Display for ExchangeRateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRate => "exchange rate must be a positive decimal with at most 38 fractional digits and a 128-bit coefficient",
            Self::Overflow => "converted amount exceeds the supported range",
        })
    }
}

impl std::error::Error for ExchangeRateError {}

impl FromStr for ExchangeRate {
    type Err = ExchangeRateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = match value.split_once('.') {
            Some((_, "")) => return Err(ExchangeRateError::InvalidRate),
            Some(pair) => pair,
            None => (value, ""),
        };
        let fraction = fraction.trim_end_matches('0');
        let scale = u32::try_from(fraction.len()).map_err(|_| ExchangeRateError::InvalidRate)?;
        if whole.is_empty()
            || scale > 38
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|digit| digit.is_ascii_digit())
        {
            return Err(ExchangeRateError::InvalidRate);
        }
        let coefficient = format!("{whole}{fraction}")
            .parse()
            .map_err(|_| ExchangeRateError::InvalidRate)?;
        Ok(Self { coefficient, scale })
    }
}

impl fmt::Display for ExchangeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            self.coefficient.fmt(f)
        } else {
            let divisor = 10_u128.pow(self.scale);
            let value = self.coefficient.get();
            write!(
                f,
                "{}.{:0width$}",
                value / divisor,
                value % divisor,
                width = self.scale as usize
            )
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for ExchangeRate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ExchangeRate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Exact conversion evidence. The rounded functional amount can only be
/// produced by converting the retained native amount at the retained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertedMoney {
    native: Money,
    functional: Money,
    rate: ExchangeRate,
}

impl ConvertedMoney {
    /// Amount in its original currency.
    #[must_use]
    pub const fn native(self) -> Money {
        self.native
    }

    /// Converted amount, rounded to functional minor units.
    #[must_use]
    pub const fn functional(self) -> Money {
        self.functional
    }

    /// Rate applied to the native amount.
    #[must_use]
    pub const fn rate(self) -> ExchangeRate {
        self.rate
    }
}

impl ExchangeRate {
    /// Converts major-unit rates to minor units, rounding ties to even.
    ///
    /// # Errors
    /// Returns an overflow error if the rounded result exceeds the money range.
    pub fn convert(
        self,
        native: Money,
        functional: Currency,
    ) -> Result<ConvertedMoney, ExchangeRateError> {
        let ten = BigInt::from(10);
        let minor = native.amount().minor_units();
        let numerator = BigInt::from(minor.unsigned_abs())
            * self.coefficient.get()
            * ten.pow(u32::from(functional.minor_units()));
        let denominator = ten.pow(u32::from(native.currency().minor_units()) + self.scale);
        let mut rounded = &numerator / &denominator;
        let twice_remainder = (numerator % &denominator) * 2_u8;
        if twice_remainder > denominator || (twice_remainder == denominator && rounded.bit(0)) {
            rounded += 1_u8;
        }
        if minor < 0 {
            rounded = -rounded;
        }
        let converted = i128::try_from(rounded).map_err(|_| ExchangeRateError::Overflow)?;
        Ok(ConvertedMoney {
            native,
            functional: Money::from_minor(converted, functional),
            rate: self,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_major_unit_rates_and_half_even_rounding() -> Result<(), ExchangeRateError> {
        let rate: ExchangeRate = "0.0005".parse()?;
        for (yen, fils) in [(1, 0), (3, 2), (-3, -2), (5, 2)] {
            let conversion = rate.convert(
                Money::jpy(yen),
                "KWD".parse().unwrap_or_else(|e| panic!("{e}")),
            )?;
            assert_eq!(conversion.functional().amount().minor_units(), fils);
            assert_eq!(conversion.native(), Money::jpy(yen));
            assert_eq!(conversion.rate(), rate);
        }
        assert_eq!("0.8500".parse::<ExchangeRate>()?.to_string(), "0.85");
        for invalid in ["0", "-1", "0.0", "1e2", "NaN", "1..2"] {
            assert_eq!(
                invalid.parse::<ExchangeRate>(),
                Err(ExchangeRateError::InvalidRate)
            );
        }
        assert_eq!(
            "2".parse::<ExchangeRate>()?
                .convert(Money::eur(i128::MAX), Currency::GBP),
            Err(ExchangeRateError::Overflow)
        );
        Ok(())
    }
}
