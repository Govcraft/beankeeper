//! Cash membership independent of an account's display name.

use core::{fmt, str::FromStr};

use super::AccountType;

/// How an asset participates in cash reporting. New accounts are non-cash
/// until explicitly classified; names never determine membership.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CashClass {
    /// Excluded from default cash reporting.
    #[default]
    NonCash,
    /// Cash available for use.
    Cash,
    /// An owner-designated cash equivalent, carried at ledger book value.
    CashEquivalent,
}

impl CashClass {
    /// Canonical storage and wire value.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NonCash => "non_cash",
            Self::Cash => "cash",
            Self::CashEquivalent => "cash_equivalent",
        }
    }

    /// Whether this classification belongs in default cash reports.
    #[must_use]
    pub const fn is_cash(self) -> bool {
        matches!(self, Self::Cash | Self::CashEquivalent)
    }
}

/// Unknown cash classification at an input boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashClassError(String);

impl fmt::Display for CashClassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid cash class {:?}; expected non_cash, cash, or cash_equivalent",
            self.0
        )
    }
}
impl std::error::Error for CashClassError {}

impl FromStr for CashClass {
    type Err = CashClassError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "non_cash" => Ok(Self::NonCash),
            "cash" => Ok(Self::Cash),
            "cash_equivalent" => Ok(Self::CashEquivalent),
            _ => Err(CashClassError(value.to_owned())),
        }
    }
}
impl fmt::Display for CashClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// A validated pairing of accounting type and cash membership.
/// Private fields prevent constructing cash liabilities or expenses.
///
/// ```compile_fail
/// use beankeeper::types::{AccountClassification, AccountType, CashClass};
/// let invalid = AccountClassification {
///     account_type: AccountType::Liability, cash_class: CashClass::Cash,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountClassification {
    account_type: AccountType,
    cash_class: CashClass,
}

/// Cash membership was requested for a non-asset account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonAssetCash(AccountType);
impl fmt::Display for NonAssetCash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cash accounts must be assets, found {}", self.0)
    }
}
impl std::error::Error for NonAssetCash {}

impl AccountClassification {
    /// Fundamental accounting type.
    #[must_use]
    pub const fn account_type(self) -> AccountType {
        self.account_type
    }
    /// Validated cash membership.
    #[must_use]
    pub const fn cash_class(self) -> CashClass {
        self.cash_class
    }
}
impl From<AccountType> for AccountClassification {
    fn from(account_type: AccountType) -> Self {
        Self {
            account_type,
            cash_class: CashClass::default(),
        }
    }
}
impl TryFrom<(AccountType, CashClass)> for AccountClassification {
    type Error = NonAssetCash;
    fn try_from((account_type, cash_class): (AccountType, CashClass)) -> Result<Self, Self::Error> {
        if cash_class.is_cash() && account_type != AccountType::Asset {
            return Err(NonAssetCash(account_type));
        }
        Ok(Self {
            account_type,
            cash_class,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_assets_can_be_cash() {
        for kind in [
            AccountType::Asset,
            AccountType::Liability,
            AccountType::Equity,
            AccountType::Revenue,
            AccountType::Expense,
        ] {
            for class in [
                CashClass::NonCash,
                CashClass::Cash,
                CashClass::CashEquivalent,
            ] {
                assert_eq!(
                    AccountClassification::try_from((kind, class)).is_ok(),
                    kind == AccountType::Asset || class == CashClass::NonCash
                );
            }
            assert_eq!(
                AccountClassification::from(kind).cash_class(),
                CashClass::default()
            );
        }
    }
    #[test]
    fn classification_parses_canonical_values() {
        for class in [
            CashClass::NonCash,
            CashClass::Cash,
            CashClass::CashEquivalent,
        ] {
            assert_eq!(class.to_string().parse::<CashClass>(), Ok(class));
        }
        assert!("bank".parse::<CashClass>().is_err());
        assert!("".parse::<CashClass>().is_err());
    }
}
