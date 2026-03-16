//! Fundamental accounting types.
//!
//! This module contains the primitive types used throughout the crate:
//! monetary amounts, currencies, account types, and entries.

pub mod account;
pub mod account_code;
pub mod account_type;
pub mod amount;
pub mod currency;
pub mod debit_credit;
pub mod entry;
pub mod money;

pub use account::Account;
pub use account_code::{AccountCode, AccountCodeError};
pub use account_type::{AccountType, AccountTypeError};
pub use amount::{Amount, AmountError};
pub use currency::{Currency, CurrencyError};
pub use debit_credit::{DebitCreditError, DebitOrCredit};
pub use entry::{Entry, EntryError};
pub use money::{Money, MoneyError};
