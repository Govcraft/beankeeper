---
name: beankeeper
version: 0.6.0
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

- **Double-entry**: Every transaction has balanced debits and credits. Total debits always equal total credits.
- **Companies**: Multi-tenant -- each company has its own chart of accounts and ledger. Specified via `--company SLUG` or `BEANKEEPER_COMPANY` env var.
- **Accounts**: Five types: `asset`, `liability`, `equity`, `revenue`, `expense`. Each has a code (e.g. `1000`) and a normal balance direction (debit or credit).
- **Amounts**: Always specified in **major units** (dollars, not cents) on the CLI. Stored internally as minor units (cents). Example: `2500` means $2,500.00.
- **Append-only ledger**: Transactions cannot be edited or deleted after posting. Corrections are made via reversing entries.
- **Audit trail**: The few operations that mutate or delete existing rows (clearance changes, intercompany correlation, budget revisions, account/company deletions) each append an immutable record — actor, timestamp, before/after — readable via `bk audit log`. Set the actor with `--actor NAME` or `BK_ACTOR`.
- **Idempotency**: Use `--reference KEY` with `--on-conflict skip` for safe retry of duplicate posts.

For detailed accounting concepts and account types, see [`references/accounting.md`](references/accounting.md).

## Agent Integration

**Always use `--json` for programmatic access.** Every JSON response uses a uniform envelope:

```json
{
  "ok": true,
  "meta": { "command": "...", "company": "...", "timestamp": "..." },
  "data": { ... }
}
```

Errors return `"ok": false` with an `"error"` object containing `"code"` and `"message"`.

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
| `bk txn show ID` | Display a single transaction with entries |
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
| `bk report aging` | Receivables/payables aging buckets (FIFO by transaction date) |
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
```

There is no invoice or due-date model: payments are applied FIFO to the oldest
open charges and ages are measured from transaction dates. To age per customer
or vendor, post with JSON metadata such as `-m '{"customer":"Globex"}'`.

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

`--to` is point-in-time: only clearings dated on or before it count. Over-clearing
is a `VALIDATION` error. Outstanding items carry `reference`, `metadata`,
`cleared_amount` and `remaining_amount`. See the statement-reconciliation recipe
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
