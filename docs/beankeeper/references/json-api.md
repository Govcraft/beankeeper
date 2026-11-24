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
| `UNDO_UNSUPPORTED` | The receipt's action has no undo operation (exit 3) |
| `UNDO_INELIGIBLE` | Current ledger state blocks the receipt's undo (exit 3) |
| `UNAUTHORIZED` | Missing required, invalid, expired, revoked or untrusted agent token (exit 6) |
| `FORBIDDEN` | Verified token lacks the required capability (exit 7) |
| `GENERAL` | Other error |
| `IO` | File I/O error |

## Amount Conventions

Monetary fields retain their **raw integer minor units** and add a sibling named
`<field>_display` containing a fixed-point decimal string in major units, without
grouping separators or currency symbols. For example:

```json
{"currency":"USD", "amount":180000, "amount_display":"1800.00"}
```

The same rule covers transaction entries and clearings, OFX import rows, budget
amounts, every report balance and total, and period/comparison values. An
`amounts` array gets an `amounts_display` array with the same order. Signs and
trailing zeros follow the currency, including JPY (0 decimals) and BHD/KWD (3).
No floating-point conversion is used. IDs, counts, ages and percentages do not
get monetary companions; user metadata is unchanged. Schema examples below
omit some display companions for readability.

Currency comes from the object or its enclosing transaction/report. Standard
statements that omit currency in their payload now include `meta.currency`.
An absent monetary value has a null display. When the currency cannot be
established, the display is also null: this includes historical budget audit
snapshots without currency, account totals without a currency, and the existing
aggregate minor-unit total of a mixed-currency clearing batch. Individual items
in such a batch still carry currency and correct decimal strings. No default
scale is assumed for these ambiguous values.

Audit `before`/`after` strings stay byte-for-byte unchanged, preserving their
hashes. Additive `before_display`/`after_display` objects contain parsed snapshots
with monetary display siblings (null if the snapshot is absent or unparseable).
New budget audit snapshots include currency, including deletion records.

Standalone exports retain their versioned file format. To convert the original
integers yourself:

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

## Pagination

`txn list` (except `--count`) and `audit log` preserve `data` arrays and add:

```json
{"meta":{"pagination":{"limit":50,"offset":0,"returned":50,"total":57,"has_more":true}}}
```

`total` counts all rows matching the current filters before pagination.
`has_more` means `offset + returned < total`, including an empty page requested
with limit zero. Use a positive limit and advance the offset by `returned` while
`has_more` is true. Negative transaction limits retain SQLite’s unlimited
semantics. Audit limits default to 50 and must be between 0 and 100. Nonempty
audit pages with more results also return `next_offset`. Use it with the same
filters. Negative offsets act as zero. A page ending exactly at the total has
`has_more: false`.
Concurrent ledger changes can affect later pages. Table mode prints truncation
notices to stderr unless `--quiet` is set.

MCP `txn_list` and `audit_log` use the same fields in the tool result’s
`_meta.pagination`, preserving their array payloads in text/structured content.
MCP monetary display fields are identical to the CLI data payload.

## Historical trend payload

`report trend` uses command ID `report.trend`. For example:

```json
{
  "data": {
    "metric": "spending",
    "period": "month",
    "from": "2026-01-01",
    "to": "2026-02-28",
    "currency": "USD",
    "columns": [
      {"label": "2026-01", "from": "2026-01-01", "to": "2026-01-31"},
      {"label": "2026-02", "from": "2026-02-01", "to": "2026-02-28"}
    ],
    "series": [
      {"code": "5000", "name": "Rent", "amounts": [10000, 0], "amounts_display": ["100.00", "0.00"]},
      {"code": null, "name": "Total Spending", "amounts": [10000, 0], "amounts_display": ["100.00", "0.00"]}
    ]
  }
}
```

