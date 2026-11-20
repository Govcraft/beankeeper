//! Exact share quantities and pure cost-basis calculations.

use core::{fmt, str::FromStr};

/// Error validating investment inputs or calculating their monetary values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvestmentError {
    /// A trade quantity was not a strictly positive plain decimal.
    InvalidQuantity,
    /// A quantity contained more than eight fractional digits.
    QuantityPrecision,
    /// Arithmetic exceeded the supported integer or monetary range.
    Overflow,
    /// A quantity subtraction or disposal exceeded the available shares.
    InsufficientQuantity,
    /// A price or cost basis was negative.
    NegativeMoney,
    /// Cost allocation requires positive available and disposed quantities.
    EmptyDisposal,
    /// A lot-selection method was not FIFO, LIFO, or specific.
    InvalidMethod,
}

impl fmt::Display for InvestmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidQuantity => "quantity must be a positive plain decimal",
            Self::QuantityPrecision => "quantity supports at most eight decimal places",
            Self::Overflow => "investment quantity or monetary amount overflow",
            Self::InsufficientQuantity => "insufficient shares for disposal",
            Self::NegativeMoney => "price and cost basis must not be negative",
            Self::EmptyDisposal => "cost allocation requires positive quantities",
            Self::InvalidMethod => "lot method must be fifo, lifo, or specific",
        })
    }
}

impl std::error::Error for InvestmentError {}

/// A nonnegative share quantity with exactly eight decimal places of precision.
///
/// Parsing requires a positive quantity for trade inputs. [`Self::ZERO`] and
/// [`Self::from_atoms`] also represent exhausted lots and empty holdings.
/// No floating-point conversions or saturating arithmetic are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Quantity(i128);

impl Quantity {
    /// Number of indivisible quantity atoms in one share.
    pub const SCALE: i128 = 100_000_000;

    /// No shares, used for exhausted lots and aggregate holdings.
    pub const ZERO: Self = Self(0);

    /// Constructs a quantity from units of one hundred millionth of a share.
    ///
    /// # Errors
    /// Returns [`InvestmentError::InvalidQuantity`] for negative atoms.
    pub const fn from_atoms(atoms: i128) -> Result<Self, InvestmentError> {
        if atoms < 0 {
            Err(InvestmentError::InvalidQuantity)
        } else {
            Ok(Self(atoms))
        }
    }

    /// Returns the exact integer count of quantity atoms.
    #[must_use]
    pub const fn atoms(self) -> i128 {
        self.0
    }

    /// Returns whether no shares remain.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Adds quantities without silently saturating.
    ///
    /// # Errors
    /// Returns [`InvestmentError::Overflow`] if the sum exceeds `i128::MAX`.
    pub const fn checked_add(self, other: Self) -> Result<Self, InvestmentError> {
        match self.0.checked_add(other.0) {
            Some(atoms) => Ok(Self(atoms)),
            None => Err(InvestmentError::Overflow),
        }
    }

    /// Subtracts shares, permitting a zero remainder.
    ///
    /// # Errors
    /// Returns [`InvestmentError::InsufficientQuantity`] if `other` exceeds self.
    pub const fn checked_sub(self, other: Self) -> Result<Self, InvestmentError> {
        if other.0 > self.0 {
            Err(InvestmentError::InsufficientQuantity)
        } else {
            Ok(Self(self.0 - other.0))
        }
    }
}

impl FromStr for Quantity {
    type Err = InvestmentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = value
            .split_once('.')
            .map_or((value, None), |(w, f)| (w, Some(f)));
        if whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(InvestmentError::InvalidQuantity);
        }
        let fraction = fraction.unwrap_or("");
        if fraction.len() > 8 {
            return Err(InvestmentError::QuantityPrecision);
        }
        let whole = whole
            .parse::<i128>()
            .map_err(|_| InvestmentError::Overflow)?;
        let mut fractional_atoms = 0_i128;
        for digit in fraction.bytes() {
            fractional_atoms = fractional_atoms * 10 + i128::from(digit - b'0');
        }
        for _ in fraction.len()..8 {
            fractional_atoms *= 10;
        }
        let atoms = whole
            .checked_mul(Self::SCALE)
            .and_then(|v| v.checked_add(fractional_atoms))
            .ok_or(InvestmentError::Overflow)?;
        if atoms == 0 {
            return Err(InvestmentError::InvalidQuantity);
        }
        Ok(Self(atoms))
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / Self::SCALE;
        let fraction = self.0 % Self::SCALE;
        if fraction == 0 {
            write!(f, "{whole}")
        } else {
            let fraction = format!("{fraction:08}");
            write!(f, "{whole}.{}", fraction.trim_end_matches('0'))
        }
    }
}

