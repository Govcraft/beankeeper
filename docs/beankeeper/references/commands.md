# Command Reference

All commands accept global options: `--db PATH`, `--company SLUG`, `--json`, `--format {table|json|csv}`, `--quiet`, `--verbose`, `--no-color`.

## Database

### `bk init`

Create a new accounting database.

```bash
bk init                          # Create beankeeper.db in current directory
bk init --encrypt                # Create with passphrase encryption
bk init --path /data/books.db    # Create at specific path
bk init --demo                   # Populate with sample multi-company data
bk init --force                  # Overwrite existing database
```

### `bk verify`

Check ledger integrity: validates the schema version and database structure. Returns exit code 0 if healthy. In JSON mode, returns the schema version and a `"healthy"` status string.

```bash
bk verify
bk --json verify   # Returns { "data": { "schema_version": 7, "status": "healthy" } }
```

### `bk export`

Export all data as JSON or CSV.

```bash
bk export --format json                # JSON to stdout
bk export --format csv --output backup.csv  # CSV to file
```

## Companies

### `bk company create SLUG NAME`

```bash
bk company create acme "Acme Corp"
bk company create acme "Acme Corp" --description "Main business entity"
```

Slug: lowercase alphanumeric and hyphens, 1-64 characters.

### `bk company list`

### `bk company show SLUG`

### `bk company delete SLUG`

```bash
bk company delete old-company          # Prompts for confirmation
bk company delete old-company --force  # Skip confirmation
```

## Accounts

All account commands require `--company`.

### `bk account create CODE NAME --type TYPE`

```bash
bk --company acme account create 1000 "Cash" --type asset
bk --company acme account create 4000 "Revenue" --type revenue --default-tax-category income
```

Types: `asset`, `liability`, `equity`, `revenue`, `expense`.

### `bk account list`

```bash
bk --company acme account list
bk --company acme account list --type expense
bk --company acme account list --name cash
bk --company acme account list --with-balances
bk --company acme account list --with-balances --from 2026-01-01 --to 2026-03-31
```

### `bk account show CODE`

### `bk account delete CODE`

```bash
bk --company acme account delete 9999 --force
```

## Transactions

All transaction commands (except `reconcile`) require `--company`.

### `bk txn post`

Record a balanced journal entry. Debits must equal credits.

```bash
bk --company acme txn post -d "Office rent" \
  --debit 5000:2500 --credit 1000:2500
```

**Flags:**
- `-d, --description TEXT` (required): Transaction description
- `--debit ACCOUNT:AMOUNT` (required, repeatable): Debit entry
- `--credit ACCOUNT:AMOUNT` (required, repeatable): Credit entry
- `--date YYYY-MM-DD`: Transaction date (defaults to today)
- `--currency CODE`: Currency (default: USD)
- `-r, --reference KEY`: Idempotency reference
- `--on-conflict {error|skip}`: Duplicate handling (default: error)
- `--correlate TXN_ID`: Link to intercompany transaction
- `-m, --metadata TEXT`: Arbitrary metadata
- `--tax ACCOUNT=CATEGORY` (repeatable): Tax category for entries

**Amount format:** `account_code:amount` where amount is in major units (dollars).

```bash
# Multi-line with tax categories
bk --company acme txn post -d "Payroll" \
  --debit 5300:5000 \
  --credit 2600:600 --credit 2800:382.50 --credit 1000:4017.50 \
  --tax 5300=payroll --tax 2600=payroll-tax --tax 2800=payroll-tax

# Idempotent post (safe to retry)
bk --company acme txn post -d "Monthly rent" \
  --debit 5000:2500 --credit 1000:2500 \
  -r "RENT-2026-03" --on-conflict skip

# Foreign currency
bk --company acme txn post -d "MXN payment" \
  --debit 5200:8500 --credit 1000:8500 --currency MXN

# Intercompany link
bk --company acme-products txn post -d "Payment to Consulting" \
  --debit 5400:5000 --credit 2500:5000 --correlate 15
```

### `bk txn list`

Search and filter transactions. Alias: `bk txn search`.

```bash
bk --company acme txn list
bk --company acme txn list --from 2026-01-01 --to 2026-03-31
bk --company acme txn list --account 1000
bk --company acme txn list -d "rent"
bk --company acme txn list --amount-gt 10000
bk --company acme txn list --amount-eq 2500
bk --company acme txn list --currency MXN
bk --company acme txn list --tax-category payroll
bk --company acme txn list --count   # Count only, no rows
```

**Flags:**
- `--account CODE`: Filter by account
- `--from DATE`, `--to DATE`: Date range (inclusive)
- `-d, --description TEXT`: Substring search (case-insensitive)
- `--amount-gt`, `--amount-lt`, `--amount-eq`: Amount filters (major units)
- `--currency CODE`: Filter by currency
- `--reference KEY`: Filter by idempotency key
- `--metadata TEXT`: Search metadata field
- `--tax-category CAT`: Filter by tax category
- `--direction {debit|credit}`: Filter entry direction
- `--limit N` (default: 50), `--offset N` (default: 0): Pagination
- `--count`: Return count only

