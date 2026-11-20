---
name: beankeeper
version: 0.10.1
description: >
  This skill should be used when interacting with the beankeeper accounting system
  via the `bk` CLI. Use when the user asks to "record a transaction", "post an entry",
  "check a balance", "generate a report", "create a company", "set up accounts",
  "import bank statements", "set a budget", "compare budget vs actual",
  "reconcile entries", "verify the ledger", "export data", "attach a receipt",
  or any financial bookkeeping task. Also use when piping structured JSON output
  from `bk` into other tools or agents.
---

# Beankeeper (`bk`) -- Double-Entry Accounting CLI

Beankeeper is a double-entry accounting system operated entirely through the `bk` command-line interface. It stores data in a local SQLite database (optionally encrypted via SQLCipher). All output supports three formats: human-readable tables, machine-readable JSON, and CSV.

## Core Concepts

- **Double-entry**: Postings are validated at runtime before saving. Unbalanced or mixed-currency entries are rejected; the library's `Transaction` type can only be constructed through `JournalEntry::post()` after validation.
- **Companies**: Multi-tenant -- each company has its own chart of accounts and ledger. Specified via `--company SLUG` or `BEANKEEPER_COMPANY` env var.
- **Accounts**: Five types: `asset`, `liability`, `equity`, `revenue`, `expense`. Each has a code (e.g. `1000`) and a normal balance direction (debit or credit).
- **Amounts**: Always specified in **major units** (dollars, not cents) on the CLI. Stored internally as minor units (cents). Example: `2500` means $2,500.00.
- **Append-only ledger**: Transactions cannot be edited or deleted after posting. Corrections are made via reversing entries.
- **Audit trail**: The few operations that mutate or delete existing rows (clearance changes, intercompany correlation, budget revisions, account/company deletions) each append an immutable record — actor, timestamp, before/after — readable via `bk audit log`. Set the actor with `--actor NAME` or `BK_ACTOR`.
- **Idempotency**: `--reference KEY` is trimmed, stored verbatim, and unique per company. Empty references are rejected. Use `--on-conflict skip` for safe retry of duplicate posts; `upsert` is not implemented.

For detailed accounting concepts and account types, see [`references/accounting.md`](references/accounting.md).

## Agent Integration

**Use `--json` for programmatic command responses.** Responses use a uniform envelope, except for successful exports:

```json
{
  "ok": true,
  "meta": { "command": "...", "company": "...", "timestamp": "..." },
  "data": { ... }
}
```

Errors return `"ok": false` with an `"error"` object containing `"code"` and `"message"`.

`bk export --output-format json` writes a standalone `{"beankeeper_export": {"version": 1, "companies": [...]}}` object, even with `--json`. It has no `ok`, `meta`, or `data` wrapper. Read exported companies with `jq '.beankeeper_export.companies'`.

Key conventions for agents:
- Amounts in JSON are **raw integers in minor units** (e.g. `250000` = $2,500.00 for USD with 2 decimal places)
- Set `BEANKEEPER_DB` and `BEANKEEPER_COMPANY` env vars to avoid repeating `--db` and `--company` on every call
- Use `--quiet` to suppress stderr status messages
- Use `--reference` + `--on-conflict skip` for idempotent transaction posting

For the complete JSON envelope specification, see [`references/json-api.md`](references/json-api.md).

## Command Overview

