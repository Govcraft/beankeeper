use chrono::NaiveDate;

use crate::types::{
    Account, Amount, Currency, DebitOrCredit, Entry, EntryError, Money, MoneyError, SourceDocument,
};

use super::transaction::{Transaction, TransactionError};

/// A builder for constructing balanced transactions.
///
/// The journal chooses a currency at construction. Debit and credit amounts
/// inherit that currency, and prebuilt entries with a different currency are
/// rejected when added. [`post`](Self::post) checks entry count and balance.
///
/// # Examples
///
/// ```
/// use beankeeper::prelude::*;
///
/// let cash = Account::new(
///     AccountCode::new("1000").unwrap(),
///     "Cash",
///     AccountType::Asset,
/// );
/// let revenue = Account::new(
///     AccountCode::new("4000").unwrap(),
///     "Sales Revenue",
///     AccountType::Revenue,
/// );
///
/// let txn = JournalEntry::new(
///         NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
///         "Sale of goods",
///         Currency::USD,
///     )
///     .debit(&cash, Amount::new(500_00)).unwrap()
///     .credit(&revenue, Amount::new(500_00)).unwrap()
///     .post()
///     .unwrap();
///
/// assert_eq!(txn.description(), "Sale of goods");
/// ```
pub struct JournalEntry {
    date: NaiveDate,
    description: String,
    currency: Currency,
    entries: Vec<Entry>,
    metadata: Option<String>,
    attachments: Vec<SourceDocument>,
}

impl JournalEntry {
    /// Creates a new journal entry with the given date, description, and currency.
    #[must_use]
    pub fn new(date: NaiveDate, description: impl Into<String>, currency: Currency) -> Self {
        Self {
            date,
            description: description.into(),
            currency,
            entries: Vec::new(),
            metadata: None,
            attachments: Vec::new(),
        }
    }

    /// Attaches metadata to this journal entry.
    #[must_use]
    pub fn with_metadata(mut self, metadata: impl Into<String>) -> Self {
        self.metadata = Some(metadata.into());
        self
    }

    /// Attaches a source document to this journal entry.
    #[must_use]
    pub fn attach(mut self, document: SourceDocument) -> Self {
        self.attachments.push(document);
        self
    }

    /// Adds a pre-constructed entry.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError::CurrencyMismatch`] if the entry uses a different currency.
    pub fn entry(mut self, entry: Entry) -> Result<Self, EntryError> {
        let actual = entry.amount().currency();
        if actual != self.currency {
            return Err(EntryError::CurrencyMismatch {
                expected: self.currency,
                actual,
            });
        }
        self.entries.push(entry);
        Ok(self)
    }

