use core::fmt;

/// Error type for monetary amount operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmountError {
    /// Arithmetic operation resulted in overflow.
    Overflow,
    /// Arithmetic operation resulted in underflow.
    Underflow,
}

impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => write!(f, "amount overflow"),
            Self::Underflow => write!(f, "amount underflow"),
        }
    }
}

impl std::error::Error for AmountError {}

/// A monetary amount stored as an integer count of minor currency units.
///
/// For example, $12.34 USD is stored as `1234` (cents).
/// Supports amounts up to approximately +/- 1.7 * 10^38 in minor units,
/// which is vastly more than any real-world monetary value.
///
/// # Design Rationale
///
/// Using `i128` in minor units instead of floating-point ensures:
/// - Exact arithmetic with no rounding errors
/// - Deterministic results across platforms
/// - Correct behavior for financial calculations
///
/// # Arithmetic
///
/// Use [`Amount::checked_add`], [`Amount::checked_sub`], and
/// [`Amount::checked_mul`] to detect values outside the representable range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount(i128);

impl Amount {
    /// An amount of zero.
    pub const ZERO: Self = Self(0);

    /// Creates a new `Amount` from a count of minor currency units.
    #[must_use]
    pub const fn new(minor_units: i128) -> Self {
        Self(minor_units)
    }

    /// Returns the raw minor-unit value.
    #[must_use]
    pub const fn minor_units(self) -> i128 {
        self.0
    }

    /// Returns `true` if this amount is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if this amount is strictly positive.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Returns `true` if this amount is strictly negative.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Checked absolute value. Returns `None` when the magnitude cannot be represented.
    #[must_use]
    pub const fn checked_abs(self) -> Option<Self> {
        match self.0.checked_abs() {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Checked negation. Returns `None` when the result cannot be represented.
    #[must_use]
    pub const fn checked_neg(self) -> Option<Self> {
        match self.0.checked_neg() {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Checked addition. Returns `None` outside the representable range.
    #[must_use]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Checked subtraction. Returns `None` outside the representable range.
    #[must_use]
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Checked multiplication by a scalar. Returns `None` on overflow.
    #[must_use]
    pub const fn checked_mul(self, rhs: i128) -> Option<Self> {
        match self.0.checked_mul(rhs) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Formats this amount as a decimal string with the given number of
    /// decimal places.
    ///
    /// # Examples
    ///
    /// ```
    /// use beankeeper::types::Amount;
    ///
    /// assert_eq!(Amount::new(1234).format_decimal(2), "12.34");
    /// assert_eq!(Amount::new(-50).format_decimal(2), "-0.50");
    /// assert_eq!(Amount::new(1234).format_decimal(0), "1234");
    /// ```
    #[must_use]
    pub fn format_decimal(self, decimal_places: u8) -> String {
        if decimal_places == 0 {
            return self.0.to_string();
        }

        let width = usize::from(decimal_places);
        let magnitude = self.0.unsigned_abs().to_string();
        let sign = if self.is_negative() { "-" } else { "" };
        if magnitude.len() <= width {
            return format!("{sign}0.{magnitude:0>width$}");
        }
        let (whole, fraction) = magnitude.split_at(magnitude.len() - width);
        format!("{sign}{whole}.{fraction}")
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for Amount {
    fn from(value: i64) -> Self {
        Self(i128::from(value))
    }
}

impl From<i128> for Amount {
    fn from(value: i128) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_amount_is_zero() {
        assert!(Amount::ZERO.is_zero());
    }

    #[test]
    fn positive_amount_is_not_negative() {
        let a = Amount::new(100);
        assert!(a.is_positive());
        assert!(!a.is_negative());
        assert!(!a.is_zero());
    }

    #[test]
    fn negative_amount_properties() {
        let a = Amount::new(-100);
        assert!(a.is_negative());
        assert!(!a.is_positive());
    }

    #[test]
    fn checked_add_two_amounts() {
        let a = Amount::new(100);
        let b = Amount::new(200);
        assert_eq!(a.checked_add(b), Some(Amount::new(300)));
    }

    #[test]
    fn checked_add_overflow_returns_none() {
        let a = Amount::new(i128::MAX);
        assert!(a.checked_add(Amount::new(1)).is_none());
    }

    #[test]
    fn sub_two_amounts() {
        let a = Amount::new(300);
        let b = Amount::new(100);
        assert_eq!(a.checked_sub(b), Some(Amount::new(200)));
    }

    #[test]
    fn negate_positive_becomes_negative() {
        let a = Amount::new(100);
        assert_eq!(a.checked_neg(), Some(Amount::new(-100)));
    }

    #[test]
    fn negate_zero_stays_zero() {
        assert_eq!(Amount::ZERO.checked_neg(), Some(Amount::ZERO));
    }

    #[test]
    fn abs_of_negative() {
        assert_eq!(Amount::new(-50).checked_abs(), Some(Amount::new(50)));
    }

    #[test]
    fn abs_of_positive() {
        assert_eq!(Amount::new(50).checked_abs(), Some(Amount::new(50)));
    }

    #[test]
    fn format_decimal_two_places() {
        assert_eq!(Amount::new(1234).format_decimal(2), "12.34");
    }

    #[test]
    fn format_decimal_zero_places() {
        assert_eq!(Amount::new(1234).format_decimal(0), "1234");
    }

    #[test]
    fn format_decimal_negative() {
        assert_eq!(Amount::new(-50).format_decimal(2), "-0.50");
    }

    #[test]
    fn format_decimal_three_places() {
        assert_eq!(Amount::new(1234).format_decimal(3), "1.234");
    }

    #[test]
    fn signed_minimum_formats_exactly_and_rejects_unrepresentable_magnitude() {
        let minimum = Amount::new(i128::MIN);
        assert_eq!(minimum.checked_abs(), None);
        assert_eq!(minimum.checked_neg(), None);
        assert_eq!(
            minimum.format_decimal(2),
            "-1701411834604692317316873037158841057.28"
        );
        assert_eq!(
            minimum.format_decimal(40),
            "-0.0170141183460469231731687303715884105728"
        );
    }

    #[test]
    fn default_is_zero() {
        assert_eq!(Amount::default(), Amount::ZERO);
    }

    #[test]
    fn ordering_is_numeric() {
        assert!(Amount::new(100) < Amount::new(200));
        assert!(Amount::new(-1) < Amount::new(0));
    }

    #[test]
    fn display_shows_raw_value() {
        assert_eq!(format!("{}", Amount::new(1234)), "1234");
        assert_eq!(format!("{}", Amount::new(-50)), "-50");
    }

    #[test]
    fn from_i64() {
        let a: Amount = 42_i64.into();
        assert_eq!(a, Amount::new(42));
    }

    #[test]
    fn from_i128() {
        let a: Amount = 42_i128.into();
        assert_eq!(a, Amount::new(42));
    }

    #[test]
    fn mul_by_scalar() {
        assert_eq!(Amount::new(100).checked_mul(3), Some(Amount::new(300)));
    }

    #[test]
    fn checked_mul_overflow_returns_none() {
        assert!(Amount::new(i128::MAX).checked_mul(2).is_none());
    }

    #[test]
    fn checked_sub_underflow_returns_none() {
        assert!(Amount::new(i128::MIN).checked_sub(Amount::new(1)).is_none());
    }
}
