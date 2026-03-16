# Beankeeper

Idiomatic, ergonomic library of primitives for professional double-entry accounting in Rust.

Beankeeper enforces the fundamental accounting equation at the type level: every posted transaction guarantees that total debits equal total credits. Unbalanced transactions cannot exist.

## Features

- **Correctness by construction** -- the builder pattern validates transactions at post time, rejecting unbalanced entries, zero amounts, and mixed currencies
- **Exact arithmetic** -- all monetary values use `i128` minor-unit representation (cents, pence, yen), eliminating floating-point rounding errors
- **Multi-currency support** -- ISO 4217 currencies with correct minor-unit precision (USD 2 decimals, JPY 0 decimals, BHD 3 decimals)
- **Zero dependencies** -- no external crates; `unsafe` code is forbidden via `#[deny(unsafe_code)]`
- **Comprehensive reporting** -- generate trial balances, query account balances, and filter by account type

## Quick Start

```rust
use beankeeper::prelude::*;

// Define accounts
let cash = Account::new(
    AccountCode::new("1000").unwrap(),
    "Cash",
    AccountType::Asset,
);
let revenue = Account::new(
    AccountCode::new("4000").unwrap(),
    "Sales Revenue",
    AccountType::Revenue,
);

// Build and post a balanced transaction
let txn = JournalEntry::new("Cash sale")
    .debit(&cash, Money::usd(50_00))
    .unwrap()
    .credit(&revenue, Money::usd(50_00))
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.description(), "Cash sale");
```

## Installation

Add beankeeper to your project:

```sh
cargo add beankeeper
```

**Minimum supported Rust version**: 1.85 (Rust 2024 edition).

## Core Concepts

Beankeeper models the complete double-entry bookkeeping cycle: define accounts, record journal entries, post transactions to a ledger, and generate reports. Each stage builds on the previous one, and the library validates data at every boundary.

### The Accounting Equation

Every transaction enforces the fundamental equation:

```text
Assets + Expenses = Liabilities + Equity + Revenue
 (Debit normal)       (Credit normal)
```

The five account types each have a normal balance direction. Debiting an asset increases it; crediting a revenue account increases it. Beankeeper encodes these rules so that `signed_amount()` on any entry returns a positive value when the entry increases the account and a negative value when it decreases.

### Accounts

An `Account` combines three elements: a validated `AccountCode`, a human-readable name, and an `AccountType` that determines its normal balance behavior.

```rust
use beankeeper::prelude::*;

let cash = Account::new(
    AccountCode::new("1000").unwrap(),
    "Cash",
    AccountType::Asset,
);

// Account codes support hierarchical numbering
let parent = AccountCode::new("1000").unwrap();
let child = AccountCode::new("1000.10").unwrap();
assert!(parent.is_parent_of(&child));
```

Account codes accept digits, hyphens, and dots, enabling standard chart-of-accounts numbering schemes like `1000`, `1000.10`, or `1-1000`.

### Transactions

The `JournalEntry` builder accumulates debit and credit entries, then validates them when you call `post()`. Validation enforces three rules: at least two entries, a single currency, and balanced totals.

```rust
use beankeeper::prelude::*;

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);
let tax_payable = Account::new(AccountCode::new("2100").unwrap(), "Sales Tax", AccountType::Liability);

// Multi-leg transaction: $108 sale with 8% tax
let txn = JournalEntry::new("Sale with sales tax")
    .debit(&cash, Money::usd(108_00))
    .unwrap()
    .credit(&revenue, Money::usd(100_00))
    .unwrap()
    .credit(&tax_payable, Money::usd(8_00))
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.entries().len(), 3);
```

Transactions can carry optional metadata for reference numbers, invoice IDs, or other tracking information:

```rust
use beankeeper::prelude::*;

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);

let txn = JournalEntry::new("Sale")
    .with_metadata("INV-2024-001")
    .debit(&cash, Money::usd(250_00))
    .unwrap()
    .credit(&revenue, Money::usd(250_00))
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.metadata(), Some("INV-2024-001"));
```

### The General Ledger

The `Ledger` is an append-only store for posted transactions, following standard accounting practice where corrections are made via reversing entries rather than deletion. It provides balance queries across all posted transactions.

```rust
use beankeeper::prelude::*;

let mut ledger = Ledger::new();

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);
let rent = Account::new(AccountCode::new("5000").unwrap(), "Rent", AccountType::Expense);

// Post a sale
let sale = JournalEntry::new("Sale")
    .debit(&cash, Money::usd(1000_00))
    .unwrap()
    .credit(&revenue, Money::usd(1000_00))
    .unwrap()
    .post()
    .unwrap();
ledger.post(sale);

// Pay rent
let payment = JournalEntry::new("Rent")
    .debit(&rent, Money::usd(500_00))
    .unwrap()
    .credit(&cash, Money::usd(500_00))
    .unwrap()
    .post()
    .unwrap();
ledger.post(payment);

// Query balances
let cash_balance = ledger.balance_for(&cash).unwrap();
assert_eq!(cash_balance, Amount::new(500_00)); // 1000 - 500

assert!(ledger.is_balanced().unwrap());
```

