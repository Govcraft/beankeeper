use core::fmt;

use super::account::Account;
use super::amount::Amount;
use super::debit_credit::DebitOrCredit;
use super::money::Money;

/// Error type for [`Entry`] construction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryError {
    /// The entry amount was zero.
    ZeroAmount,
    /// The entry amount was negative.
    NegativeAmount {
        /// The invalid amount.
        amount: Amount,
    },
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroAmount => write!(f, "entry amount must be positive, got zero"),
            Self::NegativeAmount { amount } => {
                write!(f, "entry amount must be positive, got {amount}")
            }
        }
    }
}

impl std::error::Error for EntryError {}

/// A single debit or credit entry within a transaction.
///
/// Each entry records a [`Money`] amount applied to an [`Account`]
/// as either a debit or credit. The amount must always be positive;
/// the direction is conveyed by the [`DebitOrCredit`] field.
///
/// # Examples
///
/// ```
/// use beankeeper::types::*;
///
/// let cash = Account::new(
///     AccountCode::new("1000").unwrap(),
///     "Cash",
///     AccountType::Asset,
/// );
/// let entry = Entry::debit(cash, Money::usd(500)).unwrap();
/// assert!(entry.is_debit());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    account: Account,
    amount: Money,
    direction: DebitOrCredit,
    memo: Option<String>,
}

impl Entry {
    /// Creates a new entry with explicit direction.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::ZeroAmount`] if the amount is zero, or
    /// [`EntryError::NegativeAmount`] if the amount is negative.
    pub fn new(account: Account, amount: Money, direction: DebitOrCredit) -> Result<Self, EntryError> {
        if amount.is_zero() {
            return Err(EntryError::ZeroAmount);
        }
        if amount.is_negative() {
            return Err(EntryError::NegativeAmount {
                amount: amount.amount(),
            });
        }

        Ok(Self {
            account,
            amount,
            direction,
            memo: None,
        })
    }

    /// Creates a new entry with explicit direction and a memo.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::ZeroAmount`] if the amount is zero, or
    /// [`EntryError::NegativeAmount`] if the amount is negative.
    pub fn with_memo(
        account: Account,
        amount: Money,
        direction: DebitOrCredit,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        let mut entry = Self::new(account, amount, direction)?;
        entry.memo = Some(memo.into());
        Ok(entry)
    }

    /// Creates a debit entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the amount is zero or negative.
    pub fn debit(account: Account, amount: Money) -> Result<Self, EntryError> {
        Self::new(account, amount, DebitOrCredit::Debit)
    }

    /// Creates a credit entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the amount is zero or negative.
    pub fn credit(account: Account, amount: Money) -> Result<Self, EntryError> {
        Self::new(account, amount, DebitOrCredit::Credit)
    }

    /// Creates a debit entry with a memo.
    ///
    /// # Errors
    ///
    /// Returns an error if the amount is zero or negative.
    pub fn debit_with_memo(
        account: Account,
        amount: Money,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        Self::with_memo(account, amount, DebitOrCredit::Debit, memo)
    }

    /// Creates a credit entry with a memo.
    ///
    /// # Errors
    ///
    /// Returns an error if the amount is zero or negative.
    pub fn credit_with_memo(
        account: Account,
        amount: Money,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        Self::with_memo(account, amount, DebitOrCredit::Credit, memo)
    }

    /// Returns the account this entry applies to.
    #[must_use]
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// Returns the monetary amount of this entry (always positive).
    #[must_use]
    pub fn amount(&self) -> Money {
        self.amount
    }

    /// Returns the direction of this entry.
    #[must_use]
    pub fn direction(&self) -> DebitOrCredit {
        self.direction
    }

    /// Returns `true` if this is a debit entry.
    #[must_use]
    pub fn is_debit(&self) -> bool {
        self.direction.is_debit()
    }

    /// Returns `true` if this is a credit entry.
    #[must_use]
    pub fn is_credit(&self) -> bool {
        self.direction.is_credit()
    }

    /// Returns the optional memo for this entry.
    #[must_use]
    pub fn memo(&self) -> Option<&str> {
        self.memo.as_deref()
    }

