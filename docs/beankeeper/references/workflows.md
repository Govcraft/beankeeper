# Workflow Recipes

Common multi-step workflows for beankeeper.

## Initial Setup

### New business with basic chart of accounts

```bash
bk init
bk company create mybiz "My Business LLC"

# Assets
bk --company mybiz account create 1000 "Operating Cash" --type asset
bk --company mybiz account create 1100 "Accounts Receivable" --type asset

# Liabilities
bk --company mybiz account create 2000 "Accounts Payable" --type liability
bk --company mybiz account create 2600 "Federal Tax Payable" --type liability

# Equity
bk --company mybiz account create 3000 "Owner Equity" --type equity

# Revenue
bk --company mybiz account create 4000 "Service Revenue" --type revenue \
  --default-tax-category income

# Expenses
bk --company mybiz account create 5000 "Rent" --type expense \
  --default-tax-category sched-c:24b
bk --company mybiz account create 5100 "Software" --type expense \
  --default-tax-category sched-c:18
bk --company mybiz account create 5200 "Office Supplies" --type expense \
  --default-tax-category sched-c:22
bk --company mybiz account create 5300 "Salary" --type expense \
  --default-tax-category payroll

# Suspense account for bank imports
bk --company mybiz account create 9000 "Suspense" --type equity
```

### Setup with encrypted database

```bash
bk init --encrypt --path /secure/books.db
# Prompts for passphrase interactively

# Later, open with passphrase
export BEANKEEPER_DB=/secure/books.db
bk --passphrase-file /run/secrets/bk-pass company list
```

## Daily Bookkeeping

### Record a sale

```bash
# Invoice sent (revenue earned, receivable created)
bk --company mybiz txn post -d "Invoice #1042 - March consulting" \
  --debit 1100:15000 --credit 4000:15000 \
  --date 2026-03-15 -r "INV-1042"

# Payment received (cash in, receivable cleared)
bk --company mybiz txn post -d "Payment for Invoice #1042" \
  --debit 1000:15000 --credit 1100:15000 \
  --date 2026-03-25 -r "PAY-INV-1042"
```

### Record an expense

```bash
bk --company mybiz txn post -d "March office rent" \
  --debit 5000:2500 --credit 1000:2500 \
  --date 2026-03-01 -r "RENT-2026-03"
```

### Record payroll with taxes

```bash
bk --company mybiz txn post -d "March payroll" \
  --debit 5300:8000 \
  --credit 2600:960 \
  --credit 1000:7040 \
  --date 2026-03-31 \
  --tax 5300=payroll --tax 2600=payroll-tax \
  -r "PAYROLL-2026-03"
```

## Bank Reconciliation

### Import and reconcile bank statements

```bash
# 1. Import OFX statement (safe to re-run -- duplicates are skipped)
bk --company mybiz txn import \
  --file march-statement.ofx \
  --account 1000 --suspense 9000 \
  --on-conflict skip

# 2. Review imported transactions
bk --company mybiz txn list --account 9000

# 3. Reclassify suspense entries to proper accounts
#    (Post reversing entries from suspense to the correct expense/revenue)
bk --company mybiz txn post -d "Reclassify: Netflix subscription" \
  --debit 5100:15.99 --credit 9000:15.99

# 4. Mark entries as cleared
bk --company mybiz txn clear 50 --entry 99
bk --company mybiz txn clear 50 --entry 100

# 5. After full reconciliation, mark as reconciled
bk --company mybiz txn clear 50 --entry 99 --status reconciled
```

## Budgeting

### Set up annual budgets

```bash
# Annual budgets (evenly distributed across 12 months)
bk --company mybiz budget set 5000 --year 2026 --annual 30000 --notes "Office lease"
bk --company mybiz budget set 5100 --year 2026 --annual 2400 --notes "SaaS tools"
bk --company mybiz budget set 5200 --year 2026 --annual 3600
bk --company mybiz budget set 5300 --year 2026 --annual 96000 --notes "One employee"

# Revenue target
bk --company mybiz budget set 4000 --year 2026 --annual 240000

# Review what was set
bk --company mybiz budget list --year 2026
```

### Override a specific month

