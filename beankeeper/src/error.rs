//! Top-level error type aggregating all domain errors.

use core::fmt;

use crate::core::TransactionError;
use crate::types::{
    AccountCodeError, AccountTypeError, AmountError, CurrencyError, DebitCreditError, EntryError,
    MoneyError,
};

/// Top-level error type aggregating all domain errors in the crate.
///
/// Each variant wraps a specific domain error, allowing callers to use
/// the `?` operator with any `beankeeper` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeanError {
    /// An error from monetary amount operations.
    Amount(AmountError),
    /// An error from currency operations.
    Currency(CurrencyError),
    /// An error from money operations.
    Money(MoneyError),
    /// An error from account code validation.
    AccountCode(AccountCodeError),
    /// An error from account type parsing.
    AccountType(AccountTypeError),
    /// An error from debit/credit parsing.
    DebitCredit(DebitCreditError),
    /// An error from entry construction.
    Entry(EntryError),
    /// An error from transaction validation.
    Transaction(TransactionError),
}

impl fmt::Display for BeanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Amount(e) => write!(f, "{e}"),
            Self::Currency(e) => write!(f, "{e}"),
            Self::Money(e) => write!(f, "{e}"),
            Self::AccountCode(e) => write!(f, "{e}"),
            Self::AccountType(e) => write!(f, "{e}"),
            Self::DebitCredit(e) => write!(f, "{e}"),
            Self::Entry(e) => write!(f, "{e}"),
            Self::Transaction(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BeanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Amount(e) => Some(e),
            Self::Currency(e) => Some(e),
            Self::Money(e) => Some(e),
            Self::AccountCode(e) => Some(e),
            Self::AccountType(e) => Some(e),
            Self::DebitCredit(e) => Some(e),
            Self::Entry(e) => Some(e),
            Self::Transaction(e) => Some(e),
        }
    }
}

impl From<AmountError> for BeanError {
    fn from(err: AmountError) -> Self {
        Self::Amount(err)
    }
}

impl From<CurrencyError> for BeanError {
    fn from(err: CurrencyError) -> Self {
        Self::Currency(err)
    }
}

impl From<MoneyError> for BeanError {
    fn from(err: MoneyError) -> Self {
        Self::Money(err)
    }
}

impl From<AccountCodeError> for BeanError {
    fn from(err: AccountCodeError) -> Self {
        Self::AccountCode(err)
    }
}

impl From<AccountTypeError> for BeanError {
    fn from(err: AccountTypeError) -> Self {
        Self::AccountType(err)
    }
}

impl From<DebitCreditError> for BeanError {
    fn from(err: DebitCreditError) -> Self {
        Self::DebitCredit(err)
    }
}

impl From<EntryError> for BeanError {
    fn from(err: EntryError) -> Self {
        Self::Entry(err)
    }
}

impl From<TransactionError> for BeanError {
    fn from(err: TransactionError) -> Self {
        Self::Transaction(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_amount_error() {
        let err: BeanError = AmountError::Overflow.into();
        assert!(matches!(err, BeanError::Amount(AmountError::Overflow)));
    }

    #[test]
    fn from_currency_error() {
        let err: BeanError = CurrencyError::InvalidCode {
            value: "bad".to_owned(),
        }
        .into();
        assert!(matches!(err, BeanError::Currency(_)));
    }

    #[test]
    fn from_money_error() {
        let err: BeanError = MoneyError::Overflow.into();
        assert!(matches!(err, BeanError::Money(_)));
    }

    #[test]
    fn from_account_code_error() {
        let err: BeanError = AccountCodeError::Empty.into();
        assert!(matches!(err, BeanError::AccountCode(_)));
    }

    #[test]
    fn from_entry_error() {
        let err: BeanError = EntryError::ZeroAmount.into();
        assert!(matches!(err, BeanError::Entry(_)));
    }

    #[test]
    fn from_transaction_error() {
        let err: BeanError = TransactionError::NoEntries.into();
        assert!(matches!(err, BeanError::Transaction(_)));
    }

    #[test]
    fn display_delegates() {
        let err = BeanError::Amount(AmountError::Overflow);
        assert_eq!(format!("{err}"), "amount overflow");
    }

    #[test]
    fn source_returns_inner() {
        use std::error::Error;
        let err = BeanError::Amount(AmountError::Overflow);
        assert!(err.source().is_some());
    }
}