    /// Returns the amount with a sign relative to the account's normal balance.
    ///
    /// If the entry direction matches the account's normal balance, the
    /// result is positive (increases the account). Otherwise, negative
    /// (decreases the account).
    #[must_use]
    pub fn signed_amount(&self) -> Amount {
        if self.direction == self.account.normal_balance() {
            self.amount.amount()
        } else {
            -self.amount.amount()
        }
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = if self.direction.is_debit() { "DR" } else { "CR" };
        write!(f, "{dir} {} {}", self.account.name(), self.amount)?;
        if let Some(ref memo) = self.memo {
            write!(f, " ({memo})")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::account_code::AccountCode;
    use crate::types::account_type::AccountType;
    use crate::types::currency::Currency;

    fn cash_account() -> Account {
        Account::new(
            AccountCode::new("1000").unwrap_or_else(|e| panic!("test setup: {e}")),
            "Cash",
            AccountType::Asset,
        )
    }

    fn revenue_account() -> Account {
        Account::new(
            AccountCode::new("4000").unwrap_or_else(|e| panic!("test setup: {e}")),
            "Revenue",
            AccountType::Revenue,
        )
    }

    #[test]
    fn create_debit_entry() {
        let entry = Entry::debit(cash_account(), Money::usd(500));
        assert!(entry.is_ok());
    }

    #[test]
    fn create_credit_entry() {
        let entry = Entry::credit(revenue_account(), Money::usd(500));
        assert!(entry.is_ok());
    }

    #[test]
    fn zero_amount_rejected() {
        let entry = Entry::debit(cash_account(), Money::usd(0));
        assert!(matches!(entry, Err(EntryError::ZeroAmount)));
    }

    #[test]
    fn negative_amount_rejected() {
        let entry = Entry::debit(cash_account(), Money::usd(-100));
        assert!(matches!(entry, Err(EntryError::NegativeAmount { .. })));
    }

    #[test]
    fn is_debit_flag() {
        let entry = Entry::debit(cash_account(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(entry.is_debit());
        assert!(!entry.is_credit());
    }

    #[test]
    fn is_credit_flag() {
        let entry = Entry::credit(revenue_account(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(entry.is_credit());
        assert!(!entry.is_debit());
    }

    #[test]
    fn signed_amount_debit_on_debit_normal_account() {
        // Debit on an asset (debit-normal) -> positive
        let entry = Entry::debit(cash_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.signed_amount(), Amount::new(500));
    }

    #[test]
    fn signed_amount_credit_on_debit_normal_account() {
        // Credit on an asset (debit-normal) -> negative
        let entry = Entry::credit(cash_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.signed_amount(), Amount::new(-500));
    }

    #[test]
    fn signed_amount_credit_on_credit_normal_account() {
        // Credit on revenue (credit-normal) -> positive
        let entry = Entry::credit(revenue_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.signed_amount(), Amount::new(500));
    }

    #[test]
    fn signed_amount_debit_on_credit_normal_account() {
        // Debit on revenue (credit-normal) -> negative
        let entry = Entry::debit(revenue_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.signed_amount(), Amount::new(-500));
    }

    #[test]
    fn accessors() {
        let entry = Entry::debit(cash_account(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.account(), &cash_account());
        assert_eq!(entry.amount(), Money::usd(100));
        assert_eq!(entry.direction(), DebitOrCredit::Debit);
    }

    #[test]
    fn display_debit() {
        let entry = Entry::debit(cash_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(format!("{entry}"), "DR Cash USD 5.00");
    }

    #[test]
    fn display_credit() {
        let entry = Entry::credit(revenue_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(format!("{entry}"), "CR Revenue USD 5.00");
    }

    #[test]
    fn new_with_explicit_direction() {
        let entry = Entry::new(cash_account(), Money::usd(100), DebitOrCredit::Credit);
        assert!(entry.is_ok());
        let entry = entry.unwrap_or_else(|e| panic!("test: {e}"));
        assert!(entry.is_credit());
    }

    #[test]
    fn different_currency() {
        let entry = Entry::debit(cash_account(), Money::new(Amount::new(100), Currency::EUR));
        assert!(entry.is_ok());
    }

    #[test]
    fn error_display_zero() {
        assert!(format!("{}", EntryError::ZeroAmount).contains("zero"));
    }

    #[test]
    fn error_display_negative() {
        let err = EntryError::NegativeAmount {
            amount: Amount::new(-100),
        };
        assert!(format!("{err}").contains("positive"));
    }

    #[test]
    fn with_memo_sets_memo() {
        let entry =
            Entry::with_memo(cash_account(), Money::usd(500), DebitOrCredit::Debit, "Net pay")
                .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.memo(), Some("Net pay"));
    }

    #[test]
    fn debit_with_memo() {
        let entry = Entry::debit_with_memo(cash_account(), Money::usd(500), "Net pay")
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(entry.is_debit());
        assert_eq!(entry.memo(), Some("Net pay"));
    }

    #[test]
    fn credit_with_memo() {
        let entry = Entry::credit_with_memo(revenue_account(), Money::usd(500), "Sales income")
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(entry.is_credit());
        assert_eq!(entry.memo(), Some("Sales income"));
    }

    #[test]
    fn memo_is_none_by_default() {
        let entry = Entry::debit(cash_account(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(entry.memo(), None);
    }

    #[test]
    fn display_with_memo() {
        let entry = Entry::debit_with_memo(cash_account(), Money::usd(500), "Net pay")
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(format!("{entry}"), "DR Cash USD 5.00 (Net pay)");
    }

    #[test]
    fn with_memo_zero_rejected() {
        let result =
            Entry::with_memo(cash_account(), Money::usd(0), DebitOrCredit::Debit, "memo");
        assert!(matches!(result, Err(EntryError::ZeroAmount)));
    }
}