| Command | Purpose |
|---------|---------|
| `bk init` | Create a new database (`--demo` for sample data, `--encrypt` for passphrase) |
| `bk company create/list/show/delete` | Manage companies |
| `bk account create/list/show/delete` | Manage chart of accounts |
| `bk txn post` | Record a balanced journal entry |
| `bk txn list` | Search and filter transactions |
| `bk txn show ID` | Display a single transaction with entries, their cleared/remaining amounts and individual clearings |
| `bk txn import` | Import OFX/CSV/JSON bank statements |
| `bk txn clear` | Clear entries (in full or in part) against a statement, by id, by reference, or in an atomic batch; mark reconciled |
| `bk txn attach` | Link documents (receipts, invoices) to transactions |
| `bk txn reconcile` | Find orphaned intercompany correlations |
| `bk budget set` | Create or update monthly/annual budgets |
| `bk budget list` | List budget entries |
| `bk budget delete` | Remove budget entries |
| `bk report trial-balance` | All accounts with debit/credit totals |
| `bk report balance` | Single account balance |
| `bk report income-statement` | Revenue and Expenses sections with subtotals and net income for a period |
| `bk report balance-sheet` | Assets, Liabilities, Equity sections (incl. computed retained earnings) and balance check as of a date |
| `bk report tax-summary` | Entries grouped by tax category |
| `bk report budget-variance` | Budget vs actual comparison |
| `bk report ledger` | General ledger / account statement with running balances |
| `bk report aging` | Receivables/payables aging buckets (FIFO or specific invoice matching, by transaction date) |
| `bk report reconciliation` | Book vs cleared vs reconciled balance for one account (point in time, per statement), outstanding and partially cleared items, statement difference, metadata filter/grouping |
| `bk report cash-flow` | Cash flow statement (direct method) for a period |
| `bk audit log` | Inspect the audit trail of ledger mutations/deletions |
| `bk verify` | Check ledger integrity |
| `bk export` | Export all data as JSON or CSV |

For the full command reference with all flags and examples, see [`references/commands.md`](references/commands.md).

## Essential Workflows

### Set up a new ledger

```bash
bk init
bk company create acme "Acme Corp"
bk --company acme account create 1000 "Cash" --type asset
bk --company acme account create 2000 "Accounts Payable" --type liability
bk --company acme account create 3000 "Owner Equity" --type equity
bk --company acme account create 4000 "Revenue" --type revenue
bk --company acme account create 5000 "Rent Expense" --type expense
```

### Post a transaction

Debit and credit entries use the format `account_code:amount`:

```bash
bk --company acme txn post -d "Office rent" \
  --debit 5000:2500 --credit 1000:2500
```

Multi-line entries (e.g. payroll):

```bash
bk --company acme txn post -d "March payroll" \
  --debit 5300:5000 \
  --credit 2600:600 --credit 2800:382.50 --credit 1000:4017.50 \
  --tax 5300=payroll --tax 2600=payroll-tax
```

### Import bank statements

```bash
bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000 \
  --on-conflict skip
```

### Generate reports

```bash
bk --company acme report trial-balance
bk --company acme report income-statement --from 2026-01-01 --to 2026-03-31
bk --company acme report balance-sheet --to 2026-03-31
bk --company acme report tax-summary --from 2026-01-01 --to 2026-12-31
bk --company acme report ledger --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report cash-flow --from 2026-01-01 --to 2026-03-31

# Companies with transactions in several currencies must pick one
bk --company acme report trial-balance --currency MXN

# Multi-column views (income-statement and trial-balance; need --from and --to)
bk --company acme report income-statement --from 2026-01-01 --to 2026-06-30 --period month
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-year
```

`--period month|quarter|year` gives one column per period plus a Total (and a
Net Income row per period on the income statement). `--compare
prior-period|prior-year` adds the earlier range with change and change %
(`null`/`n/a` when the prior amount is 0). The two options conflict. Amounts
are net on each account's normal side. JSON command ids are
`report.<report>.periods` and `report.<report>.compare`; see
[`references/commands.md`](references/commands.md) and
[`references/json-api.md`](references/json-api.md).

Reports never mix currencies. Without `--currency`, the report currency is
inferred from the company's transactions (USD if there are none); if the
company uses more than one currency the command fails with a `VALIDATION`
error listing them, and `--currency CODE` is required.

### Receivables / payables aging

