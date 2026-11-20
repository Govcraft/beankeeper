# JSON API Reference

Enable JSON command responses with `--json` or `--format json`. Responses use a uniform envelope, except for successful JSON exports, which write the standalone file format described below.

## JSON Export

`bk export --output-format json` writes this object to stdout, or to the path given by `--output`, even when `--json` is also set:

```json
{
  "beankeeper_export": {
    "version": 1,
    "companies": []
  }
}
```

The `companies` array contains the exported companies with their accounts and transactions. Successful export output has no `ok`, `meta`, or `data` wrapper. For example, `bk export --output-format json | jq '.beankeeper_export.companies'` reads the exported companies. Export errors in JSON response mode use the error envelope.

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

### `txn reverse` (alias `txn void`)

Uses the same `data` shape as `txn post`, with `meta.command` equal to
`txn.reverse`. A new mirror returns `id`, `created: true`, `skipped: false`.
Retrying with an explicit reference matching the linked reversal returns
`existing_id`, `created: false`, `skipped: true`, without another audit row.
Other repeat attempts fail with `VALIDATION` (exit 3). Missing or foreign
transaction IDs return `NOT_FOUND` (exit 5).

`txn show` and `txn list` retain their existing shape. Parse the transaction's
`metadata` string to read numeric links: `reversed_by` on the original and
`reverses` on the reversal. The original's other metadata is preserved;
the new transaction contains only the reversal link. Its entries start
uncleared, with their original accounts, amounts, memos and tax categories.
The atomic audit action is `reverse`, associated with the original ID;
`before` records original metadata and `after` records updated metadata,
`reversed_by` and the reversal's `reference`.

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
          "id": 1,
          "account_code": "5000",
          "direction": "debit",
          "amount": 250000,
          "memo": null,
          "status": "uncleared"
        },
        {
          "id": 2,
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

Entry `id` is the entry id (`txn clear TXN_ID --entry ID`). Entry `status`
values: `"uncleared"`, `"cleared"`, `"reconciled"`.

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

### `report ledger`

All amounts are minor units; `balance`, `opening_balance` and `closing_balance`
are on the account's `normal_balance` side (negative = abnormal side).
`metadata_filters` lists the `--filter-metadata` pairs as `{ "key", "value" }`
(`[]` without them); with filters, every balance counts matching entries only.

```json
{
  "ok": true,
  "meta": { "command": "report.ledger", "company": "acme", "timestamp": "..." },
  "data": {
    "currency": "USD",
    "from": "2026-01-01",
    "to": "2026-03-31",
    "metadata_filters": [],
    "accounts": [
      {
        "code": "1000",
        "name": "Cash",
        "type": "asset",
        "normal_balance": "debit",
        "opening_balance": 100000,
        "entries": [
          {
            "date": "2026-01-05",
            "transaction_id": 7,
            "entry_id": 13,
            "description": "Office rent",
            "reference": "RENT-2026-01",
            "memo": null,
            "debit": 0,
            "credit": 250000,
            "balance": -150000
          }
        ],
        "total_debits": 0,
        "total_credits": 250000,
        "closing_balance": -150000
      }
    ]
  }
}
```

`from`/`to` are `null` when not given. Omitting `--currency` when the company
has transactions in several currencies returns a `VALIDATION` error
envelope (exit code 3).

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

### `report income-statement --period` / `report trial-balance --period`

`meta.command` is `report.income-statement.periods` or
`report.trial-balance.periods`. Each account's `amounts` array has one entry
per element of `columns` (net amount on the account's normal side, minor
units); `total` is their sum. `totals` holds `total_revenue`,
`total_expenses` and `net_income` (income statement) or `total_debits` and
`total_credits` (trial balance), each with the same `amounts`/`total` shape.

```json
{
  "data": {
    "report": "income-statement",
    "mode": "periods",
    "period": "month",
    "from": "2026-01-01",
    "to": "2026-02-28",
    "currency": "USD",
    "columns": [
      { "label": "2026-01", "from": "2026-01-01", "to": "2026-01-31" },
      { "label": "2026-02", "from": "2026-02-01", "to": "2026-02-28" }
    ],
    "accounts": [
      {
        "code": "4000",
        "name": "Sales",
        "type": "revenue",
        "normal_balance": "credit",
        "amounts": [200000, 50000],
        "total": 250000
      }
    ],
    "totals": {
      "net_income": { "amounts": [200000, 50000], "total": 250000 },
      "total_expenses": { "amounts": [0, 0], "total": 0 },
      "total_revenue": { "amounts": [200000, 50000], "total": 250000 }
    }
  }
}
```

### `report income-statement --compare` / `report trial-balance --compare`

`meta.command` is `report.income-statement.compare` or
`report.trial-balance.compare`. `change` is `current - prior`;
`change_percent` is `change / |prior| * 100` rounded to two decimals, or
`null` when `prior` is 0. `totals` uses the same keys as the periods mode.

```json
{
  "data": {
    "report": "income-statement",
    "mode": "compare",
    "compare": "prior-year",
    "from": "2026-01-01",
    "to": "2026-03-31",
    "currency": "USD",
    "current": { "label": "2026-Q1", "from": "2026-01-01", "to": "2026-03-31" },
    "prior": { "label": "2025-Q1", "from": "2025-01-01", "to": "2025-03-31" },
    "accounts": [
      {
        "code": "4000",
        "name": "Sales",
        "type": "revenue",
        "normal_balance": "credit",
        "current": 250000,
        "prior": 100000,
        "change": 150000,
        "change_percent": 150.0
      }
    ],
    "totals": {
      "net_income": { "current": 210000, "prior": 70000, "change": 140000, "change_percent": 200.0 }
    }
  }
}
```

CSV output for these modes has the columns `code,name,type,<period labels...>,total`
(periods) or `code,name,type,current,prior,change,change_percent` (compare),
with amounts in minor units. Summary rows follow the accounts with an empty
`code`, the summary key (e.g. `net_income`) in `name` and `type` set to
`total`; `change_percent` is `n/a` when the prior amount is 0.

### `report reconciliation`

Amounts are minor units on the account's normal side. `statement_balance`,
`difference` (`statement - cleared`), and `reconciled` (`difference == 0`) are
`null` unless `--statement-balance` is given; `as_of` is `null` without `--to`;
`statement` is `null` without `--statement`; `group_by_metadata` and `groups`
are `null` without `--group-by-metadata`; `metadata_filters` lists the
`--filter-metadata` pairs (empty without them).

`cleared_balance` counts only `--statement`'s clearings when given;
`total_cleared_balance` always counts every statement's (they are equal without
`--statement`). With `--to`, a clearing counts toward the cleared balances when
`cleared_date <= as_of`, and toward `reconciled_balance` only when it is
reconciled and `reconciled_date <= as_of` (a clearing reconciled after `as_of`
is cleared but not reconciled as of `as_of`; same with `--statement`).
Invariant: `book_balance - total_cleared_balance == uncleared_total`.

