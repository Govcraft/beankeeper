//! Core accounting logic.
//!
//! This module contains the transaction builder ([`JournalEntry`]),
//! validated transactions ([`Transaction`]), and the general ledger ([`Ledger`]).

pub mod journal_entry;
pub mod ledger;
pub mod transaction;

pub use journal_entry::JournalEntry;
pub use ledger::Ledger;
pub use transaction::{Transaction, TransactionError};
