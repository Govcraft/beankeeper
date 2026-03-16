use core::fmt;

use crate::types::{Account, Amount};

/// The computed balance for a single account.
///
/// Tracks the total debits and total credits separately, allowing
/// both the net balance and the breakdown to be queried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountBalance {
    account: Account,
    debit_total: Amount,
    credit_total: Amount,
}

impl AccountBalance {
    /// Creates a new account balance.
    #[must_use]
    pub fn new(account: Account, debit_total: Amount, credit_total: Amount) -> Self {
        Self {
            account,
            debit_total,
            credit_total,
        }
    }

    /// Returns the account.
    #[must_use]
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// Returns the total of all debit entries.
    #[must_use]
    pub fn debit_total(&self) -> Amount {
        self.debit_total
    }

    /// Returns the total of all credit entries.
    #[must_use]
    pub fn credit_total(&self) -> Amount {
        self.credit_total
    }

    /// Returns the net balance (debit total minus credit total).
    ///
    /// Positive means net debit, negative means net credit.
    #[must_use]
    pub fn net_balance(&self) -> Amount {
        self.debit_total - self.credit_total
    }

    /// Returns the balance expressed according to the account's normal side.
    ///
    /// For debit-normal accounts (Asset, Expense), this is `debit - credit`.
    /// For credit-normal accounts (Liability, Equity, Revenue), this is `credit - debit`.
    ///
    /// A positive result means the account is in its normal state.
    #[must_use]
    pub fn normal_balance_amount(&self) -> Amount {
        if self.account.account_type().is_debit_normal() {
            self.debit_total - self.credit_total
        } else {
            self.credit_total - self.debit_total
        }
    }

    /// Returns `true` if both debit and credit totals are zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.debit_total.is_zero() && self.credit_total.is_zero()
    }
}

impl fmt::Display for AccountBalance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: DR {} / CR {}",
            self.account, self.debit_total, self.credit_total
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountCode, AccountType};

    fn make_account(code: &str, name: &str, acct_type: AccountType) -> Account {
        Account::new(
            AccountCode::new(code).unwrap_or_else(|e| panic!("test setup: {e}")),
            name,
            acct_type,
        )
    }

    #[test]
    fn net_balance_debit_heavy() {
        let ab = AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(500),
            Amount::new(200),
        );
        assert_eq!(ab.net_balance(), Amount::new(300));
    }

    #[test]
    fn net_balance_credit_heavy() {
        let ab = AccountBalance::new(
            make_account("4000", "Revenue", AccountType::Revenue),
            Amount::new(100),
            Amount::new(500),
        );
        assert_eq!(ab.net_balance(), Amount::new(-400));
    }

    #[test]
    fn normal_balance_for_asset() {
        let ab = AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(500),
            Amount::new(200),
        );
        // Asset is debit-normal: DR - CR = 300
        assert_eq!(ab.normal_balance_amount(), Amount::new(300));
    }

    #[test]
    fn normal_balance_for_revenue() {
        let ab = AccountBalance::new(
            make_account("4000", "Revenue", AccountType::Revenue),
            Amount::new(100),
            Amount::new(500),
        );
        // Revenue is credit-normal: CR - DR = 400
        assert_eq!(ab.normal_balance_amount(), Amount::new(400));
    }

    #[test]
    fn is_zero_when_both_zero() {
        let ab = AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::ZERO,
            Amount::ZERO,
        );
        assert!(ab.is_zero());
    }

    #[test]
    fn is_not_zero_when_has_values() {
        let ab = AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(100),
            Amount::ZERO,
        );
        assert!(!ab.is_zero());
    }

    #[test]
    fn accessors() {
        let acct = make_account("1000", "Cash", AccountType::Asset);
        let ab = AccountBalance::new(acct.clone(), Amount::new(500), Amount::new(200));
        assert_eq!(ab.account(), &acct);
        assert_eq!(ab.debit_total(), Amount::new(500));
        assert_eq!(ab.credit_total(), Amount::new(200));
    }

    #[test]
    fn display_format() {
        let ab = AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(500),
            Amount::new(200),
        );
        let display = format!("{ab}");
        assert!(display.contains("Cash"));
        assert!(display.contains("DR"));
        assert!(display.contains("CR"));
    }
}
