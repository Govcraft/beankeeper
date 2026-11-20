# Tax-oriented book exports

**Not tax advice. These are summaries of your books, not filed IRS forms or
e-file submissions.** No deduction limits, eligibility tests, payment-method
exclusions, or tax adjustments are applied. Have your accountant review the
categories, unmatched activity, and vendor annotations before using the exports.

Both reports use existing entry tags and transaction metadata. No schema
migration is needed. They complement `report tax-summary`, which continues to
group tags verbatim.

## Dates, currency, and output

```sh
bk --company llc report schedule-c --from 2026-01-01 --to 2026-12-31
bk --company llc report schedule-c --to 2026-12-31 --output-format csv > schedule-c.csv
bk --company llc report 1099 --to 2026-12-31 --threshold 600 --json
bk --company llc report 1099 --to 2026-12-31 --threshold 0 --format csv > vendors.csv
```

Dates are inclusive. `--to` defaults to today; `--from` defaults to January 1 of
the effective end date's year. Invalid or reversed dates fail. Explicit future
end dates are allowed. Currency follows `tax-summary`: infer the company's sole
transaction currency, use USD for empty books, or require `--currency CODE` if
the company has transactions in multiple currencies, even outside the period.
There is no currency conversion.

Use global `--format table|json|csv` or `--json`, or the report-local
`--output-format table|json|csv`. The local option takes precedence. JSON uses the
standard `ok`/`meta`/`data` envelope, with command IDs `report.schedule-c` and
`report.1099`. Both `data` payloads include `from`, `to`, and `currency`.
JSON monetary fields are integer minor units with additive decimal `_display`
strings. CSV monetary fields are integer minor units, with currency and period
columns. Tables display decimal major units. Empty reports have empty JSON arrays
and CSV headers; no activity is invented.

## Schedule C convention

Use **`sched-c:<line>`**, for example `sched-c:18`. Matching is ASCII
case-insensitive (`SCHED-C:24B` also matches); whitespace and other prefixes are
not normalized. Tags remain unchanged in storage. Explicit `--tax` values take
precedence over account defaults at posting time:

```sh
bk --company llc account create 5000 "Office expense" --type expense \
  --default-tax-category sched-c:18
bk --company llc txn post -d "Office supplies" --date 2026-06-01 \
  --debit 5000:45.50 --credit 1000:45.50
```

The following static mapping uses the input lines on the
[2026 IRS Schedule C](https://www.irs.gov/pub/irs-pdf/f1040sc.pdf).
It does not calculate form subtotals or infer categories from account names.
Confirm the line meanings for the filing year. In particular, **24a is travel,
24b is meals, 27a is the energy efficient commercial buildings deduction, and
27b is other expenses** on this form. Existing tags retain those literal IDs;
the report does not reinterpret earlier-year numbering.

| Line | Book category |
| --- | --- |
| 1 | Gross receipts or sales |
| 2 | Returns and allowances |
| 4 | Cost of goods sold |
| 6 | Other income |
| 8 | Advertising |
| 9 | Car and truck expenses |
| 10 | Commissions and fees |
| 11 | Contract labor |
| 12 | Depletion |
| 13 | Depreciation and section 179 expense deduction |
| 14 | Employee benefit programs |
| 15 | Insurance other than health |
| 16a | Mortgage interest |
| 16b | Vehicle loan interest |
| 16c | Other interest |
| 17 | Legal and professional services |
| 18 | Office expense |
| 19 | Pension and profit-sharing plans |
| 20a | Rent or lease: vehicles, machinery, equipment |
| 20b | Rent or lease: other business property |
| 21 | Repairs and maintenance |
| 22 | Supplies |
| 23 | Taxes and licenses |
| 24a | Travel |
| 24b | Deductible meals |
| 25 | Utilities |
| 26 | Wages |
| 27a | Energy efficient commercial buildings deduction |
| 27b | Other expenses |

The `lines` array has one row per supported line with activity, sorted in form
order, with `line`, `title`, canonical `tax_category`, `debit_total`,
`credit_total`, and `net`. Net is **credits minus debits for income lines 1 and
6** and **debits minus credits for all other supported lines**, including income
reductions 2 and 4. Refunds reduce the respective net; rows remain present when
net is zero. Amounts are book totals, including full tagged meal amounts without
applying a deductibility percentage.

Every tagged entry is included, regardless of account type. Unsupported tags,
including `misc-research` or `sched-c:999`, appear in `unmapped`, grouped by
original category and account. Untagged revenue/expense entries appear in
`untagged`, grouped by account. Untagged balance-sheet entries are excluded to
avoid presenting the other leg of every payment as uncategorized income or
expense. Unmatched rows include account code, name (`title`), type, both totals,
and net on the account's normal side (credit-positive for revenue, liabilities,
and equity; debit-positive for assets and expenses).