`uncleared_entries` lists every entry not fully cleared. Per item: `amount` is
the raw (positive) entry amount and `signed_amount` its effect on the
normal-side balance; `cleared_amount` and `remaining_amount` are raw amounts
(`amount = cleared_amount + remaining_amount`); `signed_remaining` is the
remaining amount on the normal side (they sum to `uncleared_total`);
`partially_cleared` is true when `0 < cleared_amount < amount`. `reference` is
the transaction reference (or `null`); `metadata` is the transaction metadata,
parsed as JSON when valid, else the raw string (or `null`).
`reconciled_amount` is the raw amount reconciled as of `as_of`. `clearings`
lists the item's clearings as of `as_of`, oldest first: `id`, `amount` (raw),
`cleared_date`, `reconciled_date`, `statement`, `status`. A clearing
reconciled after `as_of` appears with `status: "cleared"` and
`reconciled_date: null`.

Each `groups` element has the group `value` (`"(unassigned)"` for items without the
key, listed last), `count`, `uncleared_total` (sum of `signed_remaining`), and
the member `entry_ids`.

```json
{
  "data": {
    "account": {
      "code": "1200",
      "name": "Commission Receivable",
      "type": "asset",
      "normal_balance": "debit"
    },
    "currency": "USD",
    "as_of": "2026-03-31",
    "statement": "STMT-2026-03",
    "metadata_filters": [{ "key": "carrier", "value": "Acme" }],
    "group_by_metadata": "policy",
    "book_balance": 15000,
    "cleared_balance": 4000,
    "total_cleared_balance": 4000,
    "reconciled_balance": 0,
    "uncleared_total": 11000,
    "uncleared_count": 2,
    "partially_cleared_count": 1,
    "statement_balance": 4000,
    "difference": 0,
    "reconciled": true,
    "uncleared_entries": [
      {
        "entry_id": 1,
        "transaction_id": 1,
        "date": "2026-01-05",
        "reference": "POL-1",
        "description": "Commission POL-1",
        "memo": null,
        "metadata": { "carrier": "Acme", "policy": "P1" },
        "direction": "debit",
        "amount": 10000,
        "signed_amount": 10000,
        "cleared_amount": 4000,
        "reconciled_amount": 0,
        "remaining_amount": 6000,
        "signed_remaining": 6000,
        "partially_cleared": true,
        "clearings": [
          {
            "id": 9,
            "amount": 4000,
            "cleared_date": "2026-03-31",
            "reconciled_date": null,
            "statement": "STMT-2026-03",
            "status": "cleared"
          }
        ]
      },
      {
        "entry_id": 5,
        "transaction_id": 3,
        "date": "2026-01-15",
        "reference": "POL-3",
        "description": "Commission POL-3",
        "memo": null,
        "metadata": { "carrier": "Acme" },
        "direction": "debit",
        "amount": 5000,
        "signed_amount": 5000,
        "cleared_amount": 0,
        "reconciled_amount": 0,
        "remaining_amount": 5000,
        "signed_remaining": 5000,
        "partially_cleared": false,
        "clearings": []
      }
    ],
    "groups": [
      { "value": "P1", "count": 1, "uncleared_total": 6000, "entry_ids": [1] },
      { "value": "(unassigned)", "count": 1, "uncleared_total": 5000, "entry_ids": [5] }
    ]
  }
}
```

