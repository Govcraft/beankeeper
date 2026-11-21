# Chart data for agents

Ask for `--json --chart` on the CLI, or `chart: true` through MCP. The result
adds a versioned `data.chart` block. Agents can answer in plain words and draw a
chart directly from this block. Opening Desktop remains optional.

```sh
bk --company acme --json --chart report trend --metric spending \
  --from 2026-01-01 --to 2026-03-31
bk --company acme --json --chart summary --to 2026-03-31
bk --company acme --json --chart --chart-all report income-statement \
  --from 2026-01-01 --to 2026-03-31
```

MCP examples:

```json
{"name":"report","arguments":{"company":"acme","kind":"trend","metric":"spending","from":"2026-01-01","to":"2026-03-31","chart":true}}
{"name":"financial_summary","arguments":{"company":"acme","to":"2026-03-31","chart":true,"chart_all":true}}
```

The CLI success envelope remains `ok`, `meta`, `data`. MCP `report` keeps returning
the CLI's `data` payload; MCP `financial_summary` keeps returning the full summary
envelope. Every pre-existing field and monetary integer is retained. Without a
chart request there is no new block. Table and CSV output retain their existing
contracts; `--chart` requires JSON and `--chart-all` requires `--chart`. Unsupported
commands return the named `USAGE` error. MCP exposes both options in tool discovery.

| Output | Chart projection |
| --- | --- |
| `summary` / `financial_summary` | Income, spending and money left over, prior then current period |
| `report trend --metric spending` | Expense accounts across calendar periods, including signed refunds |
| `report trend --metric net-worth` | Total assets, total liabilities and net worth at each period end |
| `report income-statement` | Spending by category for the resolved date range |
| `report cash-flow` | Signed cash movement attributed to non-cash counter-accounts |
| `report income-statement --period/--compare` | Account activity on each account's normal side |
| `report trial-balance --period/--compare` | Account activity on each account's normal side |

Spending categories are expense account codes, not tax-category tags. Net-worth
history is the balance-over-time output: it includes opening history, carries
balances across inactive periods, and uses book values. Period trial balances
measure activity, not cumulative balances. A spending chart excludes the source
report's Total Spending series to avoid counting the categories twice. Summary
leftover is income minus booked spending, not cash flow. Use a cash-flow chart
for money moving into and out of the selected cash accounts.

## Version 1 contract

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer `1`; incompatible schema changes require a new version |
| `kind` | Rendering hint: `line` or `bar` in version 1 |
| `title` | Short factual description |
| `x` | Ordered string labels: calendar periods, explicit date intervals, or category labels |
| `x_accounts` | Exact account codes aligned with a category axis; `null` for period axes and Other |
| `series` | Named series, each with `values`, `unit`, `currency` and optional `account` |
| `series[].values` | Exact signed decimal strings in major currency units, one per x label |
| `series[].unit` | Always `major`; never an implicit minor-unit amount or percentage |
| `series[].currency` | Explicit ISO currency code, shared by the whole chart |
| `series[].account` | Exact source account code for an account series; omitted for aggregates |
| `desktop_view` | `view_id` and resolved `state` for the optional Desktop `ui_get_link` tool |
| `truncated` | Whether categories/series were combined into Other |
| `omitted_count` | Number of source categories/series represented by Other |

Amounts use the same currency scale as existing `*_display` fields: for example,
USD `"12.34"`, JPY `"123"`, KWD `"1.234"`. There are no floating-point money values.
Zero activity is explicit. Negative spending includes refunds; negative net worth
and cash outflows keep their signs. Empty categories yield an empty axis and
values; an empty net-worth history still contains zero balances. Consumers should
show a no-data message for an empty axis or series list.

Charts with more than eight categories/series rank them by the sum of absolute
amounts, retain eight and append Other with the signed sum of the remainder at
each x position. Ties use account code, then label. Net-worth balances are never
summed across time. Series with at most eight entries keep source order. The
source report always retains all accounts. Use `--chart-all` or `chart_all: true`
to preserve all chart categories/series in source order. Category charts apply
the same limit to their x axis. Labels can repeat; use account identifiers to
distinguish them. Other is an aggregate with no account identifier, even when a
real account happens to have that display name.

Desktop targets preserve company slug, currency, inclusive resolved dates,
comparison dates where present, and trend metric/aggregation period. Cash-flow
targets also preserve the selected cash account codes. Pass `chart.desktop_view`
to `ui_get_link` to obtain the local detail URL without navigating. To inspect
one account, use the `transactions` view with its exact `account` code and the
company/currency/date fields. Do not copy trend-only `period` or `metric` filters
to the register. Never put an expense display label in `category`, which means
an exact entry tax tag in Desktop. The ledger CLI does not guess a Desktop
listener URL or require a running Desktop process.

## Worked chart

Given a two-month spending report, the following chart describes one account
series. Draw a line with `x` as the labels and each series' `values` as the y
values; label the y axis USD. The original report still contains raw minor-unit
amounts and its Total Spending series.

```json
{
  "schema_version": 1,
  "kind": "line",
  "title": "Spending by category over time",
  "x": ["2026-01", "2026-02"],
  "x_accounts": [null, null],
  "series": [
    {
      "name": "Dining",
      "account": "5000",
      "values": ["120.50", "-15.00"],
      "unit": "major",
      "currency": "USD"
    }
  ],
  "desktop_view": {
    "view_id": "trend",
    "state": {
      "company": "acme",
      "currency": "USD",
      "from": "2026-01-01",
      "to": "2026-02-28",
      "period": "month",
      "metric": "spending"
    }
  },
  "truncated": false,
  "omitted_count": 0
}
```

The negative February value means refunds exceeded booked expenses by USD 15.00.
A factual answer can say that directly before offering a chart or optional link.