### `bk txn show ID`

Display a single transaction with all its entries.

### `bk txn import`

Import transactions from OFX, CSV, or JSON files.

```bash
bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000

bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000 --dry-run

cat data.ofx | bk --company acme txn import \
  --file - --format ofx --account 1000 --suspense 9000
```

**Flags:**
- `--file PATH`: Input file (use `-` for stdin)
- `--format {ofx|csv|json}`: Auto-detected from extension if omitted
- `--account CODE`: Bank/asset account (required for OFX)
- `--suspense CODE`: Contra account for unclassified entries (required for OFX)
- `--dry-run`: Validate without persisting
- `--on-conflict {error|skip}` (default: skip)

### `bk txn clear TXN_ID --entry ENTRY_ID`

Update clearance status of an entry for bank reconciliation.

```bash
bk --company acme txn clear 42 --entry 5                     # Mark as cleared
bk --company acme txn clear 42 --entry 5 --status reconciled # Mark as reconciled
```

### `bk txn attach TXN_ID FILE --type TYPE`

Attach a document to a transaction.

```bash
bk --company acme txn attach 42 receipt.pdf --type receipt
bk --company acme txn attach 42 invoice.pdf --type invoice --entry 5
```

Types: `receipt`, `invoice`, `statement`, `contract`, `other`.

### `bk txn reconcile`

Find orphaned intercompany correlations. Does not require `--company` (scans all).

## Budgets

All budget commands require `--company`.

### `bk budget set ACCOUNT`

Create or update a budget. Upsert semantics -- re-running overwrites.

```bash
# Single month
bk --company acme budget set 5000 --year 2026 --month 3 --amount 2500

# Annual (distributed evenly across 12 months)
bk --company acme budget set 5000 --year 2026 --annual 30000

# With currency and notes
bk --company acme budget set 5000 --year 2026 --annual 30000 \
  --currency MXN --notes "Office lease"
```

**Flags:**
- `--year YEAR` (required)
- `--month N` + `--amount AMT`: Single month (mutually exclusive with `--annual`)
- `--annual AMT`: Distribute evenly across 12 months (mutually exclusive with `--month`)
- `--currency CODE` (default: USD)
- `--notes TEXT`: Optional note

Amounts are in major units (dollars). Annual distribution is exact: each month gets `annual / 12`, with the first `annual % 12` months receiving an extra minor unit.

### `bk budget list`

```bash
bk --company acme budget list --year 2026
bk --company acme budget list --year 2026 --account 5000
bk --company acme budget list --year 2026 --month 3
```

### `bk budget delete ACCOUNT`

```bash
bk --company acme budget delete 5000 --year 2026           # All months
bk --company acme budget delete 5000 --year 2026 --month 3 # Single month
bk --company acme budget delete 5000 --year 2026 --force   # Skip confirmation
```

## Reports

All report commands require `--company`.

### `bk report trial-balance`

```bash
bk --company acme report trial-balance
bk --company acme report trial-balance --to 2026-03-31
bk --company acme report trial-balance --from 2026-01-01 --to 2026-03-31
bk --company acme report trial-balance --type expense
```

### `bk report balance --account CODE`

```bash
bk --company acme report balance --account 1000
bk --company acme report balance --account 1000 --from 2026-01-01 --to 2026-03-31
```

### `bk report income-statement`

Revenue and expense summary for a period.

```bash
bk --company acme report income-statement --from 2026-01-01 --to 2026-03-31
```

### `bk report balance-sheet`

Assets, liabilities, and equity as of a date.

```bash
bk --company acme report balance-sheet
bk --company acme report balance-sheet --to 2026-03-31
```

### `bk report tax-summary`

Entries grouped by tax category.

```bash
bk --company acme report tax-summary --from 2026-01-01 --to 2026-12-31
```

### `bk report budget-variance`

Compare budgeted amounts to actual spending/revenue.

```bash
bk --company acme report budget-variance --year 2026
bk --company acme report budget-variance --year 2026 --month 3
bk --company acme report budget-variance --year 2026 --from 1 --to 6
bk --company acme report budget-variance --year 2026 --type expense
bk --company acme report budget-variance --year 2026 --include-unbudgeted
```

**Flags:**
- `--year YEAR` (required)
- `--month N`: Single month (mutually exclusive with `--from`/`--to`)
- `--from N`, `--to N`: Month range, 1-12 (defaults to full year)
- `--type TYPE`: Filter by account type
- `--currency CODE` (default: USD)
- `--include-unbudgeted`: Show accounts with actuals but no budget

**Variance logic:**
- Expense accounts: variance = budget - actual (positive = favorable, underspent)
- Revenue accounts: variance = actual - budget (positive = favorable, exceeded target)
- Status: `FAV`, `UNFAV`, or `ON BUDGET`
