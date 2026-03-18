use core::fmt;

use super::account_code::AccountCode;
use super::account_type::AccountType;
use super::debit_credit::DebitOrCredit;

/// A named account in the chart of accounts.
///
/// An account has a unique code, a human-readable name, and a type
/// that determines its normal balance behavior.
///
/// # Examples
///
/// ```
/// use beankeeper::types::{Account, AccountCode, AccountType};
///
/// let cash = Account::new(
///     AccountCode::new("1000").unwrap(),
///     "Cash",
///     AccountType::Asset,
/// );
/// assert_eq!(cash.name(), "Cash");
/// assert_eq!(cash.account_type(), AccountType::Asset);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Account {
    code: AccountCode,
    name: String,
    kind: AccountType,
}

impl Account {
    /// Creates a new account.
    #[must_use]
    pub fn new(code: AccountCode, name: impl Into<String>, account_type: AccountType) -> Self {
        Self {
            code,
            name: name.into(),
            kind: account_type,
        }
    }

    /// Returns the account code.
    #[must_use]
    pub fn code(&self) -> &AccountCode {
        &self.code
    }

    /// Returns the account name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the account type.
    #[must_use]
    pub fn account_type(&self) -> AccountType {
        self.kind
    }

    /// Returns the normal balance direction for this account's type.
    #[must_use]
    pub fn normal_balance(&self) -> DebitOrCredit {
        self.kind.normal_balance()
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} - {} ({})", self.code, self.name, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_code(s: &str) -> AccountCode {
        AccountCode::new(s).unwrap_or_else(|_| {
            AccountCode::new("0000").unwrap_or_else(|_| {
                // This path should never be reached in tests
                panic!("test setup failed: could not create account code");
            })
        })
    }

    #[test]
    fn create_account() {
        let acct = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        assert_eq!(acct.name(), "Cash");
        assert_eq!(acct.account_type(), AccountType::Asset);
        assert_eq!(acct.code().as_str(), "1000");
    }

    #[test]
    fn normal_balance_delegates_to_type() {
        let asset = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        assert_eq!(asset.normal_balance(), DebitOrCredit::Debit);

        let liability = Account::new(make_code("2000"), "Payables", AccountType::Liability);
        assert_eq!(liability.normal_balance(), DebitOrCredit::Credit);
    }

    #[test]
    fn display_format() {
        let acct = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        assert_eq!(format!("{acct}"), "1000 - Cash (Asset)");
    }

    #[test]
    fn equality() {
        let a = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        let b = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_code() {
        let a = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        let b = Account::new(make_code("1001"), "Cash", AccountType::Asset);
        assert_ne!(a, b);
    }

    #[test]
    fn inequality_different_name() {
        let a = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        let b = Account::new(make_code("1000"), "Bank", AccountType::Asset);
        assert_ne!(a, b);
    }

    #[test]
    fn inequality_different_type() {
        let a = Account::new(make_code("1000"), "Cash", AccountType::Asset);
        let b = Account::new(make_code("1000"), "Cash", AccountType::Liability);
        assert_ne!(a, b);
    }

    #[test]
    fn accepts_string_name() {
        let acct = Account::new(make_code("1000"), String::from("Cash"), AccountType::Asset);
        assert_eq!(acct.name(), "Cash");
    }
}