#### `report reconciliation --mode settlement`

The common fields and envelope above remain; bank mode adds no new keys.
Settlement mode requires `--statement` and an asset/liability account. Its
`cleared_balance` counts charge clearings on that statement, plus negative
charge-reversal clearings, irrespective of payment-entry clearance.
`difference = statement_balance - cleared_balance`; `reconciled` is a statement
comparison, not a claim that no short pays remain. Both are `null` without
`--statement-balance`.

Additional top-level `data` keys:

| Key | Meaning |
|-----|---------|
| `mode` | `"settlement"` |
| `link_key` | Effective payment link metadata key, default `"applies_to"` |
| `charges` | All selected net charges, ordered by date/id, including fully applied ones |
| `negative_charges` | Charge reversals, with signed negative expected/statement amounts |
| `applied_settlements` | Credits with at least one usable same-account/currency charge link |
| `unmatched_settlements` | Credits without a usable link, with reasons; never applied FIFO |

Charges have `entry_id` (representative entry for a net transaction),
`transaction_id`, `date`, `reference`, `description`, `metadata`, `group`,
`expected`, `applied_this_statement`, `applied_total`, `reconciled_this_statement`,
`remaining`, `short_pay`,
`overpay`, and `age_days` (effective `--to` minus original charge date).
Amounts are minor units. Charge reversals reduce the original charge's
`expected`; their separate negative rows retain the reversal date and statement
application. `applied_total` uses the greater of linked payment allocations and
charge clearings across statements, without adding the two representations.
`remaining = max(expected - applied_total, 0)`;
`overpay = max(applied_total - expected, 0)`. `short_pay` is remaining for a charge
with a positive application on this statement, otherwise zero.

