use chrono::NaiveDate;

use crate::types::{Account, DebitOrCredit, Entry, EntryError, Money, SourceDocument};

use super::transaction::{Transaction, TransactionError, sum_entries_by_direction};

/// A builder for constructing balanced transactions.
///
/// Entries are accumulated and validated when [`post`](Self::post) is called.
/// The fundamental invariant — total debits equal total credits —
/// is enforced at post time.
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
///     )
///     .debit(&cash, Money::usd(500_00)).unwrap()
///     .credit(&revenue, Money::usd(500_00)).unwrap()
///     .post()
///     .unwrap();
///
/// assert_eq!(txn.description(), "Sale of goods");
/// ```
pub struct JournalEntry {
    date: NaiveDate,
    description: String,
    entries: Vec<Entry>,
    metadata: Option<String>,
    attachments: Vec<SourceDocument>,
}

impl JournalEntry {
    /// Creates a new journal entry with the given date and description.
    #[must_use]
    pub fn new(date: NaiveDate, description: impl Into<String>) -> Self {
        Self {
            date,
            description: description.into(),
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
    #[must_use]
    pub fn entry(mut self, entry: Entry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Adds a debit entry for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn debit(mut self, account: &Account, amount: Money) -> Result<Self, EntryError> {
        let entry = Entry::debit(account.clone(), amount)?;
        self.entries.push(entry);
        Ok(self)
    }

    /// Adds a credit entry for the given account and amount.
    ///
    /// # Errors
    ///
    /// Returns [`EntryError`] if the amount is zero or negative.
    pub fn credit(mut self, account: &Account, amount: Money) -> Result<Self, EntryError> {
        let entry = Entry::credit(account.clone(), amount)?;
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
        amount: Money,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        let entry = Entry::debit_with_memo(account.clone(), amount, memo)?;
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
        amount: Money,
        memo: impl Into<String>,
    ) -> Result<Self, EntryError> {
        let entry = Entry::credit_with_memo(account.clone(), amount, memo)?;
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
    /// Returns [`TransactionError::NoEntries`] if no entries have been added,
    /// or an error on arithmetic overflow.
    pub fn total_debits(&self) -> Result<Money, TransactionError> {
        if self.entries.is_empty() {
            return Err(TransactionError::NoEntries);
        }
        Ok(sum_entries_by_direction(
            &self.entries,
            DebitOrCredit::Debit,
        )?)
    }

    /// Computes the total of all credit entries.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::NoEntries`] if no entries have been added,
    /// or an error on arithmetic overflow.
    pub fn total_credits(&self) -> Result<Money, TransactionError> {
        if self.entries.is_empty() {
            return Err(TransactionError::NoEntries);
        }
        Ok(sum_entries_by_direction(
            &self.entries,
            DebitOrCredit::Credit,
        )?)
    }

    /// Returns whether total debits equal total credits.
    ///
    /// # Errors
    ///
    /// Returns an error on arithmetic overflow or currency mismatch.
    pub fn is_balanced(&self) -> Result<bool, TransactionError> {
        let debits = self.total_debits()?;
        let credits = self.total_credits()?;
        Ok(debits == credits)
    }

    /// Validates and posts this journal entry, producing a [`Transaction`].
    ///
    /// # Validation Rules
    ///
    /// 1. Must have at least two entries
    /// 2. All entries must use the same currency
    /// 3. Total debits must equal total credits
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError`] if any validation rule is violated.
    pub fn post(self) -> Result<Transaction, TransactionError> {
        // Check minimum entry count
        match self.entries.len() {
            0 => return Err(TransactionError::NoEntries),
            1 => return Err(TransactionError::SingleEntry),
            _ => {}
        }

        // Check single currency
        let first_currency = self.entries[0].amount().currency();
        let mut seen_currencies = vec![first_currency];

        for entry in &self.entries[1..] {
            let c = entry.amount().currency();
            if c != first_currency && !seen_currencies.contains(&c) {
                seen_currencies.push(c);
            }
        }

        if seen_currencies.len() > 1 {
            return Err(TransactionError::CurrencyMismatch {
                currencies: seen_currencies,
            });
        }

        // Check balance
        let total_debits = sum_entries_by_direction(&self.entries, DebitOrCredit::Debit)?;
        let total_credits = sum_entries_by_direction(&self.entries, DebitOrCredit::Credit)?;

        if total_debits != total_credits {
            return Err(TransactionError::Unbalanced {
                total_debits: total_debits.amount(),
                total_credits: total_credits.amount(),
            });
        }

        Ok(Transaction {
            date: self.date,
            description: self.description,
            entries: self.entries,
            metadata: self.metadata,
            attachments: self.attachments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountCode, AccountType, Amount};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
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
        let txn = JournalEntry::new(date(2024, 1, 15), "Sale")
            .debit(&cash(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(txn.is_ok());
    }

    #[test]
    fn multi_entry_transaction_posts() {
        let payable = make_account("2000", "Tax Payable", AccountType::Liability);

        let txn = JournalEntry::new(date(2024, 1, 15), "Sale with tax")
            .debit(&cash(), Money::usd(550))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&payable, Money::usd(50))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(txn.is_ok());
    }

    #[test]
    fn unbalanced_transaction_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Bad")
            .debit(&cash(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(300))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(matches!(result, Err(TransactionError::Unbalanced { .. })));
    }

    #[test]
    fn no_entries_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Empty").post();
        assert!(matches!(result, Err(TransactionError::NoEntries)));
    }

    #[test]
    fn single_entry_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Single")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(matches!(result, Err(TransactionError::SingleEntry)));
    }

    #[test]
    fn mixed_currencies_rejected() {
        let result = JournalEntry::new(date(2024, 1, 15), "Mixed")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::eur(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post();

        assert!(matches!(
            result,
            Err(TransactionError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn total_debits_equals_total_credits_after_post() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test")
            .debit(&cash(), Money::usd(1000))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(1000))
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
        let journal = JournalEntry::new(date(2024, 1, 15), "Test")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert!(journal.is_balanced().unwrap_or(false));
    }

    #[test]
    fn is_balanced_returns_false_for_unbalanced() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(50))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert!(!journal.is_balanced().unwrap_or(true));
    }

    #[test]
    fn with_metadata() {
        let txn = JournalEntry::new(date(2024, 1, 15), "Sale")
            .with_metadata("INV-001")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.metadata(), Some("INV-001"));
    }

    #[test]
    fn entries_accessor() {
        let journal = JournalEntry::new(date(2024, 1, 15), "Test")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(journal.entries().len(), 2);
    }

    #[test]
    fn entry_method_adds_prebuilt() {
        let entry = Entry::debit(cash(), Money::usd(100)).unwrap_or_else(|e| panic!("test: {e}"));
        let journal = JournalEntry::new(date(2024, 1, 15), "Test").entry(entry);
        assert_eq!(journal.entries().len(), 1);
    }

    #[test]
    fn posted_transaction_has_correct_debits_and_credits() {
        let txn = JournalEntry::new(date(2024, 1, 15), "Rent payment")
            .debit(&expense(), Money::usd(1200))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&cash(), Money::usd(1200))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.debit_entries().count(), 1);
        assert_eq!(txn.credit_entries().count(), 1);
        assert_eq!(
            txn.total().unwrap_or_else(|e| panic!("test: {e}")),
            Money::usd(1200)
        );
    }

    #[test]
    fn unbalanced_error_contains_amounts() {
        let result = JournalEntry::new(date(2024, 1, 15), "Bad")
            .debit(&cash(), Money::usd(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(300))
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
    fn posted_transaction_preserves_date() {
        let txn = JournalEntry::new(date(2024, 3, 15), "Sale")
            .debit(&cash(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue(), Money::usd(100))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(txn.date(), date(2024, 3, 15));
    }
}