`metric: "net-worth"` returns Total Assets, Total Liabilities, and Net Worth
series with null codes and cumulative period-end balances. `amounts` and
`amounts_display` align with `columns`; no sum across time is emitted. Dates
are inclusive. MCP returns this same data payload.

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

### `company delete`

```json
{
  "data": { "deleted": "old-company" }
}
```

The MCP `company_delete` tool returns this same `data` object. Companies with
retained ledger or setup data return `VALIDATION` and keep their history.

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

### `account rename`

A direct rename returns the same `data` in the CLI envelope and from the MCP
`account_rename` tool.

```json
{
  "status": "renamed",
  "code": "1000",
  "previous_name": "Cash",
  "name": "Operating Cash",
  "receipt_id": 42,
  "desktop_path": "/changes?company=acme&after=41&through=42#receipt-42"
}
```

A no-op with no matching rename receipt returns `status: "unchanged"`, `code`,
and `name`. A propose-only request returns `status: "proposed"`, `code`, `name`,
and `proposal_id`. Rename proposals in `txn_proposals` have
`kind: "account_rename"`. Approving one returns its `receipt_id` and
`desktop_path`. Undo returns `kind: "account_name_restored"`, `account`, the
restored `name`, and `undo_receipt`.

### `loan schedule` and `loan pay`

`loan.schedule` needs no company or ledger unless agent-token verification is
requested. Its `data` object contains `currency`, original `principal`,
`annual_rate` (a percentage string with four decimal places), requested `months`,
`regular_payment`, `total_interest`, `total_paid`, and `payments`. Every payment
has a one-based `number`, `payment`, `principal`, `interest`, and remaining
`balance`. All monetary fields are minor-unit integers with corresponding
`_display` strings inherited from `currency`. The number of payments can be
less than `months` if rounding pays off a very small loan early.

For `--principal 10 --rate 0 --months 3 --currency USD`, the monetary values are
`regular_payment: 333`, `total_interest: 0`, `total_paid: 1000`. Payment principal
is `333`, `333`, `334`; balances are `667`, `334`, `0`.

`loan.pay` uses the `txn post` result schema below with command `loan.pay` and
the resolved company. A duplicate skipped reference returns the existing
transaction identity without attaching the newly calculated split. Read
`txn show` to inspect the actual posted entries.

### `txn post`

Direct grants return a posted transaction ID. The unused ID field is omitted.

```json
{
  "data": {
    "id": 42,
    "created": true,
    "skipped": false,
    "status": "posted"
  }
}
```

When `--on-conflict skip` and reference already exists:

```json
{
  "data": {
    "existing_id": 42,
    "created": false,
    "skipped": true,
    "status": "posted"
  }
}
```

A `propose-only` grant returns the same pending proposal payload as
`txn propose`, with a `proposal_…` ID, `status: "pending"` and a null
`transaction_id`. With `--on-conflict skip`, an already posted reference
returns `{"status":"skipped","transaction_id":42}`. MCP `txn_post` returns
the same data payloads.

### `txn settle-hold` / `txn_settle_hold`

A direct settlement returns the posted transaction ID, the original hold ID,
and the posted amount minus the original authorization in minor units. An
exact retry returns the same IDs and delta with `status: "skipped"`. A
propose-only settlement uses the proposal response with
`payload.purpose: {"settle":42}`.

```json
{
  "transaction_id": 65,
  "settlement": {"hold_id": 42, "amount_delta": 43},
  "amount_delta_display": "0.43",
  "status": "created",
  "currency": "USD",
  "date": "2026-03-04",
  "desktop_view": {
    "view_id": "transactions",
    "url": "/transactions?company=acme&currency=USD&from=2026-03-04&to=2026-03-04"
  }
}
```

Transaction results from `txn post`, `txn show`, and `txn list` carry the
journal's `status` of `pending` or `posted`. The transaction's entries expose
that value as `posting_status`. Entry `status` describes statement clearance.
A settled transaction also has `settlement`; the original remains in history.
`account_list` with balances returns `posted_balance` and `available_balance`
with decimal display companions and an optional Desktop register link.

