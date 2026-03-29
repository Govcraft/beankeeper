# Beankeeper

The accounting runtime for AI agents. Your assistant manages your books -- `bk` makes sure it can't mess them up.

A CLI backed by SQLCipher-encrypted SQLite, with an embeddable Rust library underneath.

## Why Beankeeper?

AI assistants can already categorise expenses, reconcile bank feeds, and generate financial reports. The missing piece is a backend that makes those operations *safe* -- one where the agent physically cannot produce an unbalanced ledger, silently duplicate a transaction, or corrupt the audit trail.

hledger, ledger-cli, and beancount are built around plain-text files a human edits by hand. That's the right design for manual bookkeeping. `bk` is built for when an agent is doing it.

### The backing store is a database, not a file

SQLite (optionally encrypted with SQLCipher) is the source of truth, not a `.journal` file. That means the agent can post transactions, query balances, and generate reports through a stable CLI contract without worrying about file locking, merge conflicts, or parse errors.

### The agent cannot create an unbalanced transaction

The double-entry invariant is enforced at the type level. There's no error to catch at runtime because there's no way to construct a transaction with unequal debits and credits -- the accounting equation is a compile-time guarantee, not a runtime check. A confused or hallucinating agent simply cannot post bad data.

### Retries are safe

Agents retry. Import scripts fail mid-batch. `--reference` takes any string (a bank reference number, a statement row hash, whatever) and produces a deterministic idempotency key via SHA-256. Same reference, same company, always maps to the same transaction -- subsequent posts with the same key are rejected with the existing transaction ID. The agent can retry as many times as it wants without creating duplicates.

### The agent gets structured feedback, not text to parse

Every command in JSON mode returns the same envelope:

```json
{ "ok": true,  "meta": { "command": "txn.post", "company": "acme", "timestamp": "..." }, "data": { ... } }
{ "ok": false, "meta": { "command": "txn.post", "company": "acme", "timestamp": "..." }, "error": { "code": "NOT_FOUND", "message": "..." } }
```

Named error codes (`NOT_FOUND`, `VALIDATION`, `DATABASE`, etc.), `meta.command` in dot notation, consistent field presence. The agent can handle `bk` output reliably without parsing human-readable text or guessing at shapes.

### The audit trail is tamper-proof

Posted transactions cannot be edited or deleted. The entries, amounts, accounts, and dates are immutable. The only thing that can change after posting is a transaction's metadata field. The audit trail is intact by design, not by convention -- if the agent makes a mistake, you can see exactly what happened and when.

### Everything else

- **Multi-company tenancy** -- multiple entities in one database, isolated by slug; the agent manages your LLC and personal books in the same place
- **Document attachments** -- receipts and invoices linked to transactions via SHA-256 content addressing
- **Tax category tagging** -- per-entry or account-level defaults, free-form strings mapping to any tax form lines you need
- **Encryption at rest** -- SQLCipher baked in from the start, not bolted on
- **Intercompany linking** -- mirror transactions across entities stay paired through bidirectional correlation IDs; `bk txn reconcile` catches orphans

If you want to edit your ledger in `$EDITOR`, the plain-text tools are excellent. If you want your AI assistant to do the bookkeeping while you keep the guardrails, `bk` is the right foundation.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper/main/install.sh | sh
```

Detects your OS and architecture, downloads the right binary, verifies the SHA-256 checksum, and installs to `/usr/local/bin`. Override the install directory with `BK_INSTALL=~/.local/bin` or pin a version with `BK_VERSION=0.2.0`.

**Arch Linux (AUR)**:
```sh
yay -S bk-bin
```

**From source**:
```sh
cargo install --git https://github.com/Govcraft/beankeeper beankeeper-cli
```

## Quick Start

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
bk --company personal report trial-balance --from 2026-01-01 --to 2026-03-31
bk --company personal report balance --account 1000 --to 2026-03-31
bk --company personal report balance-sheet --to 2026-03-31
bk --company personal report income-statement --from 2026-01-01 --to 2026-12-31
```

## Demo Mode

Spin up a fully populated database to explore every feature without manual setup:

```sh
bk init --demo
```

This creates three companies with charts of accounts, regular transactions, and intercompany-linked mirror pairs:

| Company | Slug | Description |
|---------|------|-------------|
| Acme Consulting LLC | `acme-consulting` | Service revenue, rent, office expenses, payroll |
| Acme Products Inc | `acme-products` | Inventory, product sales, COGS, shipping |
| Personal | `personal` | Checking/savings, salary income, personal expenses |

