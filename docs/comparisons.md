# Period comparisons and what changed

The CLI and MCP `report` tool compute comparisons without changing the ledger.
Answer in words first using `summary`, offer charts from `accounts`, `totals`
and `movers`, and offer Desktop detail only when helpful.

## Questions and commands

```sh
# Why is spending up this month?
bk --json --company acme report trend --metric spending --from 2026-10-01 --to 2026-10-31 --compare previous

# How does this quarter compare to last year?
bk --json --company acme report income-statement --from 2026-07-01 --to 2026-09-30 --compare year-ago

# What changed in rent? Account codes identify categories without name ambiguity.
bk --json --company acme report trend --metric spending --from 2026-10-01 --to 2026-10-31 --compare previous --explain-change 5000

# Compare cash flow with an explicit inclusive window.
bk --json --company acme report cash-flow --from 2026-10-01 --to 2026-10-31 --compare 2026-07-01..2026-07-31 --cash-account 1000

# Compare cash balances at the ends of October and September.
bk --json --company acme report balance --account 1000 --to 2026-10-31 --compare previous --explain-change 1000
```

Equivalent MCP request, with no shell required:

```json
{"name":"report","arguments":{"company":"acme","kind":"trend","metric":"spending","from":"2026-10-01","to":"2026-10-31","compare":"previous","explain_change":"5000"}}
```

Supported kinds: `income-statement`, `trial-balance`, `trend` (both metrics),
`cash-flow`, `balance` and `balance-sheet`. CLI `data` equals the MCP structured
payload, including display amounts, summaries, movers and explanations. Normal
ledger read authorization applies, including the MCP read-only server.

## Dates and amounts

- All windows are inclusive. `previous` (`prior-period`) shifts whole calendar
  months as calendar months, so a quarter compares with the previous quarter.
  Other windows compare with the same number of days immediately before them.
- `year-ago` (`prior-year`) shifts both boundaries back a year. February 29
  clamps to February 28. An explicit `YYYY-MM-DD..YYYY-MM-DD` comparison accepts
  arbitrary windows, including overlaps. Reversed/invalid dates fail.
- Income, trial, spending and cash flow compare activity in each window.
  Without dates these use year-to-date, consistent with existing reports.
  Trend comparison aggregates the entire range; `--period` affects ordinary
  trends, not comparison buckets.
- Balance without `--from`, balance sheet, and net-worth trends compare
  cumulative balances through each endpoint. For balance and balance sheet
  without a start, `previous` uses the current month's start to select the
  prior cutoff. For net-worth trends the ordinary year-to-date window selects
  that cutoff. Balance with `--from` compares activity instead.
- Cash flow retains the existing direct-method attribution. Amounts on
  non-cash counter-accounts are positive inflows or negative outflows; non-cash
  transactions and cash transfers with zero net movement are excluded.
- Other accounts use their normal side: debit for assets/expenses and credit
  for liabilities/equity/revenue. Refunds and contra balances retain signs.
- Every line has `current`, `prior`, `change` and `change_percent`.
  `change = current - prior`; percentage is `change / abs(prior) * 100`, rounded to two decimal places.
  A zero prior amount gives `null` (table: `n/a`), including zero-to-zero.
  Amount integers are minor units with decimal-string `_display` companions.
  Overflow fails with a named error rather than returning a clamped answer.
- A company with multiple transaction currencies requires `--currency` (MCP:
  `currency`). Only that currency is read in both windows and explanations.
  No conversion or cross-currency comparison is performed.

## Movers and explanations

`movers` contains all nonzero account changes, ordered by absolute change,
then account code for ties. Each contains its one-based `rank` and `direction`
(`increase` or `decrease`). Totals are separate, so they are not double-counted
in rankings. `summary` leads with a relevant report total when available, then describes the
three largest account changes, or says
that no account amounts changed. Numeric increases are not judgments that an
owner's finances improved.

`--explain-change CODE` must name an account line in this comparison. The
`explanation.transactions` list ranks signed contributions by absolute value,
then transaction ID. A current-only transaction contributes its signed amount;
a prior-only one contributes the negative of that amount. Transactions included
in both windows cancel. For cumulative balances only changes between endpoints
remain. Multiple entries on the selected account in a transaction are combined.
The contribution sum must reconcile exactly with the account change.

`explanation.payees` groups those same contributions using `metadata.payee`,
then `metadata.counterparty`, with `(unspecified)` when neither is recorded.
Payees are ranked by absolute net contribution, with name as the tie breaker.
Descriptions are evidence of recorded activity, not an inferred payee or proof
of why the owner spent money. Results include every contribution rather than
silently truncating the explanation.

## Optional Desktop details and exports

`desktop_view` carries `view_id: "transactions"`, current `state`,
`comparison_state`, and URL-encoded `url`/`comparison_url`. These are relative
links to Desktop's existing `/transactions` register, with company, currency,
account and date filters. They select the explained account, otherwise the
requested balance account or largest mover; with no changed account, the
register covers the company. Cumulative links omit the lower date bound.

Resolve links against the owner's configured Desktop origin before presenting
them outside that host. The open ledger does not know a Desktop hostname,
require Desktop, or navigate it automatically. The two states can also be used
with Desktop's `ui_show_view`; they avoid inventing a comparison-view route.

JSON retains existing income/trial comparison fields. Other comparison results
use the same `accounts` and `totals` pattern. Table output starts with the plain
summary. Comparison CSV uses `row_type,code,name,current,prior,change,change_percent`
with account/total/mover/transaction/payee rows and summary/Desktop-link rows; empty percentages mean null.
Transaction rows use `code` for the transaction ID and `name` for description.
Normal non-comparison output is unchanged.
