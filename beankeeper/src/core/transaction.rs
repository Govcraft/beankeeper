use core::fmt;

use chrono::NaiveDate;

use crate::types::{Account, Amount, Currency, Entry, Money, MoneyError, SourceDocument};

use super::journal_entry::BalancedEntries;

/// Error type for transaction validation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransactionError {
    /// The transaction has no entries.
    NoEntries,
    /// The transaction has only one entry (need at least two).
    SingleEntry,
    /// Total debits do not equal total credits.
    Unbalanced {
        /// Total of all debit entries.
        total_debits: Amount,
        /// Total of all credit entries.
        total_credits: Amount,
    },
    /// An error occurred during monetary arithmetic.
    Money(MoneyError),
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEntries => write!(f, "transaction must have at least two entries"),
            Self::SingleEntry => write!(f, "transaction must have at least two entries, got one"),
            Self::Unbalanced {
                total_debits,
                total_credits,
            } => write!(
                f,
                "transaction is unbalanced: debits={total_debits}, credits={total_credits}"
            ),
            Self::Money(e) => write!(f, "monetary arithmetic error: {e}"),
        }
    }
}

impl std::error::Error for TransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Money(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MoneyError> for TransactionError {
    fn from(err: MoneyError) -> Self {
        Self::Money(err)
    }
}

/// A validated, balanced double-entry transaction.
///
/// [`JournalEntry::post`] produces a transaction only after validating that at
/// least two entries exist and that total debits equal total credits. The
/// transaction holds the sealed entries and their shared currency and total.
///
/// [`JournalEntry::post`]: super::JournalEntry::post
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    date: NaiveDate,
    description: String,
    entries: BalancedEntries,
    metadata: Option<String>,
    attachments: Vec<SourceDocument>,
}

impl Transaction {
    pub(super) fn from_balanced_entries(
        date: NaiveDate,
        description: String,
        entries: BalancedEntries,
        metadata: Option<String>,
        attachments: Vec<SourceDocument>,
    ) -> Self {
        Self {
            date,
            description,
            entries,
            metadata,
            attachments,
        }
    }

    /// Returns the transaction date.
    #[must_use]
    pub fn date(&self) -> NaiveDate {
        self.date
    }

    /// Returns the transaction description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns all entries in this transaction.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        self.entries.as_slice()
    }

    /// Returns optional metadata attached to this transaction.
    #[must_use]
    pub fn metadata(&self) -> Option<&str> {
        self.metadata.as_deref()
    }

    /// Returns source documents attached to this transaction.
    #[must_use]
    pub fn attachments(&self) -> &[SourceDocument] {
        &self.attachments
    }

    /// Returns an iterator over debit entries.
    pub fn debit_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries().iter().filter(|e| e.is_debit())
    }

    /// Returns an iterator over credit entries.
    pub fn credit_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries().iter().filter(|e| e.is_credit())
    }

    /// Returns the currency shared by every entry.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.total().currency()
    }

    /// Returns the total of either side of the balanced transaction.
    #[must_use]
    pub const fn total(&self) -> Money {
        self.entries.total()
    }

    /// Returns `true` if this transaction involves the given account.
    #[must_use]
    pub fn involves_account(&self, account: &Account) -> bool {
        self.entries().iter().any(|e| e.account() == account)
    }

    /// Returns the net money amount for a specific account in this transaction,
    /// or `None` if the account is not involved.
    ///
    /// The sign follows the account's normal balance convention:
    /// positive means the account increased, negative means it decreased.
    ///
    /// # Errors
    ///
    /// Returns a [`MoneyError`] if arithmetic overflows.
    pub fn amount_for_account(&self, account: &Account) -> Result<Option<Money>, MoneyError> {
        let mut total = None;
        for entry in self
            .entries()
            .iter()
            .filter(|entry| entry.account() == account)
        {
            total = Some(
                total
                    .unwrap_or(Amount::ZERO)
                    .checked_add(entry.signed_amount())
                    .ok_or(MoneyError::Overflow)?,
            );
        }
        Ok(total.map(|amount| Money::new(amount, self.currency())))
    }
}

impl fmt::Display for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Transaction [{}]: {}", self.date, self.description)?;
        for entry in self.entries() {
            writeln!(f, "  {entry}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::JournalEntry;
    use crate::types::{AccountCode, AccountType};

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

    fn make_balanced_transaction() -> Transaction {
        let cash = make_account("1000", "Cash", AccountType::Asset);
        let revenue = make_account("4000", "Revenue", AccountType::Revenue);

        JournalEntry::new(date(2024, 1, 15), "Test sale", Currency::USD)
            .with_metadata("ref-001")
            .debit(&cash, Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .credit(&revenue, Amount::new(500))
            .unwrap_or_else(|e| panic!("test: {e}"))
            .post()
            .unwrap_or_else(|e| panic!("test: {e}"))
    }

    #[test]
    fn description_preserved() {
        let txn = make_balanced_transaction();
        assert_eq!(txn.description(), "Test sale");
    }

    #[test]
    fn entries_preserved() {
        let txn = make_balanced_transaction();
        assert_eq!(txn.entries().len(), 2);
    }

    #[test]
    fn metadata_preserved() {
        let txn = make_balanced_transaction();
        assert_eq!(txn.metadata(), Some("ref-001"));
    }

    #[test]
    fn debit_entries_filter() {
        let txn = make_balanced_transaction();
        assert_eq!(txn.debit_entries().count(), 1);
    }

    #[test]
    fn credit_entries_filter() {
        let txn = make_balanced_transaction();
        assert_eq!(txn.credit_entries().count(), 1);
    }

    #[test]
    fn involves_account() {
        let txn = make_balanced_transaction();
        let cash = make_account("1000", "Cash", AccountType::Asset);
        let other = make_account("9999", "Other", AccountType::Expense);
        assert!(txn.involves_account(&cash));
        assert!(!txn.involves_account(&other));
    }

    #[test]
    fn total_returns_debit_sum() {
        let txn = make_balanced_transaction();
        let total = txn.total();
        assert_eq!(total, Money::usd(500));
    }

    #[test]
    fn amount_for_account_returns_signed() {
        let txn = make_balanced_transaction();
        let cash = make_account("1000", "Cash", AccountType::Asset);
        let result = txn
            .amount_for_account(&cash)
            .unwrap_or_else(|e| panic!("test: {e}"));
        // Debit on asset (debit-normal) -> positive
        assert_eq!(result, Some(Money::new(Amount::new(500), Currency::USD)));
    }

    #[test]
    fn amount_for_account_not_involved() {
        let txn = make_balanced_transaction();
        let other = make_account("9999", "Other", AccountType::Expense);
        let result = txn
            .amount_for_account(&other)
            .unwrap_or_else(|e| panic!("test: {e}"));
        assert_eq!(result, None);
    }

    #[test]
    fn display_format() {
        let txn = make_balanced_transaction();
        let display = format!("{txn}");
        assert!(display.contains("Test sale"));
        assert!(display.contains("DR"));
        assert!(display.contains("CR"));
    }

    #[test]
    fn error_display_no_entries() {
        assert!(format!("{}", TransactionError::NoEntries).contains("at least two"));
    }

    #[test]
    fn error_display_unbalanced() {
        let err = TransactionError::Unbalanced {
            total_debits: Amount::new(500),
            total_credits: Amount::new(300),
        };
        assert!(format!("{err}").contains("unbalanced"));
    }
}