```bash
bk --company acme report aging --account 1100                     # AR as of today
bk --company acme report aging --account 2000 --as-of 2026-03-31  # AP
bk --company acme report aging --account 1100 --buckets 15,45 --group-by-metadata customer
bk --company acme report aging --account 1100 --group-by customer --detail   # list open invoices
bk --company acme report aging --account 1100 --method specific              # honour applies_to links
bk --company acme report aging --account 1110 --account 1120                 # several sub-ledgers
bk --company acme report aging --account 1100 --include-children            # 1100 plus 11xx children
bk --company acme report aging --account 1100 --filter-metadata status=disputed
```

There is no invoice or due-date model: each net increase to the account is a
charge, each net decrease a settlement, and ages are measured from transaction
dates. Asset accounts (AR) age debit-side charges and apply credit-side
settlements; liability accounts (AP) the reverse.

With several accounts, each is matched on its own (totals equal the sum of
single-account runs; a link to a charge on another account is reported as
`other_account`), and all must be AR or all AP. `--include-children` adds every
account whose code starts with the given code minus trailing zeros. With
`--group-by`, counterparties are grouped per account unless
`--group-across-accounts` is set; JSON has per-account `accounts[]` and the
combined `groups[]`. `--filter-metadata` keeps only matching open charges,
after all settlements were applied, so tagged items (e.g. `status=disputed`)
keep their original age; unapplied credit is not shown under a filter.

- To age per customer or vendor, post with JSON metadata such as
  `-m '{"customer":"Globex"}'`. Missing keys and non-JSON metadata go to
  `(unassigned)`.
- Settlements are applied FIFO by default. To pay a specific invoice, post the
  invoice with a reference (`-r INV-102`) and the payment, credit note or
  reversal with `-m '{"applies_to":"INV-102"}'` (a transaction id also works),
  then run with `--method specific`. Excess and unlinked amounts fall back to
  FIFO; broken links are warned about and listed in `data.unmatched_links`.
- Overpayments become unapplied credit: `net` is negative and `net_credit` is
  `true`. Refunds and chargebacks are charges that consume it.
- `--as-of` ignores later-dated transactions, so past period ends are stable.

### Reconcile an account against a statement

```bash
# Mark the entries that appear on the statement as cleared
bk --company acme txn clear 42 --entry 5

# Compare the cleared balance to the statement ending balance
bk --company acme report reconciliation --account 1000 --to 2026-03-31 \
  --statement-balance 1234.56
```

Balances are on the account's normal side (debit-normal for asset/expense,
credit-normal otherwise). `book - total cleared` always equals the total of the
listed uncleared entries' remaining amounts; `difference = statement - cleared`,
and the account is reconciled when the difference is zero.

Entries can be cleared **in parts**, each clearing with an amount, a date and a
statement id. Clear by transaction reference instead of ids, or a whole
statement from a file/stdin (atomic; re-running is a no-op because each
entry is cleared at most once per statement):

```bash
# Partial payment of one item
bk --company acme txn clear --reference POL-123 --account 1200 \
  --amount 40 --date 2026-03-31 --statement STMT-03

# A whole statement: one `reference[,amount]` per line (no amount = full remaining)
printf 'POL-123,40\nPOL-124\n' | bk --json --company acme txn clear \
  --from-file - --account 1200 --date 2026-03-31 --statement STMT-03

# Check that statement alone, and list what is still open by carrier
bk --company acme report reconciliation --account 1200 --statement STMT-03 --statement-balance 65
bk --company acme report reconciliation --account 1200 --group-by-metadata carrier \
  --filter-metadata region=west
```

`--to` is point-in-time: clearings count as cleared when cleared on or before it,
and as reconciled only when also reconciled on or before it (`--status reconciled
--date D` records reconciled date `D`). Over-clearing, a `--date` before the
transaction date, and a reconciled date before the cleared date are `VALIDATION`
errors. Reconciled clearings are locked: `--status cleared` on them fails; undo
with `--status uncleared`, then clear again. Outstanding items carry `reference`,
`metadata`, `cleared_amount`, `reconciled_amount`, `remaining_amount` and
`clearings`; `bk txn show` lists every entry's clearings. See the statement-reconciliation recipe
in [`references/workflows.md`](references/workflows.md).