/// Selection order for disposing of security lots.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LotMethod {
    /// Acquire-date ascending, with the oldest lots consumed first.
    #[default]
    Fifo,
    /// Acquire-date descending, with the newest lots consumed first.
    Lifo,
    /// Dispose only of explicitly selected lots.
    Specific,
}

impl fmt::Display for LotMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fifo => "fifo",
            Self::Lifo => "lifo",
            Self::Specific => "specific",
        })
    }
}

impl FromStr for LotMethod {
    type Err = InvestmentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fifo" => Ok(Self::Fifo),
            "lifo" => Ok(Self::Lifo),
            "specific" => Ok(Self::Specific),
            _ => Err(InvestmentError::InvalidMethod),
        }
    }
}

/// Values shares at an integer price per whole share in currency minor units.
///
/// Rounds to the nearest minor unit, with exact halves rounded up. A zero price
/// or quantity produces a zero valuation.
///
/// # Errors
/// Rejects negative prices and integer or monetary overflow.
pub fn market_value(quantity: Quantity, price_minor: i64) -> Result<i64, InvestmentError> {
    if price_minor < 0 {
        return Err(InvestmentError::NegativeMoney);
    }
    let numerator = quantity
        .atoms()
        .checked_mul(i128::from(price_minor))
        .ok_or(InvestmentError::Overflow)?;
    rounded_ratio(numerator, Quantity::SCALE)
}

/// Allocates a remaining lot's cost basis to shares being disposed.
///
/// A partial disposal receives its proportion of the remaining basis, rounded
/// half up to a currency minor unit. A whole-lot disposal receives all residual
/// basis, so repeated disposals conserve the original cost exactly.
///
/// # Errors
/// Rejects negative basis, zero available or disposed shares, overselling, and
/// arithmetic overflow.
pub fn allocate_cost(
    remaining: Quantity,
    remaining_basis: i64,
    sold: Quantity,
) -> Result<i64, InvestmentError> {
    if remaining_basis < 0 {
        return Err(InvestmentError::NegativeMoney);
    }
    if remaining.is_zero() || sold.is_zero() {
        return Err(InvestmentError::EmptyDisposal);
    }
    if sold > remaining {
        return Err(InvestmentError::InsufficientQuantity);
    }
    if sold == remaining {
        return Ok(remaining_basis);
    }
    let numerator = sold
        .atoms()
        .checked_mul(i128::from(remaining_basis))
        .ok_or(InvestmentError::Overflow)?;
    rounded_ratio(numerator, remaining.atoms())
}