### `txn meta set/unset`

Envelopes use `txn.meta.set` and `txn.meta.unset`. Both return:

```json
{"id": 42, "metadata": "{\"status\":\"disputed\"}", "changed": true}
```

`metadata` is the whole stored JSON object as text, matching `txn show`/`list`.
A no-op returns `changed: false`, preserves the stored text (including null),
and appends no audit row. A changed batch appends one `metadata_set` or
`metadata_unset` audit row with whole-object snapshots. No prior metadata is
represented by `{}` in snapshots. Removing the last key stores `{}`.
Values are strings. Reserved relationship keys fail with `VALIDATION`;
unknown company-scoped IDs or references fail with `NOT_FOUND`.

### `txn reverse` (alias `txn void`)

The envelope uses `meta.command: "txn.reverse"`. A new mirror returns
`id`, `created: true`, and `skipped: false`.
Retrying with an explicit reference matching the linked reversal returns
`existing_id`, `created: false`, `skipped: true`, without another audit row.
Other repeat attempts fail with `VALIDATION` (exit 3). Missing or foreign
transaction IDs return `NOT_FOUND` (exit 5).

Parse the transaction's
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
(periods). Comparison CSV uses
`row_type,code,name,current,prior,change,change_percent`,
with amounts in minor units. Period summary rows have an empty `code`, the
summary key in `name` and `type` set to `total`. Comparison totals use
`row_type: total` and the summary key in `name`; other row types identify
accounts, movers, explanations
and summary/link rows; `change_percent` is empty when the prior amount is 0.

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

The CLI JSON `data` field and MCP `verify` structured result use the same
object. Example excerpt from a ledger with one company and one posting:

```json
{
  "schema_version": 29,
  "status": "healthy",
  "companies": [{
    "company": "acme",
    "currencies": [{
      "currency": "USD",
      "minor_unit_digits": 2,
      "debit_minor": "1250",
      "credit_minor": "1250",
      "balanced": true
    }]
  }],
  "audit_chain": {
    "head_hash": "<64 lowercase hexadecimal digits>",
    "length": 4,
    "first_id": 1,
    "last_id": 4,
    "intact": true
  },
  "row_counts": {
    "companies": 1,
    "accounts": 2,
    "transactions": 1,
    "entries": 2,
    "audit_rows": 4
  },
  "checks": [
    { "name": "schema_version", "outcome": "passed" },
    { "name": "audit_hash_chain", "outcome": "passed" },
    { "name": "accounting", "outcome": "passed" }
  ],
  "desktop_view": { "link": { "view_id": "review" } }
}
```

Totals are exact base-10 strings in minor units. In this example `"1250"`
represents USD 12.50. Companies without postings have an empty `currencies`
array. Audit length counts rows, including when IDs have historical gaps.
An empty audit chain has length zero and null `first_id`, `last_id`, and
`head_hash`. `intact` certifies that the recomputed hashes, predecessor links
and SQLite high-water mark passed for the local snapshot.

The full `checks` array includes schema version, audit chain, policy, period
locks, proposal resolutions, spend limits, accounting, creation snapshots,
account archives, current renamed labels, relationship links, statement batches, foreign keys, SQLite integrity and row
counts in execution order. Checks have outcome `passed` or `skipped` when the
snapshot schema predates that check. A failure returns a `DATABASE` error
instead of a success report. Its message begins with the failed check name.
Desktop hosts can resolve `desktop_view.link` via `ui_get_link` on their own
listener to offer optional verification status and recheck.

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

### `rule undo`

`bk --company COMPANY rule undo RULE_ID --json` and MCP
`rule_undo` with `{"company":"acme","id":"rule_01h455vb4pex5vsknk084sn02q"}`
return the same data. Both require a signed `ledger/admin` credential.