`report cash-flow` treats asset accounts named like `cash`/`bank`/`checking`/`savings` as cash unless you pass `--cash-account CODE` (repeatable); see [`references/commands.md`](references/commands.md#bk-report-cash-flow).

### Budgeting

```bash
# Set an annual budget (evenly distributed across 12 months)
bk --company acme budget set 5000 --year 2026 --annual 30000

# Set a specific month's budget
bk --company acme budget set 4000 --year 2026 --month 3 --amount 10000

# Compare budget vs actual
bk --company acme report budget-variance --year 2026
bk --company acme report budget-variance --year 2026 --from 1 --to 6 --type expense
```

### Intercompany transactions

```bash
# Post in company A
bk --company acme-consulting txn post -d "Invoice to Products" \
  --debit 1500:5000 --credit 4000:5000

# Post correlated entry in company B (links bidirectionally)
bk --company acme-products txn post -d "Payment to Consulting" \
  --debit 5400:5000 --credit 2500:5000 --correlate 15

# Check for broken links
bk txn reconcile
```

### Metadata

Transaction `metadata` is always `null` or a JSON object. `-m "free text"` is
stored as `{"memo":"free text"}`; `-m '{"k":"v"}'` is stored unchanged.
`--meta key=value` (repeatable) adds top-level string keys; duplicates are a
validation error. Filter with `--filter-metadata key=value` (exact, AND-ed), the
same rule on `txn list` (where `--meta` is an alias), `report ledger`,
`report reconciliation` and `report aging`; or `txn list --metadata TEXT`
(substring). Strings match exactly, numbers and booleans by their JSON text
(`qty=3` matches `{"qty":3}`); null, arrays, objects and non-JSON never match.

```bash
bk --company acme txn post -d "AWS bill" --debit 5000:120 --credit 1000:120 \
  -m "Q1 cloud" --meta vendor=AWS --meta project=apollo
bk --company acme txn list --filter-metadata vendor=AWS --json
bk --company acme report ledger --account 5000 --filter-metadata project=apollo
```

For additional workflow patterns including bank reconciliation and tax reporting, see [`references/workflows.md`](references/workflows.md).

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `BEANKEEPER_DB` | Database file path | `beankeeper.db` |
| `BEANKEEPER_COMPANY` | Default company slug | (none) |
| `BEANKEEPER_CURRENCY` | Default currency code (for `report budget-variance`, inferred from transactions when unset) | `USD` |
| `BEANKEEPER_PASSPHRASE_CMD` | Command to obtain encryption passphrase | (none) |
| `BK_ACTOR` | Principal recorded in the audit trail for mutations | OS user, else `cli` |
| `NO_COLOR` | Disable colored output | (unset) |

## Error Handling

All errors return a non-zero exit code and, in JSON mode, a structured error:

| Exit Code | Meaning |
|-----------|---------|
| 0 | Success |
| 2 | Usage / argument error |
| 3 | Validation error (unbalanced transaction, invalid account) |
| 4 | Database error |
| 5 | Not found |

Validate before posting: ensure account codes exist (`bk account list`), amounts are positive, and total debits equal total credits. The CLI enforces balance at post time and will reject unbalanced entries.

## Reference Files

For detailed information beyond this overview:

- **[`references/accounting.md`](references/accounting.md)** -- Account types, normal balances, the accounting equation, and double-entry rules
- **[`references/commands.md`](references/commands.md)** -- Complete command reference with all flags, arguments, and examples
- **[`references/json-api.md`](references/json-api.md)** -- JSON envelope format, field types, and programmatic usage patterns
- **[`references/workflows.md`](references/workflows.md)** -- Multi-step workflow recipes for common bookkeeping tasks
