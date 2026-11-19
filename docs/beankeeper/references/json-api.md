# JSON API Reference

Enable JSON output with `--json` or `--format json`. All responses use a uniform envelope.

## Envelope Structure

### Success

```json
{
  "ok": true,
  "meta": {
    "command": "txn.list",
    "company": "acme",
    "timestamp": "2026-03-28T12:00:00Z"
  },
  "data": { ... }
}
```

### Error

```json
{
  "ok": false,
  "meta": {
    "command": "txn.post",
    "company": "acme",
    "timestamp": "2026-03-28T12:00:00Z"
  },
  "error": {
    "code": "VALIDATION",
    "message": "transaction is not balanced: debits=5000, credits=3000"
  }
}
```

### Error Codes

| Code | Meaning |
|------|---------|
| `USAGE` | Invalid arguments or CLI usage |
| `VALIDATION` | Business rule violation (unbalanced entry, invalid account type) |
| `DATABASE` | SQLite or I/O error |
| `NOT_FOUND` | Entity does not exist |
| `GENERAL` | Other error |
| `IO` | File I/O error |

## Amount Conventions

**All amounts in JSON are raw integers in minor units.** To convert to display format:

```
display_amount = json_amount / (10 ^ currency_minor_units)
```

| Currency | Minor Units | JSON `250000` Displays As |
|----------|-------------|--------------------------|
| USD | 2 | $2,500.00 |
| EUR | 2 | 2,500.00 |
| JPY | 0 | 250,000 |
| MXN | 2 | $2,500.00 |
| BHD | 3 | 250.000 |

## Response Schemas by Command

### `company list`

```json
{
  "data": [
    {
      "slug": "acme",
      "name": "Acme Corp",
      "description": "Main business entity",
      "created_at": "2026-01-01 00:00:00"
    }
  ]
}
```

### `account list`

```json
{
  "data": [
    {
      "code": "1000",
      "name": "Cash",
      "type": "asset",
      "normal_balance": "debit"
    }
  ]
}
```

`normal_balance` is `"debit"` for asset/expense, `"credit"` for liability/equity/revenue.

### `txn post`

```json
{
  "data": {
    "id": 42,
    "existing_id": null,
    "created": true,
    "skipped": false
  }
}
```

When `--on-conflict skip` and reference already exists:

```json
{
  "data": {
    "id": null,
    "existing_id": 42,
    "created": false,
    "skipped": true
  }
}
```

### `txn list`

```json
{
  "data": [
    {
      "id": 1,
      "description": "Office rent",
      "metadata": null,
      "reference": "RENT-2026-03",
      "currency": "USD",
      "date": "2026-03-01",
      "entries": [
        {
          "account_code": "5000",
          "direction": "debit",
          "amount": 250000,
          "memo": null,
          "status": "uncleared"
        },
        {
          "account_code": "1000",
          "direction": "credit",
          "amount": 250000,
          "memo": null,
          "status": "uncleared"
        }
      ]
    }
  ]
}
```

Entry `status` values: `"uncleared"`, `"cleared"`, `"reconciled"`.

### `txn list --count`

```json
{
  "data": { "count": 42 }
}
```

### `report trial-balance`

```json
{
  "data": {
    "accounts": [
      {
        "code": "1000",
        "name": "Cash",
        "type": "asset",
        "debit_total": 500000,
        "credit_total": 250000
      }
    ],
    "total_debits": 500000,
    "total_credits": 500000,
    "balanced": true
  }
}
```

### `report balance`

```json
{
  "data": {
    "code": "1000",
    "name": "Cash",
    "type": "asset",
    "currency": "USD",
    "debit_total": 500000,
    "credit_total": 250000
  }
}
```

### `report income-statement`

Accounts are grouped into sections. Each account carries a single signed
`amount` on its normal side (credit for revenue, debit for expenses), so a
contra balance shows as negative. `net_income` is `total_revenue - total_expenses`;
a negative value is a net loss. `from` / `to` echo the requested period (`null`
when omitted).

```json
{
  "data": {
    "from": "2026-01-01",
    "to": "2026-03-31",
    "sections": [
      {
        "key": "revenue",
        "title": "Revenue",
        "normal_balance": "credit",
        "accounts": [
          { "code": "4000", "name": "Sales", "type": "revenue", "amount": 30000, "computed": false }
        ],
        "total": 30000
      },
      {
        "key": "expenses",
        "title": "Expenses",
        "normal_balance": "debit",
        "accounts": [
          { "code": "5000", "name": "Rent", "type": "expense", "amount": 17000, "computed": false }
        ],
        "total": 17000
      }
    ],
    "total_revenue": 30000,
    "total_expenses": 17000,
    "net_income": 13000
  }
}
```

### `report balance-sheet`

Sections are `assets` (debit-normal), `liabilities` and `equity`
(credit-normal), with the same account line shape as the income statement.
The equity section always ends with a computed line (`"code": null`,
`"computed": true`, name `Retained Earnings (computed)`) equal to cumulative
revenue minus expenses up to `as_of`; the same value is exposed as
`computed_earnings`. `difference` is `total_assets - total_liabilities_and_equity`
and `balanced` is `true` when it is zero. `as_of` is the `--to` date (`null`
when omitted).

