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
    /// This monetary value must be nonnegative.
    NegativeAmount,
    /// This monetary value must be strictly positive.
    ZeroAmount,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrencyMismatch { expected, actual } => {
                write!(f, "currency mismatch: expected {expected}, got {actual}")
            }
            Self::Overflow => write!(f, "monetary arithmetic overflow"),
            Self::NegativeAmount => write!(f, "monetary amount must not be negative"),
            Self::ZeroAmount => write!(f, "monetary amount must be positive"),
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

    /// Creates a MXN amount from centavos.
    #[must_use]
    pub const fn mxn(centavos: i128) -> Self {
        Self::from_minor(centavos, Currency::MXN)
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

    /// Negates this monetary value.
    /// # Errors
    /// Returns [`MoneyError::Overflow`] when the result cannot be represented.
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        self.amount
            .checked_neg()
            .map(|amount| Self::new(amount, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Computes the absolute value.
    /// # Errors
    /// Returns [`MoneyError::Overflow`] when the magnitude cannot be represented.
    pub fn checked_abs(self) -> Result<Self, MoneyError> {
        self.amount
            .checked_abs()
            .map(|amount| Self::new(amount, self.currency))
            .ok_or(MoneyError::Overflow)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let decimal_places = self.currency.minor_units();
        let formatted = self.amount.format_decimal(decimal_places);
        write!(f, "{} {formatted}", self.currency)
    }
}

/// Money known to be zero or positive, retaining its currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonNegativeMoney(Money);

impl NonNegativeMoney {
    /// Validates that the amount is nonnegative.
    ///
    /// # Errors
    /// Returns [`MoneyError::NegativeAmount`] for negative money.
    pub const fn new(money: Money) -> Result<Self, MoneyError> {
        if money.is_negative() {
            Err(MoneyError::NegativeAmount)
        } else {
            Ok(Self(money))
        }
    }

    /// Returns zero in the supplied currency.
    #[must_use]
    pub const fn zero(currency: Currency) -> Self {
        Self(Money::new(Amount::ZERO, currency))
    }

    /// Returns the underlying monetary value.
    #[must_use]
    pub const fn money(self) -> Money {
        self.0
    }

    /// Returns a strictly positive value when the amount is nonzero.
    #[must_use]
    pub const fn positive(self) -> Option<PositiveMoney> {
        if self.0.is_zero() {
            None
        } else {
            Some(PositiveMoney(self))
        }
    }

    /// Adds two nonnegative amounts of the same currency.
    ///
    /// # Errors
    /// Returns a currency mismatch or overflow error.
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Subtracts an amount of the same currency while keeping the result nonnegative.
    ///
    /// # Errors
    /// Returns a currency mismatch error or an error if the result would be negative.
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        Self::new(self.0.checked_sub(other.0)?)
    }
}

impl TryFrom<Money> for NonNegativeMoney {
    type Error = MoneyError;

    fn try_from(money: Money) -> Result<Self, Self::Error> {
        Self::new(money)
    }
}

impl From<NonNegativeMoney> for Money {
    fn from(value: NonNegativeMoney) -> Self {
        value.money()
    }
}

impl fmt::Display for NonNegativeMoney {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Money with a strictly positive amount and a known currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PositiveMoney(NonNegativeMoney);

impl PositiveMoney {
    /// Validates that the amount is strictly positive.
    ///
    /// # Errors
    /// Returns [`MoneyError::NegativeAmount`] or [`MoneyError::ZeroAmount`] for an invalid amount.
    pub const fn new(money: Money) -> Result<Self, MoneyError> {
        match NonNegativeMoney::new(money) {
            Ok(value) => match value.positive() {
                Some(value) => Ok(value),
                None => Err(MoneyError::ZeroAmount),
            },
            Err(error) => Err(error),
        }
    }

    /// Returns the underlying monetary value.
    #[must_use]
    pub const fn money(self) -> Money {
        self.0.money()
    }

    /// Widens this value to the nonnegative type.
    #[must_use]
    pub const fn nonnegative(self) -> NonNegativeMoney {
        self.0
    }
}

impl TryFrom<Money> for PositiveMoney {
    type Error = MoneyError;

    fn try_from(money: Money) -> Result<Self, Self::Error> {
        Self::new(money)
    }
}

impl From<PositiveMoney> for Money {
    fn from(value: PositiveMoney) -> Self {
        value.money()
    }
}

impl fmt::Display for PositiveMoney {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
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
        let neg = m.checked_neg().expect("test amount can be negated");
        assert_eq!(neg.amount(), Amount::new(-100));
        assert_eq!(neg.currency(), Currency::USD);
    }

    #[test]
    fn abs_of_negative_money() {
        let m = Money::usd(-100);
        assert_eq!(m.checked_abs(), Ok(Money::usd(100)));
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
