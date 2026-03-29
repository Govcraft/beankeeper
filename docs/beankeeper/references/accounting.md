# Accounting Concepts

## The Accounting Equation

```
Assets = Liabilities + Equity + (Revenue - Expenses)
```

Every transaction maintains this equation by posting equal debits and credits.

## Account Types and Normal Balances

| Type | Normal Balance | Increases With | Decreases With | Appears On |
|------|---------------|----------------|----------------|------------|
| Asset | Debit | Debit | Credit | Balance Sheet |
| Liability | Credit | Credit | Debit | Balance Sheet |
| Equity | Credit | Credit | Debit | Balance Sheet |
| Revenue | Credit | Credit | Debit | Income Statement |
| Expense | Debit | Debit | Credit | Income Statement |

**Normal balance** means the direction that increases the account. An asset account with $500 in debits and $200 in credits has a normal (debit) balance of $300.

## Double-Entry Rules

1. Every transaction must have at least two entries
2. Total debits must exactly equal total credits
3. Each entry has a positive amount and an explicit direction (debit or credit)
4. The CLI enforces these rules at post time -- unbalanced transactions are rejected

## Account Codes

Account codes are hierarchical strings using digits, dots, and hyphens:

- `1000` -- Cash (asset)
- `1000.10` -- Checking Account (child of 1000)
- `2000` -- Accounts Payable (liability)
- `4000` -- Revenue
- `5000` -- Rent Expense

A parent code `1000` is a prefix of child code `1000.10`. There is no enforced hierarchy depth.

## Common Chart of Accounts Structure

| Range | Type | Examples |
|-------|------|----------|
| 1000-1999 | Asset | Cash, Accounts Receivable, Equipment |
| 2000-2999 | Liability | Accounts Payable, Loans, Tax Payable |
| 3000-3999 | Equity | Owner Equity, Retained Earnings |
| 4000-4999 | Revenue | Sales, Consulting, Interest Income |
| 5000-5999 | Expense | Rent, Salaries, Supplies, Travel |

## Currencies

Beankeeper supports multi-currency accounting. Each transaction operates in a single currency. Supported currencies include USD, EUR, GBP, JPY, MXN, CAD, AUD, and others following ISO 4217.

Each currency has a defined number of minor units (decimal places):
- **USD, EUR, GBP, MXN**: 2 (cents)
- **JPY**: 0 (whole yen)
- **BHD, KWD**: 3 (fils)

On the CLI, amounts are always specified in major units (e.g. `2500` means $2,500.00). Internally and in JSON output, amounts are stored as integers in minor units (e.g. `250000` for $2,500.00 USD).

## Tax Categories

Entries can be tagged with a tax category string (e.g. `sched-c:24b`) for tax reporting. Accounts can also have a `default_tax_category` that is inherited by entries posted to that account unless overridden with `--tax`.

The `bk report tax-summary` command groups entries by tax category with debit/credit totals.

## Clearance Status

Each entry has a clearance status used during bank reconciliation:

| Status | Meaning |
|--------|---------|
| `uncleared` | Default. Not yet matched to external statement. |
| `cleared` | Matched to a bank statement line. |
| `reconciled` | Finalized in a reconciliation session. |

Status progresses forward only: uncleared -> cleared -> reconciled.

## Idempotency

Transactions support an optional `--reference` key (unique per company). Posting with the same reference twice returns an error by default, or silently skips with `--on-conflict skip`. This enables safe retries when importing or automating transaction posting.

The reference is a human-readable string (e.g. `RENT-2026-03`, `CHASE-20260315-001`). Internally, beankeeper generates a deterministic hash-based idempotency key from it.

## Intercompany Linking

Transactions across companies can be linked via `--correlate TRANSACTION_ID`. This creates bidirectional metadata references between the two transactions. The `bk txn reconcile` command detects broken (orphaned) correlations where one side of the link is missing.