```json
{
  "data": {
    "as_of": "2026-03-31",
    "sections": [
      {
        "key": "assets",
        "title": "Assets",
        "normal_balance": "debit",
        "accounts": [
          { "code": "1000", "name": "Cash", "type": "asset", "amount": 118000, "computed": false }
        ],
        "total": 118000
      },
      {
        "key": "liabilities",
        "title": "Liabilities",
        "normal_balance": "credit",
        "accounts": [
          { "code": "2000", "name": "Accounts Payable", "type": "liability", "amount": 5000, "computed": false }
        ],
        "total": 5000
      },
      {
        "key": "equity",
        "title": "Equity",
        "normal_balance": "credit",
        "accounts": [
          { "code": "3000", "name": "Owner Equity", "type": "equity", "amount": 100000, "computed": false },
          { "code": null, "name": "Retained Earnings (computed)", "type": "equity", "amount": 13000, "computed": true }
        ],
        "total": 113000
      }
    ],
    "total_assets": 118000,
    "total_liabilities": 5000,
    "total_equity": 113000,
    "computed_earnings": 13000,
    "total_liabilities_and_equity": 118000,
    "difference": 0,
    "balanced": true
  }
}
```

### Statement CSV output

`--format csv` for `report income-statement` and `report balance-sheet` uses
the columns `section,line_type,code,name,type,amount`, one row per line in
display order:

| `line_type` | Meaning |
|---|---|
| `account` | An account line (`amount` is signed on the normal side) |
| `computed` | The computed retained-earnings line (balance sheet, `equity` section, empty `code`) |
| `subtotal` | Section subtotal (e.g. `Total Revenue`, `Total Assets`) |
| `net_income` | Income statement bottom line (`section` = `summary`; name is `Net Income` or `Net Loss`) |
| `total` | Balance sheet `Total Liabilities + Equity` (`section` = `summary`) |
| `difference` | Balance sheet assets minus liabilities + equity (`section` = `summary`) |
| `balanced` | Balance sheet check, `amount` is `true` or `false` (`section` = `summary`) |

```csv
section,line_type,code,name,type,amount
revenue,account,4000,Sales,revenue,30000
revenue,subtotal,,Total Revenue,,30000
expenses,account,5000,Rent,expense,17000
expenses,subtotal,,Total Expenses,,17000
summary,net_income,,Net Income,,13000
```

### `report budget-variance`

```json
{
  "data": {
    "year": 2026,
    "from_month": 1,
    "to_month": 12,
    "currency": "USD",
    "lines": [
      {
        "code": "5000",
        "name": "Rent Expense",
        "account_type": "expense",
        "budget": 3000000,
        "actual": 2500000,
        "variance": 500000,
        "variance_percent": 16.7,
        "favorable": true
      }
    ],
    "totals": {
      "budget": 3000000,
      "actual": 2500000
    }
  }
}
```

### `budget list`

```json
{
  "data": [
    {
      "id": 1,
      "account_code": "5000",
      "currency": "USD",
      "year": 2026,
      "month": 1,
      "amount": 250000,
      "notes": "Office lease",
      "created_at": "2026-03-28 12:00:00"
    }
  ]
}
```

### `budget delete`

```json
{
  "data": { "deleted": 12 }
}
```

### `txn show`

Same shape as a single element from `txn list`, with entries included.

### `txn clear`

```json
{
  "data": {
    "transaction_id": 42,
    "entry_id": 5,
    "status": "cleared"
  }
}
```

### `verify`

```json
{
  "data": {
    "schema_version": 8,
    "status": "healthy"
  }
}
```

### `report tax-summary`

```json
{
  "data": [
    {
      "tax_category": "sched-c:24b",
      "debit_total": 150000,
      "credit_total": 0
    }
  ]
}
```

### `txn import`

```json
{
  "data": {
    "dry_run": false,
    "imported": 15,
    "skipped": 3,
    "errors": 0,
    "transactions": [
      {
        "id": 50,
        "date": "2026-03-15",
        "description": "ACME COFFEE SHOP",
        "amount": 450,
        "status": "imported",
        "reason": null
      },
      {
        "id": null,
        "date": "2026-03-10",
        "description": "MONTHLY FEE",
        "amount": 1500,
        "status": "skipped",
        "reason": "duplicate"
      }
    ]
  }
}
```

### `audit log`

`data` is an array of recorded changes, most recent first. `before`/`after`
are JSON strings (snapshots of the relevant fields) or `null`; `company_slug`
may be `null` for cross-company changes.

```json
{
  "data": [
    {
      "id": 12,
      "changed_at": "2026-03-31 18:04:22",
      "actor": "roland",
      "company_slug": "acme",
      "entity": "entry",
      "entity_id": "5",
      "action": "status_change",
      "before": "{\"status\":\"uncleared\"}",
      "after": "{\"status\":\"reconciled\"}"
    },
    {
      "id": 11,
      "changed_at": "2026-03-31 18:03:10",
      "actor": "roland",
      "company_slug": "acme",
      "entity": "budget",
      "entity_id": "7",
      "action": "budget_set",
      "before": "{\"amount\":250000}",
      "after": "{\"amount\":300000,\"month\":3,\"notes\":null,\"year\":2026}"
    }
  ]
}
```

`entity` is one of `entry`, `transaction`, `budget`, `account`, `company`.
`action` is one of `status_change`, `correlate`, `budget_set`, `budget_delete`,
`account_delete`, `company_delete`.

## Parsing Tips for Agents

1. **Check `ok` first**: If `false`, read `error.code` and `error.message` to decide next steps
2. **Use `meta.command`** to confirm the response matches the expected command
3. **Convert amounts**: Divide by `10^minor_units` for the currency (2 for USD/EUR/MXN, 0 for JPY, 3 for BHD)
4. **Handle null fields**: `metadata`, `reference`, `memo`, `notes`, `description` can be null
5. **Status messages go to stderr**: Use `--quiet` to suppress them; only JSON goes to stdout