fn rounded_ratio(numerator: i128, denominator: i128) -> Result<i64, InvestmentError> {
    let whole = numerator / denominator;
    let remainder = numerator % denominator;
    // Compare against ceil(denominator / 2) without doubling the remainder.
    let round_up = remainder >= denominator / 2 + denominator % 2;
    let rounded = whole
        .checked_add(i128::from(round_up))
        .ok_or(InvestmentError::Overflow)?;
    i64::try_from(rounded).map_err(|_| InvestmentError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantity(value: &str) -> Quantity {
        value.parse().unwrap()
    }

    #[test]
    fn quantities_preserve_eight_decimal_places_and_format_canonically() {
        for (input, output, atoms) in [
            ("1", "1", 100_000_000),
            ("0001.2500", "1.25", 125_000_000),
            ("0.00000001", "0.00000001", 1),
            ("12.34567890", "12.3456789", 1_234_567_890),
        ] {
            let shares = quantity(input);
            assert_eq!(shares.atoms(), atoms);
            assert_eq!(shares.to_string(), output);
            assert_eq!(quantity(&shares.to_string()), shares);
        }
    }

    #[test]
    fn invalid_and_zero_trade_quantities_are_rejected() {
        for invalid in [
            "",
            "0",
            "0.00000000",
            "-1",
            "+1",
            "1e2",
            " 1",
            "1 ",
            ".5",
            "1.",
            "1.2.3",
            "NaN",
        ] {
            assert_eq!(
                invalid.parse::<Quantity>(),
                Err(InvestmentError::InvalidQuantity),
                "{invalid}"
            );
        }
        assert_eq!(
            "1.000000000".parse::<Quantity>(),
            Err(InvestmentError::QuantityPrecision)
        );
    }

    #[test]
    fn quantity_limits_are_checked() {
        let maximum = Quantity::from_atoms(i128::MAX).unwrap();
        assert_eq!(quantity(&maximum.to_string()), maximum);
        assert_eq!(
            "1701411834604692317316873037158.84105728".parse::<Quantity>(),
            Err(InvestmentError::Overflow)
        );
        assert_eq!(
            "99999999999999999999999999999999999999999".parse::<Quantity>(),
            Err(InvestmentError::Overflow)
        );
        assert_eq!(
            Quantity::from_atoms(-1),
            Err(InvestmentError::InvalidQuantity)
        );
        assert_eq!(
            maximum.checked_add(Quantity::from_atoms(1).unwrap()),
            Err(InvestmentError::Overflow)
        );
    }

    #[test]
    fn quantity_arithmetic_preserves_zero_and_rejects_overselling() {
        assert_eq!(
            quantity("1.25").checked_add(quantity("0.75")),
            Ok(quantity("2"))
        );
        assert_eq!(quantity("1").checked_sub(quantity("1")), Ok(Quantity::ZERO));
        assert!(Quantity::ZERO.is_zero());
        assert_eq!(Quantity::ZERO.to_string(), "0");
        assert_eq!(
            quantity("1").checked_sub(quantity("2")),
            Err(InvestmentError::InsufficientQuantity)
        );
    }

    #[test]
    fn market_values_round_half_up_in_minor_units() {
        assert_eq!(market_value(quantity("1.25"), 1234), Ok(1543));
        assert_eq!(market_value(quantity("0.49999999"), 1), Ok(0));
        assert_eq!(market_value(quantity("0.5"), 1), Ok(1));
        assert_eq!(market_value(quantity("0.50000001"), 1), Ok(1));
        assert_eq!(market_value(Quantity::ZERO, i64::MAX), Ok(0));
        assert_eq!(market_value(quantity("1"), 0), Ok(0));
        assert_eq!(market_value(quantity("1"), i64::MAX), Ok(i64::MAX));
    }

    #[test]
    fn negative_values_and_monetary_overflows_are_rejected() {
        assert_eq!(
            market_value(quantity("1"), -1),
            Err(InvestmentError::NegativeMoney)
        );
        assert_eq!(
            market_value(quantity("2"), i64::MAX),
            Err(InvestmentError::Overflow)
        );
        let maximum = Quantity::from_atoms(i128::MAX).unwrap();
        assert_eq!(market_value(maximum, 2), Err(InvestmentError::Overflow));
        assert_eq!(
            allocate_cost(quantity("1"), -1, quantity("1")),
            Err(InvestmentError::NegativeMoney)
        );
        assert_eq!(
            allocate_cost(maximum, 2, maximum.checked_sub(quantity("1")).unwrap()),
            Err(InvestmentError::Overflow)
        );
    }

    #[test]
    fn partial_disposals_conserve_basis_and_close_the_residual() {
        let one = quantity("1");
        let mut shares = quantity("3");
        let mut basis = 100;
        let mut relieved = Vec::new();
        for _ in 0..3 {
            let cost = allocate_cost(shares, basis, one).unwrap();
            relieved.push(cost);
            basis -= cost;
            shares = shares.checked_sub(one).unwrap();
        }
        assert_eq!(relieved, [33, 34, 33]);
        assert_eq!(relieved.iter().sum::<i64>(), 100);
        assert_eq!(basis, 0);
        assert_eq!(shares, Quantity::ZERO);
        assert_eq!(allocate_cost(quantity("0.5"), 1, quantity("0.25")), Ok(1));
        assert_eq!(allocate_cost(quantity("0.25"), 0, quantity("0.25")), Ok(0));
    }

    #[test]
    fn invalid_disposals_do_not_allocate_cost() {
        assert_eq!(
            allocate_cost(Quantity::ZERO, 0, quantity("1")),
            Err(InvestmentError::EmptyDisposal)
        );
        assert_eq!(
            allocate_cost(quantity("1"), 100, Quantity::ZERO),
            Err(InvestmentError::EmptyDisposal)
        );
        assert_eq!(
            allocate_cost(quantity("1"), 100, quantity("2")),
            Err(InvestmentError::InsufficientQuantity)
        );
    }

    #[test]
    fn selection_methods_round_trip() {
        for method in [LotMethod::Fifo, LotMethod::Lifo, LotMethod::Specific] {
            assert_eq!(method.to_string().parse::<LotMethod>(), Ok(method));
        }
        assert_eq!(LotMethod::default(), LotMethod::Fifo);
        assert_eq!(
            "average".parse::<LotMethod>(),
            Err(InvestmentError::InvalidMethod)
        );
    }
}
