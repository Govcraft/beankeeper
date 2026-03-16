use core::fmt;

use super::amount::Amount;
use super::currency::Currency;

/// Error type for monetary operations involving currency.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoneyError {
    /// An operation was attempted between two different currencies.
    CurrencyMismatch {
        /// The expected currency.
        expected: Currency,
        /// The actual currency encountered.
        actual: Currency,
    },
    /// Arithmetic overflow.
    Overflow,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrencyMismatch { expected, actual } => {
                write!(f, "currency mismatch: expected {expected}, got {actual}")
            }
            Self::Overflow => write!(f, "monetary arithmetic overflow"),
        }
    }
}

impl std::error::Error for MoneyError {}

/// A monetary value combining an [`Amount`] with a [`Currency`].
///
/// Operations between different currencies are prevented by returning
/// errors from arithmetic methods rather than silently converting.
///
/// # Note on Arithmetic
///
/// `Money` does **not** implement [`std::ops::Add`] or [`std::ops::Sub`]
/// because those traits cannot return `Result`. Use [`checked_add`] and
/// [`checked_sub`] instead, which enforce currency matching.
///
/// [`checked_add`]: Money::checked_add
/// [`checked_sub`]: Money::checked_sub
///
/// # Examples
///
/// ```
/// use beankeeper::types::{Money, Currency};
///
/// let a = Money::usd(500);
/// let b = Money::usd(300);
/// let total = a.checked_add(b).unwrap();
/// assert_eq!(total, Money::usd(800));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money {
    amount: Amount,
    currency: Currency,
}

impl Money {
    /// Creates a new `Money` value from an [`Amount`] and [`Currency`].
    #[must_use]
    pub const fn new(amount: Amount, currency: Currency) -> Self {
        Self { amount, currency }
    }

    /// Creates a `Money` value from minor units and a currency.
    #[must_use]
    pub const fn from_minor(minor_units: i128, currency: Currency) -> Self {
        Self {
            amount: Amount::new(minor_units),
            currency,
        }
    }

    /// Creates a USD amount from cents.
    #[must_use]
    pub const fn usd(cents: i128) -> Self {
        Self::from_minor(cents, Currency::USD)
    }

    /// Creates a EUR amount from cents.
    #[must_use]
    pub const fn eur(cents: i128) -> Self {
        Self::from_minor(cents, Currency::EUR)
    }

    /// Creates a GBP amount from pence.
    #[must_use]
    pub const fn gbp(pence: i128) -> Self {
        Self::from_minor(pence, Currency::GBP)
    }

    /// Creates a JPY amount (no minor units).
    #[must_use]
    pub const fn jpy(yen: i128) -> Self {
        Self::from_minor(yen, Currency::JPY)
    }

    /// Returns the [`Amount`] component.
    #[must_use]
    pub const fn amount(&self) -> Amount {
        self.amount
    }

