use core::fmt;

use crate::types::{AccountType, Amount};

use super::account_balance::AccountBalance;

/// A trial balance report listing all accounts with their debit and credit totals.
///
/// A balanced trial balance has equal total debits and total credits.
/// This serves as a basic check on the ledger's integrity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialBalance {
    balances: Vec<AccountBalance>,
}

impl TrialBalance {
    /// Creates a new trial balance from a list of account balances.
    #[must_use]
    pub fn new(balances: Vec<AccountBalance>) -> Self {
        Self { balances }
    }

    /// Returns all account balances.
    #[must_use]
    pub fn balances(&self) -> &[AccountBalance] {
        &self.balances
    }

    /// Computes the total of all debit balances.
    #[must_use]
    pub fn total_debits(&self) -> Amount {
        self.balances.iter().map(AccountBalance::debit_total).sum()
    }

    /// Computes the total of all credit balances.
    #[must_use]
    pub fn total_credits(&self) -> Amount {
        self.balances.iter().map(AccountBalance::credit_total).sum()
    }

    /// Returns `true` if total debits equal total credits.
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        self.total_debits() == self.total_credits()
    }

    /// Returns account balances filtered by account type.
    #[must_use]
    pub fn accounts_by_type(&self, account_type: AccountType) -> Vec<&AccountBalance> {
        self.balances
            .iter()
            .filter(|ab| ab.account().account_type() == account_type)
            .collect()
    }
}

impl fmt::Display for TrialBalance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Trial Balance")?;
        writeln!(f, "{:-<50}", "")?;

        for balance in &self.balances {
            writeln!(f, "  {balance}")?;
        }

        writeln!(f, "{:-<50}", "")?;
        writeln!(
            f,
            "  Totals: DR {} / CR {}",
            self.total_debits(),
            self.total_credits()
        )?;

        if self.is_balanced() {
            writeln!(f, "  Status: BALANCED")?;
        } else {
            writeln!(f, "  Status: UNBALANCED")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Account, AccountCode, AccountType, Amount};

    fn make_account(code: &str, name: &str, acct_type: AccountType) -> Account {
        Account::new(
            AccountCode::new(code).unwrap_or_else(|e| panic!("test setup: {e}")),
            name,
            acct_type,
        )
    }

    fn make_balanced_trial_balance() -> TrialBalance {
        TrialBalance::new(vec![
            AccountBalance::new(
                make_account("1000", "Cash", AccountType::Asset),
                Amount::new(1000),
                Amount::new(500),
            ),
            AccountBalance::new(
                make_account("4000", "Revenue", AccountType::Revenue),
                Amount::ZERO,
                Amount::new(1000),
            ),
            AccountBalance::new(
                make_account("5000", "Rent", AccountType::Expense),
                Amount::new(500),
                Amount::ZERO,
            ),
        ])
    }

    #[test]
    fn empty_trial_balance_is_balanced() {
        let tb = TrialBalance::new(vec![]);
        assert!(tb.is_balanced());
    }

    #[test]
    fn balanced_trial_balance() {
        let tb = make_balanced_trial_balance();
        assert!(tb.is_balanced());
        assert_eq!(tb.total_debits(), Amount::new(1500));
        assert_eq!(tb.total_credits(), Amount::new(1500));
    }

    #[test]
    fn unbalanced_trial_balance() {
        let tb = TrialBalance::new(vec![AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(100),
            Amount::ZERO,
        )]);
        assert!(!tb.is_balanced());
    }

    #[test]
    fn accounts_by_type_filters() {
        let tb = make_balanced_trial_balance();

        let assets = tb.accounts_by_type(AccountType::Asset);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].account().name(), "Cash");

        let revenues = tb.accounts_by_type(AccountType::Revenue);
        assert_eq!(revenues.len(), 1);

        let liabilities = tb.accounts_by_type(AccountType::Liability);
        assert!(liabilities.is_empty());
    }

    #[test]
    fn balances_accessor() {
        let tb = make_balanced_trial_balance();
        assert_eq!(tb.balances().len(), 3);
    }

    #[test]
    fn display_shows_balanced_status() {
        let tb = make_balanced_trial_balance();
        let display = format!("{tb}");
        assert!(display.contains("BALANCED"));
        assert!(display.contains("Trial Balance"));
    }

    #[test]
    fn display_shows_unbalanced_status() {
        let tb = TrialBalance::new(vec![AccountBalance::new(
            make_account("1000", "Cash", AccountType::Asset),
            Amount::new(100),
            Amount::ZERO,
        )]);
        let display = format!("{tb}");
        assert!(display.contains("UNBALANCED"));
    }
}
