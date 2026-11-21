//! Exact share quantities and pure cost-basis calculations.

use core::{fmt, str::FromStr};

use num_bigint::BigUint;

use crate::types::{Money, NonNegativeMoney};

/// Error validating investment inputs or calculating their monetary values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvestmentError {
    /// A quantity was not a nonnegative plain decimal.
    InvalidQuantity,
    /// A trade quantity was zero.
    ZeroQuantity,
    /// A quantity contained more than eight fractional digits.
    QuantityPrecision,
    /// Arithmetic exceeded the supported integer or monetary range.
    Overflow,
    /// A quantity subtraction or disposal exceeded the available shares.
    InsufficientQuantity,
    /// A lot-selection method was not FIFO, LIFO, or specific.
    InvalidMethod,
}

impl fmt::Display for InvestmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidQuantity => "quantity must be a nonnegative plain decimal",
            Self::ZeroQuantity => "trade quantity must be positive",
            Self::QuantityPrecision => "quantity supports at most eight decimal places",
            Self::Overflow => "investment quantity or monetary amount overflow",
            Self::InsufficientQuantity => "insufficient shares for disposal",
            Self::InvalidMethod => "lot method must be fifo, lifo, or specific",
        })
    }
}

impl std::error::Error for InvestmentError {}

/// A nonnegative share quantity with exactly eight decimal places of precision.
///
/// Includes exhausted lots and empty holdings. Use [`PositiveQuantity`] for trades.
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

    /// Returns a positive quantity when shares remain.
    #[must_use]
    pub const fn positive(self) -> Option<PositiveQuantity> {
        if self.is_zero() {
            None
        } else {
            Some(PositiveQuantity(self))
        }
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
        Ok(Self(atoms))
    }
}

/// A strictly positive share quantity, required by purchases and disposals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveQuantity(Quantity);

impl PositiveQuantity {
    /// One whole share.
    pub const ONE: Self = Self(Quantity(Quantity::SCALE));

    /// Constructs a positive quantity from a count of share atoms.
    ///
    /// # Errors
    /// Returns [`InvestmentError::InvalidQuantity`] or [`InvestmentError::ZeroQuantity`]
    /// for invalid atoms.
    pub fn from_atoms(atoms: i128) -> Result<Self, InvestmentError> {
        Quantity::from_atoms(atoms)?.try_into()
    }

    /// Returns the exact integer count of quantity atoms.
    #[must_use]
    pub const fn atoms(self) -> i128 {
        self.0.atoms()
    }

    /// Widens the value to a quantity that may also represent zero.
    #[must_use]
    pub const fn quantity(self) -> Quantity {
        self.0
    }
}

impl TryFrom<Quantity> for PositiveQuantity {
    type Error = InvestmentError;

    fn try_from(quantity: Quantity) -> Result<Self, Self::Error> {
        quantity.positive().ok_or(InvestmentError::ZeroQuantity)
    }
}

impl From<PositiveQuantity> for Quantity {
    fn from(quantity: PositiveQuantity) -> Self {
        quantity.quantity()
    }
}

impl FromStr for PositiveQuantity {
    type Err = InvestmentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<Quantity>()?.try_into()
    }
}

impl fmt::Display for PositiveQuantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
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

/// Values shares at a price per whole share, retaining the price's currency.
///
/// Rounds to the nearest minor unit, with exact halves rounded up. A zero price
/// or quantity produces a zero valuation.
///
/// # Errors
/// Returns [`InvestmentError::Overflow`] if the rounded result overflows.
pub fn market_value(
    quantity: Quantity,
    price: NonNegativeMoney,
) -> Result<NonNegativeMoney, InvestmentError> {
    rounded_product_ratio(quantity, price, PositiveQuantity::ONE)
}

/// Allocates a remaining lot's cost basis to shares being disposed.
///
/// A partial disposal receives its proportion of the remaining basis, rounded
/// half up to a currency minor unit. A whole-lot disposal receives all residual
/// basis, so repeated disposals conserve the original cost exactly.
///
/// # Errors
/// Rejects a disposal greater than the remaining quantity or an overflowing result.
pub fn allocate_cost(
    remaining: PositiveQuantity,
    remaining_basis: NonNegativeMoney,
    sold: PositiveQuantity,
) -> Result<NonNegativeMoney, InvestmentError> {
    if sold > remaining {
        return Err(InvestmentError::InsufficientQuantity);
    }
    if sold == remaining {
        return Ok(remaining_basis);
    }
    rounded_product_ratio(sold.quantity(), remaining_basis, remaining)
}