    /// Returns the [`Currency`] component.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Returns `true` if the amount is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.amount.is_zero()
    }

    /// Returns `true` if the amount is strictly positive.
    #[must_use]
    pub const fn is_positive(&self) -> bool {
        self.amount.is_positive()
    }

    /// Returns `true` if the amount is strictly negative.
    #[must_use]
    pub const fn is_negative(&self) -> bool {
        self.amount.is_negative()
    }

    /// Checked addition. Returns an error if currencies don't match or on overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] if the currencies differ,
    /// or [`MoneyError::Overflow`] if the addition overflows.
    pub fn checked_add(self, rhs: Self) -> Result<Self, MoneyError> {
        if self.currency != rhs.currency {
            return Err(MoneyError::CurrencyMismatch {
                expected: self.currency,
                actual: rhs.currency,
            });
        }

        self.amount
            .checked_add(rhs.amount)
            .map(|amount| Self::new(amount, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Checked subtraction. Returns an error if currencies don't match or on overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] if the currencies differ,
    /// or [`MoneyError::Overflow`] if the subtraction overflows.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, MoneyError> {
        if self.currency != rhs.currency {
            return Err(MoneyError::CurrencyMismatch {
                expected: self.currency,
                actual: rhs.currency,
            });
        }

        self.amount
            .checked_sub(rhs.amount)
            .map(|amount| Self::new(amount, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Returns this amount with its sign negated.
    #[must_use]
    pub fn negate(self) -> Self {
        Self::new(-self.amount, self.currency)
    }

    /// Returns the absolute value of this amount.
    #[must_use]
    pub fn abs(self) -> Self {
        Self::new(self.amount.abs(), self.currency)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let decimal_places = self.currency.minor_units();
        let formatted = self.amount.format_decimal(decimal_places);
        write!(f, "{} {formatted}", self.currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_usd_money() {
        let m = Money::usd(500);
        assert_eq!(m.amount(), Amount::new(500));
        assert_eq!(m.currency(), Currency::USD);
    }

    #[test]
    fn add_same_currency_succeeds() {
        let a = Money::usd(100);
        let b = Money::usd(200);
        let result = a.checked_add(b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap_or(Money::usd(0)), Money::usd(300));
    }

    #[test]
    fn add_different_currency_returns_error() {
        let a = Money::usd(100);
        let b = Money::eur(100);
        let result = a.checked_add(b);
        assert!(matches!(result, Err(MoneyError::CurrencyMismatch { .. })));
    }

    #[test]
    fn sub_same_currency_succeeds() {
        let a = Money::usd(300);
        let b = Money::usd(100);
        let result = a.checked_sub(b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap_or(Money::usd(0)), Money::usd(200));
    }

    #[test]
    fn sub_different_currency_returns_error() {
        let a = Money::usd(300);
        let b = Money::gbp(100);
        let result = a.checked_sub(b);
        assert!(matches!(result, Err(MoneyError::CurrencyMismatch { .. })));
    }

    #[test]
    fn negate_positive_money() {
        let m = Money::usd(100);
        let neg = m.negate();
        assert_eq!(neg.amount(), Amount::new(-100));
        assert_eq!(neg.currency(), Currency::USD);
    }

    #[test]
    fn abs_of_negative_money() {
        let m = Money::usd(-100);
        assert_eq!(m.abs(), Money::usd(100));
    }

    #[test]
    fn display_usd() {
        let m = Money::usd(1234);
        assert_eq!(format!("{m}"), "USD 12.34");
    }

    #[test]
    fn display_jpy() {
        let m = Money::jpy(1234);
        assert_eq!(format!("{m}"), "JPY 1234");
    }

    #[test]
    fn display_negative() {
        let m = Money::usd(-50);
        assert_eq!(format!("{m}"), "USD -0.50");
    }

    #[test]
    fn zero_money_is_zero() {
        let m = Money::usd(0);
        assert!(m.is_zero());
        assert!(!m.is_positive());
        assert!(!m.is_negative());
    }

    #[test]
    fn positive_money() {
        let m = Money::usd(100);
        assert!(m.is_positive());
        assert!(!m.is_negative());
        assert!(!m.is_zero());
    }

    #[test]
    fn from_minor() {
        let m = Money::from_minor(500, Currency::EUR);
        assert_eq!(m, Money::eur(500));
    }

    #[test]
    fn new_constructor() {
        let m = Money::new(Amount::new(100), Currency::GBP);
        assert_eq!(m, Money::gbp(100));
    }

    #[test]
    fn overflow_on_add() {
        let a = Money::new(Amount::new(i128::MAX), Currency::USD);
        let b = Money::usd(1);
        assert!(matches!(a.checked_add(b), Err(MoneyError::Overflow)));
    }

    #[test]
    fn error_display_currency_mismatch() {
        let err = MoneyError::CurrencyMismatch {
            expected: Currency::USD,
            actual: Currency::EUR,
        };
        assert!(format!("{err}").contains("currency mismatch"));
    }

    #[test]
    fn error_display_overflow() {
        assert!(format!("{}", MoneyError::Overflow).contains("overflow"));
    }
}
