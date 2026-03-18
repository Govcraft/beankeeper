use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;

use crate::reporting::{AccountBalance, TrialBalance};
use crate::types::{Account, AccountCode, Amount, DebitOrCredit, Entry, MoneyError};

use super::transaction::Transaction;

/// A general ledger holding posted transactions and providing balance queries.
///
/// The ledger is append-only: once a transaction is posted, it cannot be
/// removed. This follows standard accounting practice where corrections
/// are made via reversing entries.
///
/// # Examples
///
/// ```
/// use beankeeper::prelude::*;
///
/// let mut ledger = Ledger::new();
///
/// let cash = Account::new(
///     AccountCode::new("1000").unwrap(),
///     "Cash",
///     AccountType::Asset,
/// );
/// let revenue = Account::new(
///     AccountCode::new("4000").unwrap(),
///     "Revenue",
///     AccountType::Revenue,
/// );
///
/// let txn = JournalEntry::new(
///         NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
///         "Sale",
///     )
///     .debit(&cash, Money::usd(500_00))
///     .unwrap()
///     .credit(&revenue, Money::usd(500_00))
///     .unwrap()
///     .post()
///     .unwrap();
///
/// ledger.post(txn);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    transactions: Vec<Transaction>,
    /// Maps account code to `(transaction_index, entry_index)` pairs for O(1) lookup.
    entry_index: HashMap<AccountCode, Vec<(usize, usize)>>,
    /// Unique accounts seen, sorted by code for `trial_balance()`.
    accounts: BTreeMap<AccountCode, Account>,
}

impl Ledger {
    /// Creates a new empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Posts a validated transaction to the ledger.
    ///
    /// Incrementally updates the internal account index so that subsequent
    /// balance queries remain O(1) lookup + O(k) summation.
    pub fn post(&mut self, transaction: Transaction) {
        let txn_idx = self.transactions.len();
        for (entry_idx, entry) in transaction.entries().iter().enumerate() {
            let code = entry.account().code().clone();
            self.accounts
                .entry(code.clone())
                .or_insert_with(|| entry.account().clone());
            self.entry_index
                .entry(code)
                .or_default()
                .push((txn_idx, entry_idx));
        }
        self.transactions.push(transaction);
    }

