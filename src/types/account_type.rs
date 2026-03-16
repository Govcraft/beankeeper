use core::fmt;
use core::str::FromStr;

use super::debit_credit::DebitOrCredit;

/// Error type for parsing [`AccountType`] from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountTypeError {
    /// The provided string does not match any account type name.
    InvalidName { value: String },
}

impl fmt::Display for AccountTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { value } => {
                write!(
                    f,
                    "invalid account type: {value:?} (expected Asset, Liability, Equity, Revenue, or Expense)"
                )
            }
        }
    }
}

impl std::error::Error for AccountTypeError {}

/// The five fundamental account types in double-entry bookkeeping.
///
/// Each type has a natural "normal balance" (debit or credit) that
/// indicates which side increases the account.
///
/// # The Accounting Equation
///
/// ```text
/// Assets + Expenses = Liabilities + Equity + Revenue
///  (Debit normal)       (Credit normal)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccountType {
    /// Resources owned. Normal balance: Debit.
    Asset,
    /// Obligations owed. Normal balance: Credit.
    Liability,
    /// Owner's residual interest. Normal balance: Credit.
    Equity,
    /// Income earned. Normal balance: Credit.
    Revenue,
    /// Costs incurred. Normal balance: Debit.
    Expense,
}

impl AccountType {
    /// Returns the normal balance direction for this account type.
    ///
    /// Assets and Expenses have debit normal balances;
    /// Liabilities, Equity, and Revenue have credit normal balances.
    #[must_use]
    pub const fn normal_balance(self) -> DebitOrCredit {
        match self {
            Self::Asset | Self::Expense => DebitOrCredit::Debit,
            Self::Liability | Self::Equity | Self::Revenue => DebitOrCredit::Credit,
        }
    }

    /// Returns `true` if this account type has a debit normal balance.
    #[must_use]
    pub const fn is_debit_normal(self) -> bool {
        self.normal_balance().is_debit()
    }

    /// Returns `true` if this account type has a credit normal balance.
    #[must_use]
    pub const fn is_credit_normal(self) -> bool {
        self.normal_balance().is_credit()
    }

    /// Returns the direction that increases this account type's balance.
    ///
    /// This is the same as [`normal_balance`](Self::normal_balance).
    #[must_use]
    pub const fn increases_with(self) -> DebitOrCredit {
        self.normal_balance()
    }

    /// Returns the direction that decreases this account type's balance.
    #[must_use]
    pub const fn decreases_with(self) -> DebitOrCredit {
        self.normal_balance().opposite()
    }

    /// Returns `true` if this is a balance sheet account (Asset, Liability, Equity).
    #[must_use]
    pub const fn is_balance_sheet(self) -> bool {
        matches!(self, Self::Asset | Self::Liability | Self::Equity)
    }

    /// Returns `true` if this is an income statement account (Revenue, Expense).
    #[must_use]
    pub const fn is_income_statement(self) -> bool {
        matches!(self, Self::Revenue | Self::Expense)
    }
}

impl fmt::Display for AccountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asset => write!(f, "Asset"),
            Self::Liability => write!(f, "Liability"),
            Self::Equity => write!(f, "Equity"),
            Self::Revenue => write!(f, "Revenue"),
            Self::Expense => write!(f, "Expense"),
        }
    }
}

impl FromStr for AccountType {
    type Err = AccountTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Asset" | "asset" => Ok(Self::Asset),
            "Liability" | "liability" => Ok(Self::Liability),
            "Equity" | "equity" => Ok(Self::Equity),
            "Revenue" | "revenue" => Ok(Self::Revenue),
            "Expense" | "expense" => Ok(Self::Expense),
            _ => Err(AccountTypeError::InvalidName {
                value: s.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_normal_balance_is_debit() {
        assert_eq!(AccountType::Asset.normal_balance(), DebitOrCredit::Debit);
    }

    #[test]
    fn liability_normal_balance_is_credit() {
        assert_eq!(AccountType::Liability.normal_balance(), DebitOrCredit::Credit);
    }

    #[test]
    fn equity_normal_balance_is_credit() {
        assert_eq!(AccountType::Equity.normal_balance(), DebitOrCredit::Credit);
    }

    #[test]
    fn revenue_normal_balance_is_credit() {
        assert_eq!(AccountType::Revenue.normal_balance(), DebitOrCredit::Credit);
    }

    #[test]
    fn expense_normal_balance_is_debit() {
        assert_eq!(AccountType::Expense.normal_balance(), DebitOrCredit::Debit);
    }

    #[test]
    fn asset_is_debit_normal() {
        assert!(AccountType::Asset.is_debit_normal());
        assert!(!AccountType::Asset.is_credit_normal());
    }

    #[test]
    fn liability_is_credit_normal() {
        assert!(AccountType::Liability.is_credit_normal());
        assert!(!AccountType::Liability.is_debit_normal());
    }

    #[test]
    fn increases_with_matches_normal_balance() {
        assert_eq!(AccountType::Asset.increases_with(), DebitOrCredit::Debit);
        assert_eq!(AccountType::Liability.increases_with(), DebitOrCredit::Credit);
    }

    #[test]
    fn decreases_with_is_opposite_of_normal() {
        assert_eq!(AccountType::Asset.decreases_with(), DebitOrCredit::Credit);
        assert_eq!(AccountType::Revenue.decreases_with(), DebitOrCredit::Debit);
    }

    #[test]
    fn asset_is_balance_sheet() {
        assert!(AccountType::Asset.is_balance_sheet());
        assert!(AccountType::Liability.is_balance_sheet());
        assert!(AccountType::Equity.is_balance_sheet());
    }

    #[test]
    fn revenue_is_income_statement() {
        assert!(AccountType::Revenue.is_income_statement());
        assert!(AccountType::Expense.is_income_statement());
    }

    #[test]
    fn balance_sheet_and_income_statement_are_exclusive() {
        assert!(!AccountType::Asset.is_income_statement());
        assert!(!AccountType::Revenue.is_balance_sheet());
    }

    #[test]
    fn from_str_round_trips_all_variants() {
        let variants = ["Asset", "Liability", "Equity", "Revenue", "Expense"];
        for name in &variants {
            let parsed: AccountType = name.parse().unwrap_or(AccountType::Asset);
            assert_eq!(format!("{parsed}"), *name);
        }
    }

    #[test]
    fn from_str_lowercase() {
        assert_eq!("asset".parse::<AccountType>().ok(), Some(AccountType::Asset));
        assert_eq!("expense".parse::<AccountType>().ok(), Some(AccountType::Expense));
    }

    #[test]
    fn from_str_invalid() {
        assert!("Debit".parse::<AccountType>().is_err());
    }

    #[test]
    fn display_all_variants() {
        assert_eq!(format!("{}", AccountType::Asset), "Asset");
        assert_eq!(format!("{}", AccountType::Liability), "Liability");
        assert_eq!(format!("{}", AccountType::Equity), "Equity");
        assert_eq!(format!("{}", AccountType::Revenue), "Revenue");
        assert_eq!(format!("{}", AccountType::Expense), "Expense");
    }
}