```json
{
  "rule": "rule_01h455vb4pex5vsknk084sn02q",
  "undo_receipt": 43
}
```

The rule stops matching future imports. The `rule_deactivate` event uses the
original rule ID as its `entity_id` and links to its creation through `undo_of`.
Its snapshots decode to `"active"` and `"deactivated"`. A retry returns the
same undo receipt. A superseded or deleted original returns `UNDO_INELIGIBLE`.
An ID created by an amendment returns `UNDO_UNSUPPORTED`. An unknown ID or
an ID belonging to another company returns `NOT_FOUND`.

### `audit log`

`data` is an array of recorded changes, most recent first. `before`/`after`
are JSON strings (snapshots of the relevant fields) or `null`; `company_slug`
may be `null` for cross-company changes. Owner undo rows include an `undo_of`
field containing the original receipt ID.

CLI JSON and MCP `audit_log` rows include a nullable `client_hint`. When known
environment markers are present on a write without a verified identity, the
hint has this shape:

```json
{"verified": false, "source": "env", "runtime": "claude-code", "markers": ["CLAUDECODE"]}
```

Runtime labels are `claude-code`, `codex`, `gemini-cli`, `cursor-agent`, and
`generic`. Use `agent_uri` for verified attribution. CLI table and CSV outputs
show the runtime with an unverified label.

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

### Direct onboarding receipts

The direct setup commands and MCP tools return a numeric `receipt_id`, the
operation's result, and a `desktop_path` to its audit receipt. A successful
statement import returns the format, `source_sha256`, `created`, and
`preexisting`. Each item in those arrays contains its statement reference
and `transaction_id`.

The owner can pass the receipt ID to `audit undo` or `audit_undo`.
The response has `kind: "onboarding_reverted"` and an `undo_receipt`.
`child_receipts` identifies the account archives or mirror postings.
A profile undo returns its `profile_revision` and whether the book was
archived. Exact retries return the recorded result.

### `audit receipts`

CLI JSON `data` and MCP `audit_receipts` structured content have the same
shape. Provide `company` and optionally `cursor` (an exclusive nonnegative
audit ID, default 0) and `limit` (1 to 100, default 50).

```json
{
  "company": "acme",
  "after": 40,
  "through": 65,
  "continuation": "acme:52:65",
  "desktop_path": "/changes?company=acme&after=40&through=52",
  "receipts": [
    {
      "id": 52,
      "change": {
        "id": 52,
        "changed_at": "2026-11-23T15:00:00Z",
        "actor": "bookkeeper",
        "attribution": {"kind": "unverified", "client_hint": null},
        "target": {"kind": "account", "id": "5000"},
        "action": "create",
        "summary": {
          "kind": "account_created",
          "after": {"name": "Supplies", "type": "expense", "default_tax_category": null}
        },
        "source_reference": null,
        "related_posting": null,
        "undo_of": null
      },
      "posting": null
    }
  ]
}
```

The receipt array contains records in ascending audit order. Each includes a
positive numeric `id`, a primary `change`, and a nullable `posting` with the
linked posting event for a reversal or approved proposal. Events include the
actor, attribution, timestamp, typed target, action and change summary, plus
nullable `source_reference`, `related_posting`, and `undo_of` fields. Posting
summaries carry a currency and amounts as integer minor units.

While `continuation` is present, pass it unchanged as MCP `continuation` or
CLI `--continuation` with the same company and ledger. Leave `cursor` out on
these calls. `through` is the audit head captured for the first page. Once
`continuation` is null, save `through` as the cursor for a later read, even
when the page is empty. `desktop_path` is relative to the optional Desktop
listener serving the same books. Desktop displays up to 50 receipts per view.

### `audit undo`

Pass the positive receipt ID to `bk --company acme --json audit undo 52`, or
call MCP `audit_undo` with `{"company":"acme","receipt":52}`. Both require a
signed `ledger/admin` token regardless of the ledger's enforcement setting.
The write applies immediately, and retries for the same receipt return the
original result.