Settlement rows have `entry_id`, `transaction_id`, `date`, `reference`,
`description`, `metadata`, positive `amount`, `charge_transaction_ids` in link
order, `excess`, and `reasons`. Reasons use aging's snake-case vocabulary:
`not_found`, `not_on_account`, `not_a_charge`, `charge_after_settlement`,
`after_as_of`, `invalid`, plus `missing_link`. Links can be references, ids,
or arrays, with the same reference-first resolution as aging. Arrays allocate
in order; excess remains on the last valid charge and is counted in its overpay.
Partially usable arrays appear in `applied_settlements` with their reasons.

`uncleared_entries` retains the existing item shape but contains only open net
charges. `amount`/`signed_amount` are effective expected amounts,
`cleared_amount` is effective total application, `remaining_amount` and
`signed_remaining` are the open remainder. `clearings` lists the representative
entry's actual clearings, which can differ from linked payment allocations.
`uncleared_total` sums open charge remainders. The bank balance invariant does
not apply in this mode. Filters select charge metadata after linking; linked
credits follow selected charges and unmatched credits use their own metadata.
Groups subtotal open charges. Explanations cover the account through `--to`;
only statement application and comparison are scoped to `--statement`.

For the $1,800 expected commission and $1,620 payment example (excerpt):

```json
{
  "data": {
    "mode": "settlement",
    "link_key": "applies_to",
    "statement": "S1",
    "statement_balance": 162000,
    "cleared_balance": 162000,
    "difference": 0,
    "reconciled": true,
    "uncleared_total": 18000,
    "uncleared_count": 1,
    "charges": [{
      "reference": "P1-2026-10",
      "date": "2026-10-01",
      "expected": 180000,
      "applied_this_statement": 162000,
      "applied_total": 162000,
      "remaining": 18000,
      "short_pay": 18000,
      "overpay": 0,
      "age_days": 30
    }],
    "negative_charges": [],
    "applied_settlements": [{
      "reference": "PAY-S1-P1",
      "amount": 162000,
      "charge_transaction_ids": [1],
      "excess": 0,
      "reasons": []
    }],
    "unmatched_settlements": []
  }
}
```

CSV keeps the bank header and common row types, adding `charge`,
`negative_charge`, `applied_settlement`, and `unmatched_settlement`.
Charge `amount`/`cleared_amount`/`remaining_amount` mean
expected/applied-this-statement/remaining. Settlement `amount` is its positive
credit and `remaining_amount` its excess; `description` includes any reasons.

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

Same shape as a single element from `txn list` (as a one-element array), with
entries and `attachments` included. Each entry also carries:

- `cleared_amount`: total cleared amount (all clearings, minor units);
- `remaining_amount`: `amount - cleared_amount`;
- `clearings`: the entry's clearings, oldest first, each with `id`, `amount`,
  `cleared_date`, `reconciled_date` (`null` unless `status` is `reconciled`),
  `statement` (or `null`) and `status` (`cleared` or `reconciled`).

```json
{
  "data": [
    {
      "id": 42,
      "description": "Commission POL-123",
      "metadata": null,
      "reference": "POL-123",
      "currency": "USD",
      "date": "2026-03-05",
      "entries": [
        {
          "id": 5,
          "account_code": "1200",
          "direction": "debit",
          "amount": 10000,
          "memo": null,
          "status": "uncleared",
          "cleared_amount": 6500,
          "remaining_amount": 3500,
          "clearings": [
            {
              "id": 9,
              "amount": 4000,
              "cleared_date": "2026-03-31",
              "reconciled_date": "2026-04-05",
              "statement": "STMT-2026-03",
              "status": "reconciled"
            },
            {
              "id": 12,
              "amount": 2500,
              "cleared_date": "2026-04-30",
              "reconciled_date": null,
              "statement": "STMT-2026-04",
              "status": "cleared"
            }
          ]
        },
        {
          "id": 6,
          "account_code": "4000",
          "direction": "credit",
          "amount": 10000,
          "memo": null,
          "status": "uncleared",
          "cleared_amount": 0,
          "remaining_amount": 10000,
          "clearings": []
        }
      ],
      "attachments": []
    }
  ]
}
```

### `txn clear`