```bash
# December has higher software costs (annual renewals)
bk --company mybiz budget set 5100 --year 2026 --month 12 --amount 800
```

### Monthly budget review

```bash
# How did March go?
bk --company mybiz report budget-variance --year 2026 --month 3

# Year-to-date expenses only
bk --company mybiz report budget-variance --year 2026 --from 1 --to 3 --type expense

# Full picture including unbudgeted accounts
bk --company mybiz report budget-variance --year 2026 --include-unbudgeted
```

## Multi-Currency

### Record transactions in foreign currency

```bash
# MXN expense (each transaction is single-currency)
bk --company mybiz txn post -d "Mexico office supplies" \
  --debit 5200:1500 --credit 1000:1500 --currency MXN

# Budgets can also be currency-specific
bk --company mybiz budget set 5200 --year 2026 --annual 18000 --currency MXN

# Variance report for MXN
bk --company mybiz report budget-variance --year 2026 --currency MXN
```

## Intercompany Transactions

### Transfer between entities

```bash
# Company A sends payment to Company B
# In Company A: record the outgoing payment
bk --company company-a txn post -d "Payment to Company B for services" \
  --debit 1500:5000 --credit 1000:5000

# In Company B: record the incoming payment, linked to Company A's transaction
bk --company company-b txn post -d "Received from Company A" \
  --debit 1000:5000 --credit 2500:5000 \
  --correlate 15

# Verify all intercompany links are intact
bk txn reconcile
```

## Tax Reporting

### Generate tax summaries

```bash
# Full year tax summary
bk --company mybiz report tax-summary --from 2026-01-01 --to 2026-12-31

# Quarterly
bk --company mybiz report tax-summary --from 2026-01-01 --to 2026-03-31

# As JSON for processing
bk --company mybiz --json report tax-summary --from 2026-01-01 --to 2026-12-31
```

## Periodic Reports

### Monthly close checklist

```bash
COMPANY=mybiz
MONTH_START=2026-03-01
MONTH_END=2026-03-31

# 1. Transaction count for the period
bk --company $COMPANY txn list --from $MONTH_START --to $MONTH_END --count

# 2. Trial balance (should be balanced)
bk --company $COMPANY report trial-balance --from $MONTH_START --to $MONTH_END

# 3. Income statement for the month
bk --company $COMPANY report income-statement --from $MONTH_START --to $MONTH_END

# 4. Balance sheet as of month end
bk --company $COMPANY report balance-sheet --to $MONTH_END

# 5. Budget variance for the month
bk --company $COMPANY report budget-variance --year 2026 --month 3
```

### End-of-year reporting

```bash
# Full year income statement
bk --company mybiz report income-statement --from 2026-01-01 --to 2026-12-31

# Year-end balance sheet
bk --company mybiz report balance-sheet --to 2026-12-31

# Full year tax summary
bk --company mybiz report tax-summary --from 2026-01-01 --to 2026-12-31

# Annual budget variance
bk --company mybiz report budget-variance --year 2026

# Export everything for accountant
bk export --format json --output 2026-full-export.json
```

## Agent Automation Patterns

### Idempotent batch posting

Use `--reference` and `--on-conflict skip` for safe automation:

```bash
# Safe to run multiple times -- already-posted entries are silently skipped
bk --company mybiz --json txn post -d "Auto: Daily revenue accrual" \
  --debit 1100:500 --credit 4000:500 \
  --date 2026-03-28 \
  -r "AUTO-REV-2026-03-28" --on-conflict skip --quiet
```

### Structured JSON pipeline

```bash
# Get trial balance as JSON, pipe to jq for analysis
bk --company mybiz --json --quiet report trial-balance \
  | jq '.data.accounts[] | select(.type == "expense")'

# Get budget variance, extract unfavorable lines
bk --company mybiz --json --quiet report budget-variance --year 2026 \
  | jq '.data.lines[] | select(.favorable == false)'
```

### Environment-based configuration

```bash
export BEANKEEPER_DB=/data/production.db
export BEANKEEPER_COMPANY=mybiz
export BEANKEEPER_CURRENCY=USD

# All subsequent commands use these defaults
bk txn list --from 2026-03-01
bk report trial-balance
bk budget list --year 2026
```