Successful undo results include `company`, `undo_receipt`, and the relative
`desktop_path` to the new audit receipt:

- `kind: "reversed"` includes `transaction` (original ID) and `reversal` (new ID).
- `kind: "archived"` includes the archived `account` code.
- `kind: "onboarding_reverted"` includes the [onboarding undo details](#direct-onboarding-receipts).

Expect `NOT_FOUND` for a receipt missing from the selected company,
`UNDO_UNSUPPORTED` when the change has no undo, and `UNDO_INELIGIBLE` when
current state blocks an otherwise supported undo. CLI writes the error
envelope to stderr. MCP uses `isError: true` and structured `{code, message}`.

## Parsing Tips for Agents

1. **Check `ok` first**: If `false`, read `error.code` and `error.message` to decide next steps
2. **Use `meta.command`** to confirm the response matches the expected command
3. **Present amounts**: Use `<field>_display` for decimal major units; retain integer fields for exact arithmetic. Check pagination before treating a list as complete.
4. **Handle null fields**: `metadata`, `reference`, `memo`, `notes`, `description` can be null
5. **Parse `metadata` as JSON**: when non-null, transaction `metadata` is a string containing a JSON object (free text from `-m` appears as `{"memo": "..."}`; intercompany links as `{"correlate": <id>}`). Databases created before schema v9 are migrated to this form automatically on open.
6. **Status messages go to stderr**: Use `--quiet` to suppress them; only JSON goes to stdout

## Optional identity fields (0.14.0)

With `--agent-token` / `BK_AGENT_TOKEN`, envelopes add `meta.agent` containing
the verified `agent://` URI. It is absent without a token, and unverified input
is never echoed into metadata. Transaction show/list data and audit rows add
nullable `agent_uri` and `token_jti`. Named `posted_by`/`actor` retain their
meaning. Audit rows also include `hash_version`. Versions 2 through 7 include
nullable identity in the hash payload. New rows use version 7, which also covers
the nullable client hint and receipt links. See [audit integrity](../../audit-integrity.md).

`key.init`, `agent.issue`, `agent.list`, `agent.revoke` use the normal envelope.
See [the command reference](commands.md#agent-identity) for their data fields.
Tokens and generated private seeds appear only in issuance output; list and
audit data contain public claims. Successful export output remains standalone.
Capability enforcement defaults off. `policy show` and `policy set --enforce
on|off` return `data: {"enforce": <boolean>, "updated_at": "<UTC timestamp>"}`.
With enforcement on, missing credentials are `UNAUTHORIZED`; insufficient grants
are `FORBIDDEN`, naming the required capability in the message. Before enabling,
issue an admin token; subsequent policy changes require `ledger/admin` and are
audited as `policy_change`. See the [capability map](commands.md#capability-policy).
Agent write signatures and proof of private-key possession remain out of scope.

## Period comparison answers

Supported report kinds accept `compare` and `explain_change` via MCP, or
`--compare` and `--explain-change` via CLI. See the
[comparison contract](../../comparisons.md) for dates, currency rules and signs.

Comparison `data.accounts[]` entries carry `code`, `name`, `type`, `current`,
`prior`, `change`, `change_percent` and amount `_display` companions.
`data.totals` maps named totals to the same amount/percent fields. `current` and
`prior` at the report level describe the effective windows. Existing income
and trial comparison fields retain their names.

`movers[]` repeats changed account lines, adding one-based `rank` and
`direction`, ordered by absolute change then code. `summary` is a plain-language
answer. Optional `explanation` contains the selected `account`, reconciled
`change`, ranked `transactions[]` (transaction_id, date, description, reference,
payee, amount and signed change) and `payees[]` (payee and signed change).
`desktop_view` supplies current/prior filtered register states and relative
Desktop URLs, without requiring a Desktop server to run the report.