    /// Returns all posted transactions.
    #[must_use]
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Returns the number of posted transactions.
    #[must_use]
    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }

    /// Computes the net balance for a specific account across all transactions.
    ///
    /// The balance is signed according to the account's normal balance:
    /// positive means the account has its expected balance, negative means
    /// contra-balance.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn balance_for(&self, account: &Account) -> Result<Amount, MoneyError> {
        let mut balance = Amount::ZERO;

        for (_, entry) in self.indexed_entries(account.code()) {
            balance = balance
                .checked_add(entry.signed_amount())
                .ok_or(MoneyError::Overflow)?;
        }

        Ok(balance)
    }

    /// Computes the total of all debit entries for an account.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn debit_total_for(&self, account: &Account) -> Result<Amount, MoneyError> {
        let mut total = Amount::ZERO;

        for (_, entry) in self.indexed_entries(account.code()) {
            if entry.direction() == DebitOrCredit::Debit {
                total = total
                    .checked_add(entry.amount().amount())
                    .ok_or(MoneyError::Overflow)?;
            }
        }

        Ok(total)
    }

    /// Computes the total of all credit entries for an account.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn credit_total_for(&self, account: &Account) -> Result<Amount, MoneyError> {
        let mut total = Amount::ZERO;

        for (_, entry) in self.indexed_entries(account.code()) {
            if entry.direction() == DebitOrCredit::Credit {
                total = total
                    .checked_add(entry.amount().amount())
                    .ok_or(MoneyError::Overflow)?;
            }
        }

        Ok(total)
    }

    /// Returns all entries involving the given account across all transactions.
    #[must_use]
    pub fn entries_for(&self, account: &Account) -> Vec<&Entry> {
        self.indexed_entries(account.code())
            .map(|(_, entry)| entry)
            .collect()
    }

    /// Generates a trial balance report from all posted transactions.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn trial_balance(&self) -> Result<TrialBalance, MoneyError> {
        let mut balances = Vec::with_capacity(self.accounts.len());

        // BTreeMap iteration is sorted by AccountCode
        for (code, account) in &self.accounts {
            let (debit_total, credit_total) = self.debit_credit_totals(code)?;
            balances.push(AccountBalance::new(
                account.clone(),
                debit_total,
                credit_total,
            ));
        }

        Ok(TrialBalance::new(balances))
    }

    /// Returns whether the ledger is balanced (total debits == total credits).
    ///
    /// A properly functioning ledger should always be balanced since
    /// every posted transaction is individually balanced.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn is_balanced(&self) -> Result<bool, MoneyError> {
        let tb = self.trial_balance()?;
        Ok(tb.is_balanced())
    }

    /// Returns transactions on or before the given date.
    #[must_use]
    pub fn transactions_as_of(&self, date: NaiveDate) -> Vec<&Transaction> {
        self.transactions
            .iter()
            .filter(|txn| txn.date() <= date)
            .collect()
    }

    /// Computes the net balance for an account using only transactions on or before the given date.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn balance_for_as_of(
        &self,
        account: &Account,
        date: NaiveDate,
    ) -> Result<Amount, MoneyError> {
        let mut balance = Amount::ZERO;

        for (txn, entry) in self.indexed_entries(account.code()) {
            if txn.date() <= date {
                balance = balance
                    .checked_add(entry.signed_amount())
                    .ok_or(MoneyError::Overflow)?;
            }
        }

        Ok(balance)
    }

    /// Generates a trial balance using only transactions on or before the given date.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if arithmetic overflows.
    pub fn trial_balance_as_of(&self, date: NaiveDate) -> Result<TrialBalance, MoneyError> {
        let mut balances = Vec::with_capacity(self.accounts.len());

        for (code, account) in &self.accounts {
            let (debit_total, credit_total) = self.debit_credit_totals_as_of(code, date)?;

            // Only include accounts that had activity on or before the date
            if debit_total != Amount::ZERO || credit_total != Amount::ZERO {
                balances.push(AccountBalance::new(
                    account.clone(),
                    debit_total,
                    credit_total,
                ));
            }
        }

        Ok(TrialBalance::new(balances))
    }

    /// Iterates `(transaction, entry)` pairs for a given account code via the index.
    fn indexed_entries(&self, code: &AccountCode) -> impl Iterator<Item = (&Transaction, &Entry)> {
        self.entry_index
            .get(code)
            .into_iter()
            .flatten()
            .map(|&(txn_idx, entry_idx)| {
                let txn = &self.transactions[txn_idx];
                (txn, &txn.entries()[entry_idx])
            })
    }

    /// Computes debit and credit totals for an account code across all transactions.
    fn debit_credit_totals(&self, code: &AccountCode) -> Result<(Amount, Amount), MoneyError> {
        let mut debit_total = Amount::ZERO;
        let mut credit_total = Amount::ZERO;

        for (_, entry) in self.indexed_entries(code) {
            match entry.direction() {
                DebitOrCredit::Debit => {
                    debit_total = debit_total
                        .checked_add(entry.amount().amount())
                        .ok_or(MoneyError::Overflow)?;
                }
                DebitOrCredit::Credit => {
                    credit_total = credit_total
                        .checked_add(entry.amount().amount())
                        .ok_or(MoneyError::Overflow)?;
                }
            }
        }

        Ok((debit_total, credit_total))
    }

    /// Computes debit and credit totals for an account code, filtered to transactions on or before a date.
    fn debit_credit_totals_as_of(
        &self,
        code: &AccountCode,
        date: NaiveDate,
    ) -> Result<(Amount, Amount), MoneyError> {
        let mut debit_total = Amount::ZERO;
        let mut credit_total = Amount::ZERO;

        for (txn, entry) in self.indexed_entries(code) {
            if txn.date() <= date {
                match entry.direction() {
                    DebitOrCredit::Debit => {
                        debit_total = debit_total
                            .checked_add(entry.amount().amount())
                            .ok_or(MoneyError::Overflow)?;
                    }
                    DebitOrCredit::Credit => {
                        credit_total = credit_total
                            .checked_add(entry.amount().amount())
                            .ok_or(MoneyError::Overflow)?;
                    }
                }
            }
        }

        Ok((debit_total, credit_total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::JournalEntry;
    use crate::types::{AccountCode, AccountType, Money};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap_or_else(|| panic!("invalid date: {y}-{m}-{d}"))
    }

    fn make_account(code: &str, name: &str, acct_type: AccountType) -> Account {
        Account::new(
            AccountCode::new(code).unwrap_or_else(|e| panic!("test setup: {e}")),
            name,
            acct_type,
        )
    }

    fn cash() -> Account {
        make_account("1000", "Cash", AccountType::Asset)
    }

    fn revenue() -> Account {
        make_account("4000", "Revenue", AccountType::Revenue)
    }

    fn expense() -> Account {
        make_account("5000", "Rent", AccountType::Expense)
    }

    fn post_sale(ledger: &mut Ledger, amount: i128) {
        post_sale_on(ledger, amount, date(2024, 1, 15));
    }

    fn post_sale_on(ledger: &mut Ledger, amount: i128, on: NaiveDate) {
        let txn = JournalEntry::new(on, "Sale")
            .debit(&cash(), Money::usd(amount))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(amount))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));
        ledger.post(txn);
    }

    #[test]
    fn empty_ledger_has_zero_balance() {
        let ledger = Ledger::new();
        let balance = ledger
            .balance_for(&cash())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::ZERO);
    }

    #[test]
    fn post_transaction_increases_count() {
        let mut ledger = Ledger::new();
        assert_eq!(ledger.transaction_count(), 0);
        post_sale(&mut ledger, 500);
        assert_eq!(ledger.transaction_count(), 1);
    }

    #[test]
    fn balance_for_asset_after_debit() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);

        // Cash is asset (debit normal) - debit increases it
        let balance = ledger
            .balance_for(&cash())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::new(500));
    }

    #[test]
    fn balance_for_revenue_after_credit() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);

        // Revenue is credit normal - credit increases it
        let balance = ledger
            .balance_for(&revenue())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::new(500));
    }

    #[test]
    fn trial_balance_is_balanced() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);
        post_sale(&mut ledger, 300);

        let tb = ledger
            .trial_balance()
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(tb.is_balanced());
    }

    #[test]
    fn multiple_transactions_accumulate() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);
        post_sale(&mut ledger, 300);

        let balance = ledger
            .balance_for(&cash())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::new(800));
    }

    #[test]
    fn entries_for_account_returns_correct_entries() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);

        let cash_acct = cash();
        let entries = ledger.entries_for(&cash_acct);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_debit());
    }

    #[test]
    fn debit_total_for_account() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);
        post_sale(&mut ledger, 300);

        let total = ledger
            .debit_total_for(&cash())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(total, Amount::new(800));
    }

    #[test]
    fn credit_total_for_account() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);

        let total = ledger
            .credit_total_for(&revenue())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(total, Amount::new(500));
    }

    #[test]
    fn is_balanced_returns_true() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 500);
        assert!(ledger.is_balanced().unwrap_or(false));
    }

    #[test]
    fn empty_ledger_is_balanced() {
        let ledger = Ledger::new();
        assert!(ledger.is_balanced().unwrap_or(false));
    }

    #[test]
    fn complex_scenario() {
        let mut ledger = Ledger::new();

        // Record a sale
        post_sale(&mut ledger, 1000);

        // Pay rent
        let txn = JournalEntry::new(date(2024, 1, 20), "Rent")
            .debit(&expense(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&cash(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));
        ledger.post(txn);

        // Cash should be 1000 - 500 = 500
        let cash_balance = ledger
            .balance_for(&cash())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(cash_balance, Amount::new(500));

        // Revenue should be 1000
        let rev_balance = ledger
            .balance_for(&revenue())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(rev_balance, Amount::new(1000));

        // Expense should be 500
        let exp_balance = ledger
            .balance_for(&expense())
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(exp_balance, Amount::new(500));

        // Ledger should be balanced
        assert!(ledger.is_balanced().unwrap_or(false));
    }

    #[test]
    fn transactions_accessor() {
        let mut ledger = Ledger::new();
        post_sale(&mut ledger, 100);
        assert_eq!(ledger.transactions().len(), 1);
    }

    #[test]
    fn transactions_as_of_filters_by_date() {
        let mut ledger = Ledger::new();
        post_sale_on(&mut ledger, 500, date(2024, 1, 10));
        post_sale_on(&mut ledger, 300, date(2024, 1, 20));
        post_sale_on(&mut ledger, 200, date(2024, 2, 1));

        assert_eq!(ledger.transactions_as_of(date(2024, 1, 15)).len(), 1);
        assert_eq!(ledger.transactions_as_of(date(2024, 1, 20)).len(), 2);
        assert_eq!(ledger.transactions_as_of(date(2024, 2, 1)).len(), 3);
    }

    #[test]
    fn balance_for_as_of_filters_by_date() {
        let mut ledger = Ledger::new();
        post_sale_on(&mut ledger, 500, date(2024, 1, 10));
        post_sale_on(&mut ledger, 300, date(2024, 1, 20));

        let balance = ledger
            .balance_for_as_of(&cash(), date(2024, 1, 15))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::new(500));

        let balance = ledger
            .balance_for_as_of(&cash(), date(2024, 1, 20))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(balance, Amount::new(800));
    }

    #[test]
    fn trial_balance_as_of_filters_by_date() {
        let mut ledger = Ledger::new();
        post_sale_on(&mut ledger, 500, date(2024, 1, 10));
        post_sale_on(&mut ledger, 300, date(2024, 1, 20));

        let tb = ledger
            .trial_balance_as_of(date(2024, 1, 15))
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert!(tb.is_balanced());
        assert_eq!(tb.balances().len(), 2);
    }
}
