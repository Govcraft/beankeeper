# Financial summary

Use one read-only call when the owner asks, "How am I doing?"

```sh
bk --company personal summary --json
bk --company personal summary --to 2026-03-18 --currency USD --json
bk --company personal summary --from 2026-01-01 --to 2026-03-31 \
  --compare-from 2025-01-01 --compare-to 2025-03-31 --json
```

Any MCP agent can call `financial_summary` with the same snake_case options:

```json
{"company":"personal","to":"2026-03-18","currency":"USD"}
```

The tool returns the **full CLI envelope**, rather than only its `data` payload:
`{ok, meta: {command: "summary", company, timestamp, currency}, data}`.
Failures set MCP `isError: true` and return `{ok: false, meta, error: {code,
message}}`, using the CLI's named error codes. A verified caller's URI is
included in successful response metadata. Timestamps reflect each call.
The tool remains available in read-only mode and requires `ledger/read` when
capability enforcement is enabled. Neither interface posts, clears, reconciles,
runs a schedule, nor appends an audit event. All calculations share one SQLite
read snapshot.

Start an answer with the short factual `highlights`, using the explicit dates
in `periods`. Explain material `needs_attention` facts and their limitations.
Let the owner ask for details. An agent supplies its own advisor voice and can
draw a chart directly from the returned numbers. Offer the optional Desktop
target only if a visual would help; an app is never required for the answer.
Do not describe an age-based balance as overdue or scheduled posting volume as
bills to pay.

## Dates and comparison

By default the current period starts on the first day of the effective end
month and ends today, using local dates. `--to` replaces today. Its comparison
is the **full previous calendar month**, so a partial current month is not a
like-for-like elapsed-day comparison. Dates are explicit in every response.
When `--from` is supplied, the comparison instead uses the immediately preceding
interval of the same number of inclusive days. Supply both `--compare-from`
and `--compare-to` to override that comparison. Reversed or invalid ranges fail.
Standing balances and attention items are evaluated through the current end
date, including balances before the start date and excluding future postings.
Schedule status is current stored state, not a historical reconstruction of
which schedules were pending at a past date.

## Data contract

Amounts are signed integer **minor units**, with currency-aware decimal-string
`_display` companions. The response includes `currency` and `minor_units`.
Currency is inferred from the company's transactions, USD for an empty book;
a company with several transaction currencies must supply `--currency`.
There is no currency conversion or cross-currency sum.

| Section | Chartable facts and stable identifiers |
| --- | --- |
| `standing` | `cash_on_hand`, `owed_to_you`, `you_owe`, `net_worth`; selected `cash_accounts`, `receivable_accounts`, `payable_accounts` contain company-scoped account codes |
| `comparison.series` | Stable metric `id` (`income`, `spending`, `left_over`), `label`, `current`, `prior`, `change`, `change_percent` |
| `biggest_movers.series` | Up to five expense accounts ordered by absolute change, ties by account code; `id` is the account code, with the same comparison fields |
| `needs_attention.items` | Stable item keys/IDs, `available`, `count`, `total`, `basis`/`reason`; recurring items add `from`, `to`, `schedule_count`; unreconciled items add `entry_count` |
| `highlights` | Brief, repeatable facts with human-readable amounts; no accounting entry terminology or account codes |
| `desktop_view` | Stable `view: "financial-summary"`, company, currency, current/comparison dates, selected account lists and a navigation `uri`; also included on each section |

Changes are current minus prior. Percentages are that change divided by the
**absolute prior amount**, multiplied by 100 and rounded to one decimal place.
A zero prior amount returns `null`, including zero-to-zero. Negative baselines
are supported without reversing the sign of an improvement. Monetary
calculations check for overflow and fail instead of wrapping or saturating.

Income means net revenue posted during the period; spending means net expense
activity, including refunds. Left over is income minus spending. These are
recorded accounting activity, not cash receipts or a cash-flow forecast.
Net worth is total assets minus total liabilities at ledger book values, not
market valuation of investments. Loans, transfers and owner funding do not
become income merely because cash arrives.

Cash defaults to asset names containing `cash`, `bank`, `checking` or `savings`,
matching the cash-flow report's convention. Receivables default to asset names
containing `receivable`, and payables to liability names containing `payable`.
Matching is case-insensitive and never guesses from account codes. Repeat
`--cash-account`, `--receivable-account` or `--payable-account` to replace the
corresponding inferred set. MCP accepts arrays named `cash_account`,
`receivable_account`, `payable_account`. Selectors validate account existence
and type and deduplicate codes. Unmatched sets produce zero and empty account
lists, with `cash_accounts_identified` / `receivable_accounts_identified` false.
Highlights qualify selected-account totals and flag missing sets; inspect the lists or set selectors when names do not follow this
convention. `you_owe` includes **all liabilities**, including loans, regardless
of the payable selection.

## Attention limits

- `uncategorized` is a **review proxy**: transactions with nonzero net activity
  on accounts named Uncategorized or Suspense, including historical corrections.
  The total adds absolute activity on those accounts. The ledger has no
  unresolved categorization flag, so this is not a guaranteed unresolved count.
- `unreconciled_accounts` counts selected cash accounts with entry amounts not
  reconciled through the as-of date. The total is gross remaining entry volume,
  counting incoming and outgoing amounts without cancelling them. It includes
  cleared amounts awaiting reconciliation and handles partial reconciliations
  and later reconciliation dates. It does not assert bank-statement agreement.
- `recurring_due` counts every still-unposted occurrence in the next 14 days,
  starting tomorrow and including day 14. Cancelled/completed schedules are
  excluded. The total adds one side of each balanced scheduled template,
  including income and noncash postings: **transaction volume**, not cash due.
- `overdue_receivables` and `overdue_payables` have `available: false`, with
  `count` and `total` both `null`, because there is no invoice/due-date model.
- `aged_receivables` and `aged_payables` separately count open charges more than
  30 days old by posting date, after specific `applies_to` links and FIFO
  matching within each account. These totals do not establish payment deadlines.

## Optional Desktop targets and output formats

`desktop_view` supplies a portable navigation target for a Desktop host. Its URI
contract is `beankeeper://summary?company=SLUG&currency=CODE&from=DATE&to=DATE&compare_from=DATE&compare_to=DATE`.
Query values are percent-encoded. Repeated `cash_account`, `receivable_account`
and `payable_account` query parameters preserve the selected account lists; the
same lists accompany the target object.
Desktop owns registering and handling this target; this CLI change does not
install a URI handler or build a screen. Agents can pass the complete target to
a host navigation tool without opening Desktop to answer the question.

Plain output (`bk summary`) starts with the facts, then attention items and an
optional Desktop target. `--format csv` produces rows for standing, comparison,
movers and attention with section/metric identifiers, currency, integer amounts,
changes, percentages, counts and availability. Unknown values are empty CSV
cells. Use JSON to retain every basis, period, display value and view filter.