    /// Adds a debit entry for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn debit(mut self, account: &Account, amount: Amount) -> Result<Self, EntryError> {
        let entry = Entry::debit(account.clone(), Money::new(amount, self.currency))?;
        self.entries.push(entry);
        Ok(self)
    }

    /// Adds a credit entry for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn credit(mut self, account: &Account, amount: Amount) -> Result<Self, EntryError> {
        let entry = Entry::credit(account.clone(), Money::new(amount, self.currency))?;
        self.entries.push(entry);
        Ok(self)
    }

    /// Adds a debit entry with a memo for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn debit_with_memo(
        mut self,
        account: &Account,
        amount: Amount,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        let entry =
            Entry::debit_with_memo(account.clone(), Money::new(amount, self.currency), memo)?;
        self.entries.push(entry);
        Ok(self)
    }

    /// Adds a credit entry with a memo for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn credit_with_memo(
        mut self,
        account: &Account,
        amount: Amount,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        let entry =
            Entry::credit_with_memo(account.clone(), Money::new(amount, self.currency), memo)?;
        self.entries.push(entry);
        Ok(self)
    }

    /// Returns the accumulated entries.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Computes the total of all debit entries.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::NoEntries`] if the journal is empty, or an
    /// error on arithmetic overflow.
    pub fn total_debits(&self) -> Result<Money, TransactionError> {
        self.total_for(DebitOrCredit::Debit)
    }

    /// Computes the total of all credit entries.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::NoEntries`] if the journal is empty, or an
    /// error on arithmetic overflow.
    pub fn total_credits(&self) -> Result<Money, TransactionError> {
        self.total_for(DebitOrCredit::Credit)
    }

    fn total_for(&self, direction: DebitOrCredit) -> Result<Money, TransactionError> {
        if self.entries.is_empty() {
            return Err(TransactionError::NoEntries);
        }
        let total = self
            .entries
            .iter()
            .filter(|entry| entry.direction() == direction)
            .try_fold(Amount::ZERO, add_entry_amount)?;
        Ok(Money::new(total, self.currency))
    }

    /// Returns whether total debits equal total credits.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal is empty or the totals overflow.
    pub fn is_balanced(&self) -> Result<bool, TransactionError> {
        if self.entries.is_empty() {
            return Err(TransactionError::NoEntries);
        }
        let (debits, credits) = entry_totals(&self.entries)?;
        Ok(debits == credits)
    }

    /// Posts the journal once it contains at least two entries and balances.
    ///
    /// # Errors
    ///
    /// Returns a [`TransactionError`] if there are fewer than two entries,
    /// the totals overflow, or debits do not equal credits.
    pub fn post(self) -> Result<Transaction, TransactionError> {
        match self.entries.as_slice() {
            [] => return Err(TransactionError::NoEntries),
            [_] => return Err(TransactionError::SingleEntry),
            [_, _, ..] => {}
        }
        let (total_debits, total_credits) = entry_totals(&self.entries)?;
        if total_debits != total_credits {
            return Err(TransactionError::Unbalanced {
                total_debits,
                total_credits,
            });
        }

        Ok(Transaction::from_balanced_entries(
            self.date,
            self.description,
            BalancedEntries {
                entries: self.entries,
                total: Money::new(total_debits, self.currency),
            },
            self.metadata,
            self.attachments,
        ))
    }
}

fn add_entry_amount(total: Amount, entry: &Entry) -> Result<Amount, MoneyError> {
    total
        .checked_add(entry.amount().amount())
        .ok_or(MoneyError::Overflow)
}

fn entry_totals(entries: &[Entry]) -> Result<(Amount, Amount), MoneyError> {
    let mut debits = Amount::ZERO;
    let mut credits = Amount::ZERO;
    for entry in entries {
        match entry.direction() {
            DebitOrCredit::Debit => debits = add_entry_amount(debits, entry)?,
            DebitOrCredit::Credit => credits = add_entry_amount(credits, entry)?,
        }
    }
    Ok((debits, credits))
}

/// A sealed group of at least two single-currency entries and their balanced total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BalancedEntries {
    // Keep the entries contiguous for callers that use the public slice API.
    entries: Vec<Entry>,
    total: Money,
}

impl BalancedEntries {
    pub(super) fn as_slice(&self) -> &[Entry] {
        &self.entries
    }

    pub(super) const fn total(&self) -> Money {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountCode, AccountType, Amount};

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

    #[test]
    fn simple_two_entry_transaction_posts() {
        let txn = JournalEntry::new(date(2024, 1, 15), "Sale", Currency::USD)
            .debit(&cash(), Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(txn.is_ok());
    }

    #[test]
    fn multi_entry_transaction_posts() {
        let payable = make_account("2000", "Tax Payable", AccountType::Liability);

        let txn = JournalEntry::new(date(2024, 1, 15), "Sale with tax", Currency::USD)
            .debit(&cash(), Amount::new(550))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&payable, Amount::new(50))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(txn.is_ok());
    }

