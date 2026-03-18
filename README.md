# Beankeeper

Professional double-entry accounting in Rust -- a library of accounting primitives and a CLI backed by SQLCipher-encrypted SQLite.

## Workspace

This repository contains two crates:

| Crate | Description |
|-------|-------------|
| [`beankeeper`](https://crates.io/crates/beankeeper) | Library of accounting primitives: amounts, currencies, accounts, entries, dated transactions, document attachments, idempotency keys, tax categories, ledger, and reporting |
| `beankeeper-cli` | CLI binary (`bk`) providing SQLite-backed multi-tenant accounting with encryption, content-addressed document storage, idempotency, tax categorisation, three output formats, and scriptable stdin/stdout |

## Install the CLI

**Arch Linux (AUR)**:
```sh
yay -S bk-bin
```

**From GitHub Releases** (Linux x86_64, Linux aarch64, macOS Intel, macOS Apple Silicon):
```sh
# Download the latest release for your platform
curl -sL https://github.com/Govcraft/beankeeper/releases/latest/download/bk-0.1.0-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo install -m755 bk-*/bk /usr/local/bin/
```

**From source**:
```sh
cargo install --git https://github.com/Govcraft/beankeeper beankeeper-cli
```

## CLI Quick Start

```sh
# Create a database
bk init

# Set up companies
bk company create personal "Personal Finances" --description "Day-to-day expenses"
bk company create govcraft "GovCraft LLC" --description "Consulting entity"

# Create accounts
bk --company personal account create 1000 "Cash" --type asset
bk --company personal account create 4000 "Revenue" --type revenue
bk --company personal account create 5000 "Rent" --type expense

# Post a transaction (amounts are in dollars, not cents)
bk --company personal txn post -d "March rent" --date 2026-03-01 \
  --debit 5000:1200 --credit 1000:1200

# Split entries with per-line memos
bk --company personal txn post -d "Paycheck" \
  --debit "1000:3800:Net pay" \
  --credit "4000:5000:Gross salary" \
  --debit "5100:1200:Federal tax"

# Reports
bk --company personal report trial-balance
bk --company personal report balance --account 1000
bk --company personal report income-statement --from 2026-01-01 --to 2026-12-31
```

### Intercompany Transactions

```sh
# Post one side
bk --company govcraft txn post -d "Owner draw" \
  --debit 3000:500 --credit 1000:500

# Post the mirror entry, linking bidirectionally
bk --company personal txn post -d "Capital from GovCraft" \
  --debit 1000:500 --credit 3100:500 --correlate 1

# Verify no orphaned links
bk txn reconcile
```

### Idempotency Keys

When `bk` is driven by AI agents or import scripts, retries can cause duplicate transactions. The `--reference` flag provides a deterministic idempotency key -- same reference, same company, always maps to the same transaction:

```sh
# First post succeeds
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95

# Retry is rejected with the existing transaction ID
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95
# → error: transaction with reference 'chase-2026-03-15-001' already exists (id: 1)
```

References are hashed into deterministic `txnref_`-prefixed keys. Transactions without `--reference` are unrestricted.

### Document Attachments

Link source documents (receipts, invoices, statements) to transactions. Files are stored in a content-addressed directory alongside the database, with SHA-256 integrity verification:

```sh
# Attach a receipt to an existing transaction
bk --company personal txn attach 1 receipt.pdf --type receipt

# View transaction with attachments
bk --company personal txn show 1
```

Supported document types: `receipt`, `invoice`, `statement`, `contract`, `other`.

### Tax Categories

Tag entries with free-form tax categories that map to Schedule C lines (or any tax form). Categories can be set per-entry or inherited from an account default:

```sh
# Set a default tax category on an account
bk --company personal account create 5100 "Meals" --type expense \
  --default-tax-category "sched-c:24b"

# Override per-entry at posting time
bk --company personal txn post -d "Client lunch" \
  --debit 5100:25 --credit 1000:25 \
  --tax 5100=sched-c:24b

# View categorised entries
bk --company personal txn show 1

# Summarise by tax category for a tax year
bk --company personal report tax-summary --from 2026-01-01 --to 2026-12-31
```

Resolution order: explicit `--tax` flag > account `--default-tax-category` > none. Categories are free-form strings -- no tax-year specifics are baked into the schema.

### Querying the Ledger

`bk txn list` (aliased as `bk txn search`) supports rich filtering so agents and scripts get back precisely the data they need without dumping the entire ledger:

```sh
# Search transactions by description
bk --company personal txn list --description "AWS" --from 2026-01-01 --json

# Find large expenses (amounts are in dollars, not cents)
bk --company personal txn list --account 5000 --amount-gt 500 --json

# Count matching transactions without fetching them
bk --company personal txn search --description "payroll" --count --json
# → {"ok": true, "meta": {...}, "data": {"count": 12}}

# Filter by tax category, direction, currency, reference, or metadata
bk --company personal txn list --tax-category "sched-c:24b" --direction debit --json
bk --company personal txn list --currency MXN --json
bk --company personal txn list --reference "chase-2026-03-15-001" --json
bk --company personal txn list --metadata "vendor" --json
```

All filters can be combined. Amount filters use the `--currency` value (or `BEANKEEPER_CURRENCY` env, defaulting to USD) for major-to-minor unit conversion.

`bk account list` can include balances in a single query:

```sh
# List expense accounts with their debit/credit totals
bk --company personal account list --type expense --with-balances --json

# Search accounts by name, with balances as of a date
bk --company personal account list --name "Cash" --with-balances --as-of 2026-06-30 --json
```

### Output Formats

Every command supports `--format table` (default), `--format json`, and `--format csv`. Use `--json` as shorthand.

```sh
# Pipe JSON to jq (data is inside the envelope's "data" field)
bk --company personal report trial-balance --json | jq '.data.accounts[] | select(.type == "asset")'

# CSV for spreadsheets
bk --company personal txn list --format csv > transactions.csv

# Machine-readable for scripts (exit codes: 0=ok, 3=validation error)
bk txn reconcile --json || echo "Orphaned correlations found"
```

### JSON Envelope

All JSON output follows a uniform envelope contract for reliable programmatic consumption:

**Success:**
```json
{
  "ok": true,
  "meta": {
    "command": "company.list",
    "timestamp": "2026-03-18T15:30:00Z"
  },
  "data": [{"slug": "acme", "name": "Acme Corp"}]
}
```

**Error:**
```json
{
  "ok": false,
  "meta": {
    "command": "account.show",
    "company": "acme",
    "timestamp": "2026-03-18T15:30:00Z"
  },
  "error": {
    "code": "NOT_FOUND",
    "message": "account '9999' not found"
  }
}
```

The `meta.command` field uses dot notation (`company.list`, `txn.post`, `report.trial-balance`). The `meta.company` field is present when the command operates on a specific company. Error codes are: `USAGE`, `VALIDATION`, `DATABASE`, `NOT_FOUND`, `IO`, `GENERAL`.

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `BEANKEEPER_DB` | Database file path (default: `./beankeeper.db`) |
| `BEANKEEPER_COMPANY` | Default company slug (avoids `--company` on every command) |
| `BEANKEEPER_CURRENCY` | Default currency code for amount filters (default: `USD`) |
| `BEANKEEPER_PASSPHRASE_CMD` | Command to obtain encryption passphrase |
| `NO_COLOR` | Disable colored output |

### Encryption

```sh
# Create an encrypted database
bk init --encrypt

# Provide passphrase via command (for automation)
export BEANKEEPER_PASSPHRASE_CMD="op read op://Vault/beankeeper/passphrase"
bk --company personal report trial-balance
```

## Library

Add the library to your Rust project:

```sh
cargo add beankeeper
```

### Quick Start

```rust
use beankeeper::prelude::*;

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);

let txn = JournalEntry::new(
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
        "Cash sale",
    )
    .debit(&cash, Money::usd(50_00))
    .unwrap()
    .credit(&revenue, Money::usd(50_00))
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.description(), "Cash sale");
assert_eq!(txn.date(), NaiveDate::from_ymd_opt(2026, 3, 15).unwrap());
```

### Core Concepts

**Correctness by construction** -- `Transaction` can only be created through `JournalEntry::post()`, which enforces the balance invariant. Unbalanced transactions cannot exist as values.

**Exact arithmetic** -- all monetary values use `i128` minor-unit representation (cents, pence, centavos, yen). No floating-point. Deterministic across platforms.

**The accounting equation**:
```text
Assets + Expenses = Liabilities + Equity + Revenue
 (Debit normal)       (Credit normal)
```

### Multi-leg Transactions with Memos

```rust
use beankeeper::prelude::*;

let salary = Account::new(AccountCode::new("5000").unwrap(), "Salary", AccountType::Expense);
let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let tax = Account::new(AccountCode::new("2200").unwrap(), "Tax Withheld", AccountType::Liability);

let txn = JournalEntry::new(
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
        "March Paycheck",
    )
    .debit_with_memo(&salary, Money::usd(5000_00), "Gross salary")
    .unwrap()
    .credit_with_memo(&cash, Money::usd(3800_00), "Net pay")
    .unwrap()
    .credit_with_memo(&tax, Money::usd(1200_00), "Federal withholding")
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.entries()[0].memo(), Some("Gross salary"));
```

### Multi-Currency Support

Ten ISO 4217 currencies with correct minor-unit precision:

| Currency | Code | Minor Units |
|----------|------|-------------|
| US Dollar | `USD` | 2 (cents) |
| Euro | `EUR` | 2 (cents) |
| British Pound | `GBP` | 2 (pence) |
| Japanese Yen | `JPY` | 0 |
| Swiss Franc | `CHF` | 2 |
| Canadian Dollar | `CAD` | 2 |
| Australian Dollar | `AUD` | 2 |
| Mexican Peso | `MXN` | 2 (centavos) |
| Bahraini Dinar | `BHD` | 3 |
| Kuwaiti Dinar | `KWD` | 3 |

Currency mismatches within a transaction are rejected at the type level.

### Error Handling

All fallible operations return `Result` with descriptive variants. The top-level `BeanError` aggregates all domain errors for ergonomic `?` usage:

```rust
use beankeeper::prelude::*;

fn record_sale(ledger: &mut Ledger) -> Result<(), BeanError> {
    let cash = Account::new(AccountCode::new("1000")?, "Cash", AccountType::Asset);
    let revenue = Account::new(AccountCode::new("4000")?, "Revenue", AccountType::Revenue);
    let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
    let txn = JournalEntry::new(today, "Sale")
        .debit(&cash, Money::usd(50_00))?
        .credit(&revenue, Money::usd(50_00))?
        .post()?;
    ledger.post(txn);
    Ok(())
}
```

## Safety and Quality

- `#[deny(unsafe_code)]` -- no unsafe Rust in either crate
- `#[deny(clippy::unwrap_used)]` and `#[deny(clippy::expect_used)]` -- proper error handling everywhere
- `#[warn(clippy::pedantic)]` -- pedantic linting enabled
- Library depends only on `chrono`, `sha2`, and `data-encoding` -- minimal footprint
- 395+ tests covering unit, integration, and real-world accounting scenarios

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