Single entry (`TXN_ID --entry ID` or `--reference REF --account CODE`).
`status` is the requested status; `entry_status` the entry's derived status
afterwards. `action` is `cleared` (a clearing was added), `reconciled`
(clearings were promoted, possibly with a new reconciled clearing),
`uncleared` (clearings were removed) or `skipped` (nothing changed;
`skip_reason` is `already_cleared_by_statement`, `fully_cleared` or
`nothing_to_unclear`). `amount` is what this call cleared (or removed);
`cleared_amount`/`remaining_amount` are the entry's totals afterwards.
`clearing_id` is the new clearing's id, or `null`. `reconciled_date` is
`--date` with `--status reconciled` (the reconciled date it set), else `null`.

Validation errors (exit 3): a `--date` before the transaction date; with
`--status reconciled`, a `--date` before a promoted clearing's cleared date;
and `--status cleared` on reconciled (locked) clearings, whose message says to
run `--status uncleared` first and then clear again.

```json
{
  "data": {
    "reference": "POL-123",
    "transaction_id": 42,
    "entry_id": 5,
    "action": "cleared",
    "clearing_id": 9,
    "amount": 4000,
    "entry_amount": 10000,
    "cleared_amount": 4000,
    "remaining_amount": 6000,
    "previous_status": "uncleared",
    "entry_status": "uncleared",
    "skip_reason": null,
    "status": "cleared",
    "cleared_date": "2026-03-31",
    "reconciled_date": null,
    "statement": "STMT-2026-03"
  }
}
```

Batch (`--from-file`). The batch is atomic: on any error nothing is applied
and the error envelope names the failing line. `cleared` holds every item that
changed (whatever its `action`), `skipped` the rest; each item has the fields
above (except the batch-level `status`, `cleared_date`, `reconciled_date`, `statement`) plus its
1-based input `line`. `cleared_total` sums `amount` over `cleared`.