### Reporting

The `TrialBalance` report lists all accounts with their debit and credit totals, serving as a basic integrity check on the ledger.

```rust
use beankeeper::prelude::*;

let mut ledger = Ledger::new();

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);

let txn = JournalEntry::new("Sale")
    .debit(&cash, Money::usd(100_00))
    .unwrap()
    .credit(&revenue, Money::usd(100_00))
    .unwrap()
    .post()
    .unwrap();
ledger.post(txn);

let tb = ledger.trial_balance().unwrap();
assert!(tb.is_balanced());
assert_eq!(tb.total_debits(), tb.total_credits());

// Filter by account type
let assets = tb.accounts_by_type(AccountType::Asset);
assert_eq!(assets.len(), 1);
```

## Design Principles

### Why Integer Arithmetic

Financial calculations require exact results. Floating-point types (`f64`) introduce rounding errors that compound across thousands of transactions. Beankeeper stores all monetary values as `i128` counts of minor currency units (cents for USD, pence for GBP, yen for JPY). This ensures:

- Deterministic results across platforms
- No rounding surprises
- Correct behavior for all standard currencies, including those with 0 or 3 decimal places

### Why the Builder Pattern

The `JournalEntry` builder separates construction from validation. You accumulate entries freely, then `post()` performs all validation at once. This design prevents partially-constructed transactions from entering the ledger and makes the API hard to misuse: a `Transaction` value proves that balance was checked.

### Why Append-Only

The `Ledger` does not support deleting or modifying posted transactions. In professional accounting, corrections are recorded as new reversing entries. This preserves a complete audit trail and matches real-world accounting practice.

## Multi-Currency Support

Beankeeper includes nine ISO 4217 currencies with correct minor-unit precision:

| Currency | Code | Minor Units |
|----------|------|-------------|
| US Dollar | `USD` | 2 (cents) |
| Euro | `EUR` | 2 (cents) |
| British Pound | `GBP` | 2 (pence) |
| Japanese Yen | `JPY` | 0 |
| Swiss Franc | `CHF` | 2 |
| Canadian Dollar | `CAD` | 2 |
| Australian Dollar | `AUD` | 2 |
| Bahraini Dinar | `BHD` | 3 |
| Kuwaiti Dinar | `KWD` | 3 |

Arithmetic between different currencies is rejected at the type level. A transaction must use a single currency; attempting to mix USD and EUR entries produces a `TransactionError::CurrencyMismatch`.

```rust
use beankeeper::prelude::*;

// JPY has no minor units
let amount = Money::jpy(50000);
assert_eq!(format!("{amount}"), "JPY 50000");

// EUR uses cents
let amount = Money::eur(12_50);
assert_eq!(format!("{amount}"), "EUR 12.50");
```

## Error Handling

All fallible operations return `Result` types with descriptive error variants. The top-level `BeanError` enum aggregates all domain errors, enabling ergonomic use of the `?` operator:

```rust
use beankeeper::prelude::*;

fn record_sale(ledger: &mut Ledger) -> Result<(), BeanError> {
    let cash = Account::new(AccountCode::new("1000")?, "Cash", AccountType::Asset);
    let revenue = Account::new(AccountCode::new("4000")?, "Revenue", AccountType::Revenue);

    let txn = JournalEntry::new("Sale")
        .debit(&cash, Money::usd(50_00))?
        .credit(&revenue, Money::usd(50_00))?
        .post()?;

    ledger.post(txn);
    Ok(())
}
```

Specific error types cover each domain:

- **Transaction errors** -- `Unbalanced`, `CurrencyMismatch`, `NoEntries`, `SingleEntry`
- **Entry errors** -- `ZeroAmount`, `NegativeAmount`
- **Account code errors** -- `Empty`, `InvalidCharacter`
- **Money errors** -- `CurrencyMismatch`, `Overflow`
- **Currency errors** -- `InvalidCode`, `UnknownCode`

## Safety and Quality

Beankeeper applies strict quality standards:

- `#[deny(unsafe_code)]` -- no unsafe Rust anywhere in the crate
- `#[deny(clippy::unwrap_used)]` and `#[deny(clippy::expect_used)]` -- fallible operations always use proper error handling
- `#[warn(clippy::pedantic)]` -- pedantic linting enabled
- Zero external dependencies
- 204 tests covering unit, integration, and real-world accounting scenarios

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