CSV is a single rectangular table with a `section` column (`lines`, `unmapped`,
`untagged`), carrying every JSON activity row, including unmatched sections.
`account_code` and `account_type` are empty/null for mapped lines; `line` is
empty/null for unmatched rows. Tables label all three sections even when empty.

## 1099-style vendor convention

Annotate only transactions you want counted as vendor payments:

```sh
bk --company llc txn post -d "Contractor payment" --date 2026-06-01 \
  --debit 5000:700 --credit 1000:700 \
  --meta 'payee=Jordan Key' --meta 'tin=001-02-0003' \
  --meta 'payee_address=12 Main St'
bk --company llc txn list --filter-metadata 'payee=Jordan Key' --json
```

- `payee`: required, nonempty string. Grouping uses the exact string, including
  case and surrounding spaces; whitespace-only strings are omitted. Missing,
  null, and non-string payees are omitted.
- `tin`: optional nonempty string, preserved exactly, including leading zeros.
  There is no tax-ID format validation. Empty/whitespace strings or null are
  treated as absent; other JSON types fail validation.
- `payee_address`: optional string with the same empty/type rules as `tin`.

Each eligible posted transaction contributes its **total debit amount** in the
selected period/currency. Split debit legs add to the total but count as one
payment. The report does not infer cash movement, exclude credit-card payments,
net refunds, or detect accruals. A payee annotation on a refund, accrual, or
non-payment transaction counts its debits positively too; annotate and review
accordingly. Missing payees do not affect Schedule C or `tax-summary` inclusion.

The `vendors` array contains `payee`, nullable `tin` and `payee_address`,
`payment_count`, `total`, and `meets_threshold`, sorted by exact payee string.
Missing optional details can be filled by another payment to the same payee.
Conflicting nonempty TINs or addresses for a payee fail the whole report with a
validation error rather than silently choosing a value. Correct annotations with
`txn meta set` before exporting. Error messages identify the payee and field,
without printing conflicting TIN/address values.

`--threshold` is a nonnegative decimal in major units of the report currency,
with no more than that currency's supported decimal places. The default is
**600**, a configurable bookkeeping convention, not a determination of the
applicable legal reporting threshold for a filing year. Only vendors whose total
is **greater than or equal to** the threshold are returned, so returned rows
have `meets_threshold: true`. Use `--threshold 0` to list everyone. JSON includes
the effective integer `threshold` and `threshold_display`; CSV repeats the
threshold on each row. `1099-nec` is an alias for `1099`.

## MCP

The existing read-only-capable `report` tool accepts:

```json
{"company":"llc","kind":"schedule-c","to":"2026-12-31"}
{"company":"llc","kind":"1099","to":"2026-12-31","threshold":"600.00"}
```

Both accept optional `from`, `to`, and `currency`. Vendor `threshold` is an
optional decimal **string**; `1099-nec` is also accepted. MCP returns the same
`data` payload as CLI JSON, including decimal display fields. Exports contain
any annotated TINs and addresses, so choose their destination accordingly.