```json
{
  "data": {
    "account": "1200",
    "status": "cleared",
    "cleared_date": "2026-03-31",
    "reconciled_date": null,
    "statement": "STMT-2026-03",
    "count": 2,
    "cleared_count": 1,
    "skipped_count": 1,
    "cleared_total": 4000,
    "cleared": [
      {
        "line": 1,
        "reference": "POL-123",
        "transaction_id": 42,
        "entry_id": 5,
        "action": "cleared",
        "clearing_id": 9,
        "amount": 4000,
        "entry_amount": 10000,
        "cleared_amount": 4000,
        "remaining_amount": 6000,
        "previous_status": "uncleared",
        "entry_status": "uncleared",
        "skip_reason": null
      }
    ],
    "skipped": [
      {
        "line": 2,
        "reference": "POL-124",
        "transaction_id": 43,
        "entry_id": 7,
        "action": "skipped",
        "clearing_id": null,
        "amount": 0,
        "entry_amount": 2500,
        "cleared_amount": 2500,
        "remaining_amount": 0,
        "previous_status": "cleared",
        "entry_status": "cleared",
        "skip_reason": "already_cleared_by_statement"
      }
    ]
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

### `report cash-flow`

Amounts are signed minor units (positive = cash inflow).

```json
{
  "data": {
    "from": "2026-01-01",
    "to": "2026-03-31",
    "currency": "USD",
    "cash_accounts": [{ "code": "1000", "name": "Checking" }],
    "operating": {
      "lines": [
        { "code": "4000", "name": "Sales", "account_type": "revenue", "amount": 65000 },
        { "code": "5000", "name": "Rent", "account_type": "expense", "amount": -25000 }
      ],
      "total": 40000
    },
    "investing": { "lines": [], "total": 0 },
    "financing": {
      "lines": [{ "code": "2000", "name": "Loan Payable", "account_type": "liability", "amount": 100000 }],
      "total": 100000
    },
    "net_change": 140000,
    "opening_cash": 100000,
    "closing_cash": 240000,
    "reconciled": true,
    "difference": 0
  }
}
```

### `report aging`

Amounts are minor units. `counterparty` is `null` (and `group_by` is `null`) when `--group-by-metadata`/`--group-by` is not used; otherwise it is the metadata value or `"(unassigned)"` (missing key, non-JSON or non-object metadata). `max_days` is `null` for the open-ended last bucket.

The report has two views of the same open items:

- `accounts[]`: one section per aged account (code order), each with its own `groups`, `totals`, `account_balance` and `unmatched_links`. Each section is identical to a single-account run of that account.
- `groups[]`: the counterparty groups combined across accounts (with one account, the same as that account's `groups`). `--group-across-accounts` only changes table and CSV output; JSON always has both views.

Top-level `totals` are the sums of the per-account totals, `account_balance` is the sum of the accounts' balances (normal direction, as of `as_of`, in `currency`), and `unmatched_links` holds every account's links. Without a metadata filter, `totals.net` (= `total_open - unapplied_credit`) always equals `account_balance`, both overall and per account.

- `include_children` and `group_across_accounts` echo the flags.
- `metadata_filters` lists the `--filter-metadata` pairs as `{ "key", "value" }` (`[]` without them). With a filter, groups and items contain only matching open charges (after every settlement was applied), groups without matching items are dropped, `unapplied_credit` is `0`, and `account_balance` stays unfiltered, so `totals.net` no longer equals it.
- `method` is `"fifo"` or `"specific"`; `basis` is `"fifo-by-transaction-date"` or `"specific-then-fifo-by-transaction-date"` accordingly.
- `link_key` is the metadata key used for links with `"specific"` (default `"applies_to"`), `null` with `"fifo"`.
- `net` is negative and `net_credit` is `true` when a group (or the totals) is in net credit, i.e. settlements exceed the open charges.
- `open_items[].account` is the account the charge is on; `reference` is the charge's transaction reference, or `null`. `open_items` is always present (`--detail` only affects table and CSV output).
- `unmatched_links` lists settlement links that were not honoured (always `[]` with `"fifo"`); each settlement was applied FIFO instead (`fallback: "fifo"`). `account` is the account the settlement is on. `link` is the value as found in metadata (string, number, or the raw invalid value). `reason` is one of `not_found`, `not_on_account`, `not_a_charge`, `charge_after_settlement`, `after_as_of`, `other_counterparty`, `other_account`, `invalid` (see `commands.md`). `counterparty` is the settlement's group (`null` when not grouping); `settlement_amount` is its full amount.

Two receivable accounts grouped by customer, filtered to disputed charges (the second account's section and the `Initech` group are elided):

```json
{
  "data": {
    "kind": "receivable",
    "as_of": "2026-03-01",
    "currency": "USD",
    "include_children": false,
    "group_by": "customer",
    "group_across_accounts": false,
    "metadata_filters": [{ "key": "status", "value": "disputed" }],
    "method": "specific",
    "link_key": "applies_to",
    "basis": "specific-then-fifo-by-transaction-date",
    "buckets": [
      { "label": "Current", "min_days": 0, "max_days": 0 },
      { "label": "1-30", "min_days": 1, "max_days": 30 },
      { "label": "30+", "min_days": 31, "max_days": null }
    ],
    "accounts": [
      {
        "code": "1210",
        "name": "AR East",
        "type": "asset",
        "groups": [
          {
            "counterparty": "Globex",
            "buckets": [
              { "label": "Current", "amount": 0 },
              { "label": "1-30", "amount": 0 },
              { "label": "30+", "amount": 10000 }
            ],
            "total_open": 10000,
            "unapplied_credit": 0,
            "net": 10000,
            "net_credit": false,
            "open_items": [
              {
                "account": "1210",
                "transaction_id": 1,
                "date": "2026-01-05",
                "reference": "INV-1",
                "description": "Invoice 1",
                "age_days": 55,
                "bucket": "30+",
                "original_amount": 10000,
                "open_amount": 10000
              }
            ]
          }
        ],
        "totals": {
          "buckets": [
            { "label": "Current", "amount": 0 },
            { "label": "1-30", "amount": 0 },
            { "label": "30+", "amount": 10000 }
          ],
          "total_open": 10000,
          "unapplied_credit": 0,
          "net": 10000,
          "net_credit": false
        },
        "account_balance": 11000,
        "unmatched_links": []
      }
    ],
    "groups": [
      {
        "counterparty": "Globex",
        "buckets": [
          { "label": "Current", "amount": 0 },
          { "label": "1-30", "amount": 0 },
          { "label": "30+", "amount": 10000 }
        ],
        "total_open": 10000,
        "unapplied_credit": 0,
        "net": 10000,
        "net_credit": false,
        "open_items": [
          {
            "account": "1210",
            "transaction_id": 1,
            "date": "2026-01-05",
            "reference": "INV-1",
            "description": "Invoice 1",
            "age_days": 55,
            "bucket": "30+",
            "original_amount": 10000,
            "open_amount": 10000
          }
        ]
      }
    ],
    "totals": {
      "buckets": [
        { "label": "Current", "amount": 0 },
        { "label": "1-30", "amount": 7000 },
        { "label": "30+", "amount": 10000 }
      ],
      "total_open": 17000,
      "unapplied_credit": 0,
      "net": 17000,
      "net_credit": false
    },
    "account_balance": 16000,
    "unmatched_links": [
      {
        "account": "1220",
        "settlement_transaction_id": 5,
        "settlement_date": "2026-02-11",
        "settlement_amount": 2000,
        "counterparty": "(unassigned)",
        "link": "INV-1",
        "reason": "other_account",
        "fallback": "fifo"
      }
    ]
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

OFX success responses use the following shape:

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

CSV/JSON responses retain `dry_run`, `imported`, `skipped`, `errors`, and
`transactions`. Each transaction outcome includes `date`, `description`,
`reference`, and `status` (`imported`/`skipped`). Newly posted outcomes include
`id`; dry runs and skipped outcomes omit it. CSV/JSON outcomes omit OFX's
`amount` and `reason` fields because JSON journals can contain split entries.
Use `txn show` to retrieve the posted entries and amounts.

```json
{
  "ok": true,
  "meta": {"command": "txn.import", "company": "acme", "timestamp": "2026-03-15T00:00:00Z"},
  "data": {
    "dry_run": false,
    "imported": 1,
    "skipped": 0,
    "errors": 0,
    "transactions": [
      {"id": 50, "date": "2026-03-15", "description": "Office supplies", "reference": "receipt-1001", "status": "imported"}
    ]
  }
}
```

CSV/JSON batch failures return the standard error envelope on stderr and no
success data. Nothing in the batch is committed. The success `errors` count
is therefore always zero. See [commands.md](commands.md#bk-txn-import) for the
JSON input schema, exact major-unit amounts, column mappings, and hash references.

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
      "before": "{\"cleared_amount\":0,\"status\":\"uncleared\"}",
      "after": "{\"cleared_amount\":4000,\"clearings\":[{\"amount\":4000,\"cleared_date\":\"2026-03-31\",\"id\":9,\"reconciled_date\":null,\"statement\":\"STMT-2026-03\",\"status\":\"cleared\"}],\"op\":\"clear\",\"status\":\"uncleared\"}"
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
`action` is one of `status_change`, `correlate`, `reverse`, `budget_set`, `budget_delete`,
`account_delete`, `company_delete`.

`status_change` rows come from `txn clear`. `before`/`after` carry the entry's
derived `status` and total `cleared_amount`; `after` also carries `op`
(`clear`, `reconcile` or `unclear`) and `clearings`, the affected clearing rows
(for `unclear`, the removed ones), each with `id`, `amount`, `cleared_date`,
`reconciled_date` (`null` unless reconciled), `statement` and `status`.

## Parsing Tips for Agents

1. **Check `ok` first**: If `false`, read `error.code` and `error.message` to decide next steps
2. **Use `meta.command`** to confirm the response matches the expected command
3. **Convert amounts**: Divide by `10^minor_units` for the currency (2 for USD/EUR/MXN, 0 for JPY, 3 for BHD)
4. **Handle null fields**: `metadata`, `reference`, `memo`, `notes`, `description` can be null
5. **Parse `metadata` as JSON**: when non-null, transaction `metadata` is a string containing a JSON object (free text from `-m` appears as `{"memo": "..."}`; intercompany links as `{"correlate": <id>}`). Databases created before schema v9 are migrated to this form automatically on open.
6. **Status messages go to stderr**: Use `--quiet` to suppress them; only JSON goes to stdout