    #[test]
    fn unbalanced_transaction_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Bad", Currency::USD)
            .debit(&cash(), Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(300))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(matches!(result, Err(TransactionError::Unbalanced { .. })));
    }

    #[test]
    fn no_entries_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Empty", Currency::USD).post();
        assert!(matches!(result, Err(TransactionError::NoEntries)));
    }

    #[test]
    fn single_entry_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Single", Currency::USD)
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(matches!(result, Err(TransactionError::SingleEntry)));
    }

    #[test]
    fn mixed_currencies_rejected_when_ingesting_prebuilt_entry() {
        let entry =
            Entry::credit(revenue(), Money::eur(100)).unwrap_or_else(|e| panic!("test: {e}"));
        let result = JournalEntry::new(date(2024, 1, 15), "Mixed", Currency::USD).entry(entry);

        assert!(matches!(
            result,
            Err(EntryError::CurrencyMismatch {
                expected: Currency::USD,
                actual: Currency::EUR
            })
        ));
    }

    #[test]
    fn total_debits_equals_total_credits_after_post() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test", Currency::USD)
            .debit(&cash(), Amount::new(1000))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(1000))
            .unwrap_or_else(|e| panic!("test: {e}"));

        let debits = journal
            .total_debits()
            .unwrap_or_else(|e| panic!("test: {e}"));
        let credits = journal
            .total_credits()
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(debits, credits);
    }

    #[test]
    fn is_balanced_returns_true_for_balanced() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test", Currency::USD)
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert!(journal.is_balanced().unwrap_or(false));
    }

    #[test]
    fn is_balanced_returns_false_for_unbalanced() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test", Currency::USD)
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(50))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert!(!journal.is_balanced().unwrap_or(true));
    }

    #[test]
    fn with_metadata() {
        let txn = JournalEntry::new(date(2024, 1, 15), "Sale", Currency::USD)
            .with_metadata("INV-001")
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.metadata(), Some("INV-001"));
    }

    #[test]
    fn entries_accessor() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test", Currency::USD)
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(journal.entries().len(), 2);
    }

    #[test]
    fn entry_method_adds_prebuilt() {
        let entry = Entry::debit(cash(), Money::usd(100)).unwrap_or_else(|e| panic!("test: {e}"));
        let journal = JournalEntry::new(date(2024, 1, 15), "Test", Currency::USD)
            .entry(entry)
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(journal.entries().len(), 1);
    }

    #[test]
    fn posted_transaction_has_correct_debits_and_credits() {
        let txn = JournalEntry::new(date(2024, 1, 15), "Rent payment", Currency::USD)
            .debit(&expense(), Amount::new(1200))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&cash(), Amount::new(1200))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.debit_entries().count(), 1);
        assert_eq!(txn.credit_entries().count(), 1);
        assert_eq!(txn.total(), Money::usd(1200));
    }

    #[test]
    fn unbalanced_error_contains_amounts() {
        let result = JournalEntry::new(date(2024, 1, 15), "Bad", Currency::USD)
            .debit(&cash(), Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(300))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        match result {
            Err(TransactionError::Unbalanced {
                total_debits,
                total_credits,
            }) => {
                assert_eq!(total_debits, Amount::new(500));
                assert_eq!(total_credits, Amount::new(300));
            }
            other => panic!("expected Unbalanced, got {other:?}"),
        }
    }

    #[test]
    fn totals_overflow_cannot_produce_a_transaction() -> Result<(), EntryError> {
        let journal = JournalEntry::new(date(2024, 3, 15), "Overflow", Currency::USD)
            .debit(&cash(), Amount::new(i128::MAX))?
            .debit(&cash(), Amount::new(1))?
            .credit(&revenue(), Amount::new(i128::MAX))?
            .credit(&revenue(), Amount::new(1))?;
        assert!(matches!(
            journal.post(),
            Err(TransactionError::Money(MoneyError::Overflow))
        ));
        Ok(())
    }

    #[test]
    fn posted_transaction_preserves_date() {
        let txn = JournalEntry::new(date(2024, 3, 15), "Sale", Currency::USD)
            .debit(&cash(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Amount::new(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.date(), date(2024, 3, 15));
    }
}
