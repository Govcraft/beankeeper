//! Core accounting logic.
//!
//! This module contains the transaction builder ([`JournalEntry`]),
//! validated transactions ([`Transaction`]), and the general ledger ([`Ledger`]).

pub mod journal_entry;
pub mod ledger;
pub mod recurrence;
pub mod transaction;

pub use journal_entry::JournalEntry;
pub use ledger::Ledger;
pub use recurrence::{Frequency, RecurrenceError, occurrence_date};
pub use transaction::{Transaction, TransactionError};