Intercompany transactions demonstrate bidirectional correlation:

- Owner loans personal savings to acme-consulting (`--correlate`)
- acme-consulting purchases software licences from acme-products
- acme-consulting settles the intercompany payable to acme-products
- **Payroll with withholdings** -- a split transaction showing a $5,000 gross salary broken into federal tax, state tax, FICA, and net pay on both the employer and employee books

Try it out:

```sh
# Trial balance for the consulting company
bk --company acme-consulting report trial-balance

# View the payroll split transaction
bk --company acme-consulting txn list --description "payroll" --json

# Balance sheet as of quarter end
bk --company personal report balance-sheet --to 2024-03-31

# Trial balance for a date range
bk --company personal report trial-balance --from 2024-01-01 --to 2024-03-31

# Balance as of a specific date
bk --company personal report balance --account 1000 --to 2024-03-16
```

## Demo Mode


Beyond the core guarantees above, `bk` provides the following capabilities for multi-entity accounting, automation, and reporting.

### Intercompany Transactions

Mirror entries across companies stay linked through bidirectional correlation IDs:

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

When `bk` is driven by AI agents or import scripts, retries can cause duplicate transactions. The `--reference` flag provides a deterministic idempotency key -- same reference, same company, always maps to the same transaction.

The `--on-conflict` flag controls how duplicates are handled:

```sh
# First post succeeds
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95

# Default is --on-conflict error: retry is rejected
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95
# -> error: transaction with reference 'chase-2026-03-15-001' already exists (id: 1)

# Use --on-conflict skip to succeed silently
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95 --on-conflict skip
# -> [skipped] duplicate reference; transaction already exists (id: 1)
```

In JSON mode, `txn post` returns a `data` object indicating the result:

```json
{
  "ok": true,
  "meta": { ... },
  "data": {
    "id": 1,
    "created": true,
    "skipped": false
  }
}
```

If skipped, `id` is null and `existing_id` is provided:

```json
{
  "ok": true,
  "meta": { ... },
  "data": {
    "existing_id": 1,
    "created": false,
    "skipped": true
  }
}
```

References are hashed into deterministic `txnref_`-prefixed keys. Transactions without `--reference` are unrestricted.

### OFX / QFX Bank Statement Import

Import transactions from bank and credit card statements exported in OFX format. Each OFX transaction becomes a balanced double-entry against a user-specified bank account and suspense/clearing account:

```sh
# Import an OFX statement -- format is auto-detected from the file extension
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000

# Preview what would be imported without posting
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000 --dry-run

# Import from stdin with explicit format
cat statement.qfx | bk --company personal txn import --file - --format ofx --account 1000 --suspense 9000
```

**Deduplication is automatic.** Each OFX transaction's unique `FITID` is stored as an idempotency reference, so re-importing the same file skips already-posted transactions. Use `--on-conflict error` to fail the whole import if any transaction already exists:

```sh
# Default is --on-conflict skip
Imported 47 transactions, skipped 3 duplicates.
```

Supports banking (`BANKMSGSRSV1`), credit card (`CREDITCARDMSGSRSV1`), and investment account (`INVSTMTMSGSRSV1`) OFX message sets. Investment statements extract the banking transactions (`INVBANKTRAN/STMTTRN`) from brokerage cash activity.

| OFX Field | Beankeeper Field | Notes |
|-----------|-----------------|-------|
| `DTPOSTED` | Transaction date | Formatted as YYYY-MM-DD |
| `NAME` + `MEMO` | Description | Joined with ` - ` if both present |
| `TRNAMT` (positive) | Debit bank, credit suspense | Inflows |
| `TRNAMT` (negative) | Debit suspense, credit bank | Outflows |
| `FITID` | Reference (idempotency key) | Namespaced as `ofx:<currency>:<account_id>:<fit_id>` |
| `TRNTYPE` | Metadata | Stored as `{"ofx_type": "CHECK"}` |
| `CURDEF` | Currency | Must be a supported currency |

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
# -> {"ok": true, "meta": {...}, "data": {"count": 12}}

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

# Search accounts by name, with balances for a period
bk --company personal account list --name "Cash" --with-balances --to 2026-06-30 --json
```

### Output Formats

Every command supports `--format table` (default), `--format json`, and `--format csv`. Use `--json` as shorthand:

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

The CLI is built on the [`beankeeper`](https://crates.io/crates/beankeeper) library crate, which you can embed directly in Rust projects that need accounting primitives without the CLI layer.

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
- 415+ tests covering unit, integration, and real-world accounting scenarios

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
