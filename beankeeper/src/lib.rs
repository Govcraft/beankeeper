//! # Beankeeper
//!
//! Idiomatic, ergonomic library of primitives for professional double-entry accounting.
//!
//! ## Core Invariant
//!
//! Every [`core::Transaction`] enforces the fundamental accounting equation:
//! **total debits must equal total credits**.
//!
//! ## Quick Start
//!
//! ```
//! use beankeeper::prelude::*;
//!
//! // Create accounts
//! let cash = Account::new(
//!     AccountCode::new("1000").unwrap(),
//!     "Cash",
//!     AccountType::Asset,
//! );
//! let revenue = Account::new(
//!     AccountCode::new("4000").unwrap(),
//!     "Sales Revenue",
//!     AccountType::Revenue,
//! );
//!
//! // Build a balanced transaction
//! let txn = JournalEntry::new("Sale of goods")
//!     .debit(&cash, Money::usd(500_00))
//!     .unwrap()
//!     .credit(&revenue, Money::usd(500_00))
//!     .unwrap()
//!     .post()
//!     .unwrap();
//!
//! assert_eq!(txn.description(), "Sale of goods");
//! ```
//!
//! ## Crate Structure
//!
//! - [`types`] — Fundamental accounting types (amounts, currencies, accounts, entries)
//! - [`core`] — Transaction building, validation, and the general ledger
//! - [`reporting`] — Trial balances and account balance summaries
//! - [`error`] — Top-level error type aggregating all domain errors

pub mod core;
pub mod error;
pub mod reporting;
pub mod types;

/// Convenience re-exports for common usage.
pub mod prelude {
    pub use crate::core::{JournalEntry, Ledger, Transaction, TransactionError};
    pub use crate::error::BeanError;
    pub use crate::reporting::{AccountBalance, TrialBalance};
    pub use crate::types::{
        Account, AccountCode, AccountCodeError, AccountType, AccountTypeError, Amount, AmountError,
        Currency, CurrencyError, DebitCreditError, DebitOrCredit, Entry, EntryError, Money,
        MoneyError,
    };
}
