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

Same schema as `report trial-balance`. Contains only revenue and expense accounts for the specified period.

### `report balance-sheet`

Same schema as `report trial-balance`. Contains only asset, liability, and equity accounts as of the specified date.

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
    "schema_version": 7,
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

## Parsing Tips for Agents

1. **Check `ok` first**: If `false`, read `error.code` and `error.message` to decide next steps
2. **Use `meta.command`** to confirm the response matches the expected command
3. **Convert amounts**: Divide by `10^minor_units` for the currency (2 for USD/EUR/MXN, 0 for JPY, 3 for BHD)
4. **Handle null fields**: `metadata`, `reference`, `memo`, `notes`, `description` can be null
5. **Status messages go to stderr**: Use `--quiet` to suppress them; only JSON goes to stdout