fn rounded_product_ratio(
    quantity: Quantity,
    money: NonNegativeMoney,
    divisor: PositiveQuantity,
) -> Result<NonNegativeMoney, InvestmentError> {
    let amount = money.money();
    let minor = amount.amount().minor_units();
    let denominator = divisor.atoms();
    // Compare against ceil(denominator / 2) without doubling the remainder.
    let midpoint = denominator / 2 + denominator % 2;
    let (whole, round_up) = if let Some(numerator) = quantity.atoms().checked_mul(minor) {
        (numerator / denominator, numerator % denominator >= midpoint)
    } else {
        let numerator = BigUint::from(quantity.atoms().unsigned_abs()) * minor.unsigned_abs();
        let denominator = denominator.unsigned_abs();
        let whole =
            i128::try_from(&numerator / denominator).map_err(|_| InvestmentError::Overflow)?;
        let round_up = numerator % denominator >= BigUint::from(midpoint.unsigned_abs());
        (whole, round_up)
    };
    let rounded = whole
        .checked_add(i128::from(round_up))
        .ok_or(InvestmentError::Overflow)?;
    NonNegativeMoney::new(Money::from_minor(rounded, amount.currency()))
        .map_err(|_| InvestmentError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MoneyError, PositiveMoney};

    fn positive(value: &str) -> PositiveQuantity {
        value
            .parse()
            .unwrap_or_else(|error| panic!("test positive quantity: {error}"))
    }

    fn usd(minor: i128) -> NonNegativeMoney {
        NonNegativeMoney::new(Money::usd(minor))
            .unwrap_or_else(|error| panic!("test money: {error}"))
    }

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
            "", "-1", "+1", "1e2", " 1", "1 ", ".5", "1.", "1.2.3", "NaN",
        ] {
            assert_eq!(
                invalid.parse::<Quantity>(),
                Err(InvestmentError::InvalidQuantity),
                "{invalid}"
            );
        }
        for zero in ["0", "0.00000000"] {
            assert_eq!(zero.parse::<Quantity>(), Ok(Quantity::ZERO));
            assert_eq!(
                zero.parse::<PositiveQuantity>(),
                Err(InvestmentError::ZeroQuantity)
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
    fn market_values_round_half_up_and_retain_currency() {
        assert_eq!(market_value(quantity("1.25"), usd(1234)), Ok(usd(1543)));
        assert_eq!(market_value(quantity("0.49999999"), usd(1)), Ok(usd(0)));
        assert_eq!(market_value(quantity("0.5"), usd(1)), Ok(usd(1)));
        assert_eq!(market_value(quantity("0.50000001"), usd(1)), Ok(usd(1)));
        assert_eq!(market_value(Quantity::ZERO, usd(i128::MAX)), Ok(usd(0)));
        assert_eq!(market_value(quantity("1"), usd(0)), Ok(usd(0)));
        let euro = NonNegativeMoney::new(Money::eur(100))
            .unwrap_or_else(|error| panic!("test valuation currency: {error}"));
        assert_eq!(market_value(quantity("1"), euro), Ok(euro));
        assert!(matches!(
            euro.checked_add(usd(100)),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn only_out_of_range_results_overflow() {
        assert_eq!(
            market_value(quantity("1"), usd(i128::MAX)),
            Ok(usd(i128::MAX))
        );
        assert_eq!(
            market_value(quantity("2"), usd(i128::MAX)),
            Err(InvestmentError::Overflow)
        );
        let maximum = PositiveQuantity::from_atoms(i128::MAX)
            .unwrap_or_else(|error| panic!("test maximum quantity: {error}"));
        let half = PositiveQuantity::from_atoms(i128::MAX / 2)
            .unwrap_or_else(|error| panic!("test partial quantity: {error}"));
        assert_eq!(
            allocate_cost(maximum, usd(i128::MAX), half),
            Ok(usd(i128::MAX / 2))
        );
    }

    #[test]
    fn negative_and_zero_money_cannot_enter_positive_calculations() {
        assert_eq!(
            NonNegativeMoney::new(Money::usd(-1)),
            Err(MoneyError::NegativeAmount)
        );
        assert_eq!(
            PositiveMoney::new(Money::usd(0)),
            Err(MoneyError::ZeroAmount)
        );
        assert_eq!(usd(1).checked_sub(usd(2)), Err(MoneyError::NegativeAmount));
    }

    #[test]
    fn partial_disposals_conserve_basis_and_close_the_residual()
    -> Result<(), Box<dyn std::error::Error>> {
        let one = positive("1");
        let mut shares = quantity("3");
        let mut basis = usd(100);
        let mut relieved = Vec::new();
        for _ in 0..3 {
            let cost = allocate_cost(shares.try_into()?, basis, one)?;
            relieved.push(cost.money().amount().minor_units());
            basis = basis.checked_sub(cost)?;
            shares = shares.checked_sub(one.into())?;
        }
        assert_eq!(relieved, [33, 34, 33]);
        assert_eq!(relieved.iter().sum::<i128>(), 100);
        assert_eq!(basis, usd(0));
        assert_eq!(shares, Quantity::ZERO);
        assert_eq!(
            allocate_cost(positive("0.5"), usd(1), positive("0.25")),
            Ok(usd(1))
        );
        assert_eq!(
            allocate_cost(positive("0.25"), usd(0), positive("0.25")),
            Ok(usd(0))
        );
        Ok(())
    }

    #[test]
    fn invalid_disposals_do_not_allocate_cost() {
        assert_eq!(
            PositiveQuantity::try_from(Quantity::ZERO),
            Err(InvestmentError::ZeroQuantity)
        );
        assert_eq!(
            allocate_cost(positive("1"), usd(100), positive("2")),
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
