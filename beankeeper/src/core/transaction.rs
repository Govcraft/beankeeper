use core::fmt;

use chrono::NaiveDate;

use crate::types::{Account, Amount, Currency, DebitOrCredit, Entry, Money, MoneyError};

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
    /// Entries use more than one currency.
    CurrencyMismatch {
        /// The distinct currencies found in the entries.
        currencies: Vec<Currency>,
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
            Self::CurrencyMismatch { currencies } => {
                write!(f, "transaction entries use multiple currencies: ")?;
                for (i, c) in currencies.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{c}")?;
                }
                Ok(())
            }
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
/// A `Transaction` can only be created through [`JournalEntry::post`],
/// which guarantees that total debits equal total credits.
///
/// This type is intentionally **not** constructible outside this crate
/// to preserve the balance invariant.
///
/// [`JournalEntry::post`]: super::JournalEntry::post
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) entries: Vec<Entry>,
    pub(crate) metadata: Option<String>,
}

impl Transaction {
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
        &self.entries
    }

    /// Returns optional metadata attached to this transaction.
    #[must_use]
    pub fn metadata(&self) -> Option<&str> {
        self.metadata.as_deref()
    }

    /// Returns an iterator over debit entries.
    pub fn debit_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.is_debit())
    }

    /// Returns an iterator over credit entries.
    pub fn credit_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.is_credit())
    }

    /// Returns the total amount of the transaction (debits or credits, since they're equal).
    ///
    /// # Errors
    ///
    /// Returns a [`MoneyError`] if arithmetic overflows.
    pub fn total(&self) -> Result<Money, MoneyError> {
        sum_entries_by_direction(&self.entries, DebitOrCredit::Debit)
    }

    /// Returns `true` if this transaction involves the given account.
    #[must_use]
    pub fn involves_account(&self, account: &Account) -> bool {
        self.entries.iter().any(|e| e.account() == account)
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
        let relevant: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.account() == account)
            .collect();

        if relevant.is_empty() {
            return Ok(None);
        }

        let currency = relevant[0].amount().currency();
        let mut total = Amount::ZERO;

        for entry in &relevant {
            let signed = entry.signed_amount();
            total = total.checked_add(signed).ok_or(MoneyError::Overflow)?;
        }

        Ok(Some(Money::new(total, currency)))
    }
}

impl fmt::Display for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Transaction [{}]: {}", self.date, self.description)?;
        for entry in &self.entries {
            writeln!(f, "  {entry}")?;
        }
        Ok(())
    }
}

/// Sums the amounts of entries matching the given direction.
///
/// # Panics
///
/// Panics if `entries` is empty. All internal callers guarantee at least
/// two entries (validated by [`JournalEntry::post`] or guarded before calling).
pub(crate) fn sum_entries_by_direction(
    entries: &[Entry],
    direction: DebitOrCredit,
) -> Result<Money, MoneyError> {
    // Safety: all callers guarantee non-empty entries — Transaction always
    // has ≥2 entries, JournalEntry::total_debits/total_credits guard on
    // is_empty(), and post() checks len ≥ 2 before calling.
    let currency = entries[0].amount().currency();

    let mut total = Money::from_minor(0, currency);

    for entry in entries.iter().filter(|e| e.direction() == direction) {
        total = total.checked_add(entry.amount())?;
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountCode, AccountType};

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

    fn make_balanced_transaction() -> Transaction {
        let cash = make_account("1000", "Cash", AccountType::Asset);
        let revenue = make_account("4000", "Revenue", AccountType::Revenue);

        Transaction {
            date: date(2024, 1, 15),
            description: "Test sale".to_owned(),
            entries: vec![
                Entry::debit(cash, Money::usd(500)).unwrap_or_else(|e| panic!("test: {e}")),
                Entry::credit(revenue, Money::usd(500)).unwrap_or_else(|e| panic!("test: {e}")),
            ],
            metadata: Some("ref-001".to_owned()),
        }
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
        let total = txn.total().unwrap_or_else(|e| panic!("test: {e}"));
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
