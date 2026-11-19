# Command Reference

All commands accept global options: `--db PATH`, `--company SLUG`, `--json`, `--format {table|json|csv}`, `--quiet`, `--verbose`, `--no-color`, `--actor NAME`.

`--actor NAME` sets the principal recorded in the audit trail for any mutating command (see [Audit](#audit)). It falls back to the `BK_ACTOR` environment variable, then the OS user, then `"cli"`.

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

Update clearance status of an entry for bank reconciliation. The transition is
recorded in the audit trail (who/when, old status → new status), so "when was
this account last reconciled?" is answerable via `bk audit log`.

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

**Report currency.** Every report is computed in a single currency; amounts in
different currencies are never added together. All report subcommands accept
`--currency CODE`, which limits the report to transactions in that currency.
When it is omitted the currency is inferred from the company's transactions:

- one currency in use: that currency is used;
- no transactions yet: USD;
- several currencies in use: the command fails with a `VALIDATION` error
  (exit code 3) that lists the currencies found; rerun with `--currency`.

### `bk report trial-balance`

```bash
bk --company acme report trial-balance
bk --company acme report trial-balance --to 2026-03-31
bk --company acme report trial-balance --from 2026-01-01 --to 2026-03-31
bk --company acme report trial-balance --type expense
bk --company acme report trial-balance --currency MXN
```

### `bk report balance --account CODE`

```bash
bk --company acme report balance --account 1000
bk --company acme report balance --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report balance --account 1000 --currency MXN
```

### `bk report income-statement`

Revenue and expense summary for a period. Output has a **Revenue** section and an
**Expenses** section, each listing accounts with one signed amount on the
account's normal side (credit for revenue, debit for expenses) and a subtotal,
followed by **Net Income** (or **Net Loss** when expenses exceed revenue).

```bash
bk --company acme report income-statement --from 2026-01-01 --to 2026-03-31
```

### `bk report balance-sheet`

Assets, liabilities, and equity as of a date. Output has **Assets**,
**Liabilities** and **Equity** sections with subtotals, then **Total
Liabilities + Equity** and a balance check (`[ok] BALANCED` /
`[!!] UNBALANCED`).

Beankeeper does not post closing entries, so the Equity section always
includes a computed line, **Retained Earnings (computed)**, equal to cumulative
revenue minus expenses up to `--to`. This keeps Assets = Liabilities + Equity
for any valid ledger. If you post your own closing entries into a retained
earnings account, the revenue/expense balances drop to zero and the computed
line becomes zero accordingly.

```bash
bk --company acme report balance-sheet
bk --company acme report balance-sheet --to 2026-03-31
```

### `bk report tax-summary`

Entries grouped by tax category.

```bash
bk --company acme report tax-summary --from 2026-01-01 --to 2026-12-31
```

### `bk report ledger`

General ledger / account statement. For each account: an opening balance as of
the day before `--from` (zero without `--from`), one row per entry (ordered by
date, transaction id, entry id) with a running balance, and a closing balance.
Balances are on the account's normal side: `debits - credits` for asset and
expense accounts, `credits - debits` for liability, equity and revenue accounts.

```bash
bk --company acme report ledger
bk --company acme report ledger --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report ledger --account 1000 --account 4000 --currency EUR
bk --company acme --format csv report ledger --account 1000
```

**Flags:**
- `--account CODE` (repeatable): Accounts to include. Default: every account
  with activity (entries in the period or a non-zero opening balance).
- `--from DATE`, `--to DATE`: Inclusive date range (`YYYY-MM-DD`).
- `--currency CODE`: Only entries of this currency. Default: the company's only
  transaction currency; if the company has several, the command fails with a
  validation error (exit 3) asking for `--currency`; with no transactions, USD.

**CSV:** one flat row per statement line. Columns: `account_code`,
`account_name`, `account_type`, `currency`, `row_type`, `date`,
`transaction_id`, `entry_id`, `description`, `reference`, `memo`, `debit`,
`credit`, `balance`. `row_type` is `opening` (balance only, date = `--from`),
`entry`, or `closing` (date = `--to`, `debit`/`credit` = period totals,
`balance` = closing balance). Amounts are minor units.

### `bk report aging`

Receivables / payables aging for a single asset or liability account.

```bash
bk --company acme report aging --account 1100
bk --company acme report aging --account 2000 --as-of 2026-03-31
bk --company acme report aging --account 1100 --buckets 15,45
bk --company acme report aging --account 1100 --group-by-metadata customer
bk --company acme report aging --account 1100 --group-by customer --detail
bk --company acme report aging --account 1100 --method specific
bk --company acme report aging --account 1100 --method specific --link-key invoice
bk --company acme report aging --account 1100 --currency EUR
```

**Flags:**
- `--account CODE` (required): an asset account (receivable: charges are debits, settlements are credits) or a liability account (payable: charges are credits, settlements are debits). Other account types are a validation error (exit 3).
- `--as-of DATE`: include transactions dated on or before this date (default: today). Charges and settlements dated later are ignored, so rerunning for a past period end gives the same answer after later-dated payments are posted (back-dated postings on or before the date do change it).
- `--buckets N,N,...`: strictly ascending positive day bounds (default `30,60,90`, giving `Current`, `1-30`, `31-60`, `61-90`, `90+`)
- `--group-by-metadata KEY` (alias `--group-by KEY`): group by a key of the transaction metadata. Metadata is parsed leniently: if it is a JSON object holding a non-empty string or a number under `KEY`, that value is the counterparty; anything else (missing key, non-JSON text, JSON that is not an object, other value types) goes to `(unassigned)`.
- `--method fifo|specific` (default `fifo`): how settlements are matched to charges (see below).
- `--link-key KEY` (default `applies_to`): metadata key holding a settlement's link to its charge. Only valid with `--method specific` (exit 3 otherwise).
- `--detail`: list the open charges under each counterparty in table and CSV output (JSON always includes `open_items`).
- `--currency CODE`: currency to report. Defaults to the company's single transaction currency; if the company has several, `--currency` is required (exit 3); with no transactions, USD.

**How it works.** The ledger has no invoice or due-date model, so ages are measured from transaction dates. Entries on the account are netted per transaction; a net increase is a *charge* and a net decrease is a *settlement*. For an asset account (receivables) charges are debits and settlements credits; for a liability account (payables) charges are credits and settlements debits -- the same command serves AR and AP. Transactions are processed by date (charges before settlements on the same date, then by id), per counterparty when grouping. Settlements beyond the open charges are reported as an *unapplied credit*, which later charges consume first. Each charge's remaining open amount is bucketed by age in days (`as_of - transaction date`; age 0 is `Current`). The report always reconciles: total open - unapplied credit = account balance as of the date.

**Matching methods.**
- `fifo`: every settlement is applied to the oldest open charges.
- `specific`: a settlement whose metadata links to a charge is applied to that charge first (up to its open amount); any excess, and settlements without a link, fall back to FIFO. Links are ignored on charges.

**Links.** References are unique per company, so a payment cannot share its invoice's reference; instead the settlement's JSON metadata names the charge under the link key (default `applies_to`):
- a string is the charge's `reference` (as posted with `txn post -r`); if no transaction has that reference and the string is an integer, optionally `#`-prefixed (`"42"`, `"#42"`), it is used as a transaction id;
- a positive integer is the charge's transaction id;
- an array of these is applied in order (one payment covering several invoices).

```bash
bk --company acme txn post -d "Invoice 102" -r INV-102 --debit 1100:300 --credit 4000:300 \
  -m '{"customer":"Globex"}'
bk --company acme txn post -d "Globex payment" --debit 1000:100 --credit 1100:100 \
  -m '{"customer":"Globex","applies_to":"INV-102"}'
```

A link that cannot be honoured is ignored, the settlement is applied FIFO, a `[warn]` line is printed on stderr (unless `--quiet`), the table lists it under "Unmatched links", and JSON reports it in `unmatched_links` with one of these reasons: `not_found` (no transaction with that reference/id), `not_on_account` (the transaction has no entries on this account in this currency), `not_a_charge` (it is a settlement or nets to zero on this account), `charge_after_settlement` (the charge is dated after the settlement), `after_as_of` (the charge is dated after `--as-of`), `other_counterparty` (the charge belongs to another counterparty group), `invalid` (the value is not a non-empty string, a positive integer, or an array of those). A link to a charge that is already fully settled is not an error: the amount simply falls back to FIFO.

**Credits, refunds and reversals.** On a receivable, every credit-side net movement is a settlement: payments, credit notes, write-offs, and reversals of an invoice. Under `fifo` it reduces the oldest open charge; under `specific`, link it to the invoice (e.g. a credit note with `{"applies_to":"INV-102"}`) and it cancels that invoice. Every debit-side net movement is a charge: invoices, refunds paid out to the customer, and payment chargebacks (bounced payments). A charge first consumes any unapplied credit, so refunding an overpayment clears the credit; a chargeback that is not absorbed becomes a new open item aged from its own date. Payables mirror this with the sides swapped: bills and vendor refunds received are charges (credits), payments and vendor credit notes are settlements (debits).

**Net credit.** When settlements exceed the open charges the counterparty (or the account) is in *net credit*: the customer has prepaid or overpaid (for payables: we have prepaid or overpaid the vendor). Its open buckets are zero and the excess is shown as `Unapplied Credit`; `Net` (= total open - unapplied credit) is negative. The table marks it `CR` (e.g. `-150.00 CR`) and adds a legend line; CSV has a negative `net`; JSON has a negative `net` and `"net_credit": true`.

**Output.** Table output has one row per counterparty (or a single `(all)` row) plus a total row, with columns for each bucket, `Total Open`, `Unapplied Credit` and `Net`. With `--detail`, an "Open items" table follows for each counterparty: date, transaction id, reference, description, age (days), bucket, original amount and open amount.

CSV (amounts in minor units) without `--detail` has columns `counterparty`, one per bucket, `total_open`, `unapplied_credit`, `net`, one row per counterparty and a final `TOTAL` row. With `--detail` it is a single flat table with a leading `row_type` column: `row_type, counterparty, transaction_id, date, reference, description, age_days, bucket, original_amount, open_amount, <bucket columns>, total_open, unapplied_credit, net`. Rows are `item` (one per open charge; its open amount appears in its bucket column and in `total_open`), then a `counterparty` summary row per group (item columns empty), then a final `total` row with counterparty `TOTAL`.

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
- `--currency CODE`: Currency of the budgets and actuals (env: `BEANKEEPER_CURRENCY`;
  inferred from the company's transactions when omitted, see above)
- `--include-unbudgeted`: Show accounts with actuals but no budget

**Variance logic:**
- Expense accounts: variance = budget - actual (positive = favorable, underspent)
- Revenue accounts: variance = actual - budget (positive = favorable, exceeded target)
- Status: `FAV`, `UNFAV`, or `ON BUDGET`

### Multi-column reports: `--period` and `--compare`

`report income-statement` and `report trial-balance` accept two options that
turn the single-column report into a multi-column one. Both require `--from`
and `--to` (a missing date is a validation error, exit code 3), and they
cannot be combined (usage error, exit code 2). Without either option the
output is unchanged.

```bash
# One column per month, plus a Total column and a Net Income row per month
bk --company acme report income-statement --from 2026-01-01 --to 2026-06-30 --period month

# Quarterly trial-balance activity, expense accounts only
bk --company acme report trial-balance --from 2026-01-01 --to 2026-12-31 --period quarter --type expense

# This quarter vs. the same quarter last year, with change and change %
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-year

# This quarter vs. the previous quarter
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-period
```

**Flags:**
- `--period month|quarter|year`: One column per calendar period between
  `--from` and `--to`, plus a `Total` column. The first and last periods are
  clipped to the range. Columns are labelled `2026-01`, `2026-Q1`, `2026`.
  At most 120 columns.
- `--compare prior-year|prior-period`: Adds a column for an earlier range,
  plus `Change` (current - prior) and `Change %` (relative to the absolute
  prior amount; `n/a` when the prior amount is 0).
  - `prior-year`: the same dates one year earlier (Feb 29 becomes Feb 28).
  - `prior-period`: the range of equal length ending the day before `--from`.
    When the range is whole calendar months it shifts back by that many
    months (Q2 compares with Q1, March with February).

**Amounts:** every cell is the account's net activity in that window on its
normal side (debit - credit for assets and expenses, credit - debit for
liabilities, equity and revenue), so a positive number is a "normal" balance.
Trial-balance columns show activity within each window, not cumulative
balances. The income statement adds `Total Revenue`, `Total Expenses` and
`Net Income` rows; the trial balance adds `Total Debits` and `Total Credits`
rows (raw debit/credit totals per window).

### `bk report reconciliation --account CODE`

Reconcile one account against a bank or card statement using entry clearance
status (set with `bk txn clear`). Not to be confused with `bk txn reconcile`,
which finds orphaned intercompany correlations.

```bash
bk --company acme report reconciliation --account 1000
bk --company acme report reconciliation --account 1000 --to 2026-03-31
bk --company acme report reconciliation --account 1000 --to 2026-03-31 \
  --statement-balance 1234.56
bk --company acme report reconciliation --account 2100 --currency USD   # credit card
```

**Flags:**
- `--account CODE` (required): Account to reconcile
- `--to YYYY-MM-DD`: As-of date, inclusive (default: all dates)
- `--statement-balance AMT`: Statement ending balance in major units (e.g. `1234.56`, may be negative), on the account's normal side
- `--currency CODE`: Only entries of transactions in this currency. If omitted, the company's single transaction currency is used (USD if it has no transactions); if the company has several currencies, the command fails with `VALIDATION` and `--currency` is required

**Output:**
- Book balance (all entries), cleared balance (`cleared` + `reconciled` entries), and reconciled-only balance, all on the account's normal side (debit-normal for asset/expense, credit-normal for liability/equity/revenue)
- The uncleared entries (entry id, transaction id, date, description, signed amount) and their total. Invariant: `book - cleared = uncleared total`
- With `--statement-balance`: the statement balance, `difference = statement - cleared`, and `reconciled: yes/no` (yes when the difference is zero)

**CSV:** one header row `row_type,entry_id,transaction_id,date,description,amount,currency`,
then one `entry` row per uncleared entry, then `summary` rows whose `description`
names the metric (`book_balance`, `cleared_balance`, `reconciled_balance`,
`uncleared_total`, plus `statement_balance`, `difference`, and `reconciled`
(`1`/`0`) when a statement balance is given). Amounts are signed minor units.

### `bk report cash-flow`

Cash flow statement for a period using the direct method.

```bash
bk --company acme report cash-flow --from 2026-01-01 --to 2026-12-31
bk --company acme report cash-flow --from 2026-01-01 --to 2026-03-31 \
  --cash-account 1000 --cash-account 1010
bk --company acme report cash-flow --from 2026-01-01 --to 2026-12-31 --currency EUR
```

**Flags:**
- `--from DATE`, `--to DATE` (required, inclusive, `YYYY-MM-DD`)
- `--cash-account CODE` (repeatable): accounts to treat as cash; each must exist and be an asset
- `--currency CODE`: report currency. Defaults to the company's only transaction currency; if the company has several, `--currency` is required (validation error); with no transactions, USD

**Default cash accounts:** Without `--cash-account`, every **asset** account whose name contains `cash`, `bank`, `checking` or `savings` (case-insensitive) is treated as cash -- this matches the bundled templates (`Operating Cash`, `Checking Account`, `Savings Account`). A code range is deliberately not used: `1100` is `Savings Account` in the personal template but `Accounts Receivable` in the business ones. If no account qualifies, the command exits with a validation error (exit code 3) asking for `--cash-account`.

**Method:**
- Only transactions dated in the range, in the report currency, whose cash entries have a non-zero net movement are included. Transfers between two cash accounts net to zero and are excluded.
- Each non-cash entry of such a transaction is attributed directly to its account: a credit to the counter-account is an inflow, a debit an outflow. Per transaction these sum exactly to the net cash movement, so split transactions need no allocation.
- Sections by counter-account type: revenue/expense = **Operating**; non-cash asset (receivables, inventory, equipment, ...) = **Investing**; liability/equity = **Financing**. The schema has no current/non-current distinction, so working-capital assets land in Investing.
- Opening cash = cash balance before `--from`; closing cash = cash balance through `--to`, computed independently. `opening + net change = closing` is checked and a mismatch is flagged (`[!!] MISMATCH` in the table, `reconciled: false` in JSON).
- CSV columns: `row_type,section,code,name,account_type,amount` with `row_type` `line`, `subtotal` or `summary` (amounts in minor units).

## Audit

The ledger is append-only: posted transactions are never edited or deleted
(corrections go through reversing entries). The few operations that *do* change
existing rows in place each append an immutable record to the audit trail,
capturing the actor (`--actor` / `BK_ACTOR`), a timestamp, and JSON before/after
snapshots. The recorded operations are:

| Action | Recorded when |
|--------|---------------|
| `status_change` | `bk txn clear` flips an entry's clearance status |
| `correlate` | `bk txn post --correlate` links a partner transaction |
| `budget_set` | `bk budget set` creates or revises a budget (prior amount captured) |
| `budget_delete` | `bk budget delete` removes a budget |
| `account_delete` | `bk account delete` removes an account |
| `company_delete` | `bk company delete` removes a company |

### `bk audit log`

List recorded changes, most recent first. Does not require `--company` (scans
all by default).

```bash
bk audit log                                      # 50 most recent changes
bk audit log --company acme                        # one company
bk audit log --entity entry                        # clearance/reconciliation history
bk audit log --action company_delete --limit 100   # last 100 company deletions
bk audit log --json                                # full before/after snapshots
```

**Flags:**
- `--company SLUG`: Restrict to one company
- `--entity KIND`: One of `entry`, `transaction`, `budget`, `account`, `company`
- `--action KIND`: One of `status-change`, `correlate`, `budget-set`, `budget-delete`, `account-delete`, `company-delete`
- `--limit N` (default: 50)
