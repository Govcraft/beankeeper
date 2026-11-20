# Investment lots

Beankeeper tracks share quantities separately from monetary ledger entries. Each
security has a company-scoped, case-sensitive symbol, a name, and one immutable
currency. Different companies can register the same symbol independently.

## Record a purchase, sale, and price

Use separate asset accounts for investment cost and cash, and a revenue account
for realized gains. Losses debit the same revenue account. The accounts must
exist in the selected company and must be distinct.

```sh
bk --company acme account create 1200 Investments --type asset
bk --company acme account create 1010 'Brokerage cash' --type asset
bk --company acme account create 4100 'Realized investment gains' --type revenue
bk --company acme security create ABC --name 'ABC shares' --currency USD
bk --company acme security list --json

bk --company acme investment buy ABC --quantity 1.5 --cost 150.00 \
  --date 2026-01-02 --asset 1200 --cash 1010 --reference broker-buy-001
bk --company acme investment lots ABC --json
bk --company acme investment sell ABC --quantity 0.5 --proceeds 60.00 \
  --date 2026-02-02 --asset 1200 --cash 1010 --gain 4100 \
  --reference broker-sell-001
bk --company acme security price ABC --date 2026-02-02 --price 120.00
bk --company acme investment holdings --as-of 2026-02-02 --json
```

This leaves one share with $100 cost basis, $120 market value, and $20 unrealized
gain. The sale posts $60 cash debit, $50 investment credit, and $10 gain credit.
A purchase posts investment debit and cash credit for its total cost. All entries
are balance-validated before posting. Lot changes, postings, and audit records
commit together or roll back together.

## Select lots and allocate basis

Sales use FIFO by acquisition date, then lot ID. `--method lifo` reverses that
order. Selection stays within the specified company, symbol, and investment
asset account. `--method specific --lot LOT_ID` selects exactly one lot, using
the ID returned by `investment buy` or `investment lots`. Split a sale into
separate trades to select multiple specific lots. Overselling is rejected.

Quantities are positive decimal strings with up to eight fractional places,
including fractional shares. Floating point is never used. Money inputs use
major currency units with the currency's native precision: `12.34` for USD,
`12` for JPY, `12.345` for BHD. Prices are minor units per whole share, so
sub-minor-unit price precision is outside this phase. Costs include purchase
fees; proceeds are net of sale fees. There is no separate fee calculation.

For partial sales, basis is the sold proportion of the lot's remaining basis,
rounded to the nearest minor unit, with halves rounded up. The final disposal
takes all remaining basis. This conserves the original cost exactly. Arithmetic
and aggregation overflow produce errors without changing the ledger. A sale
with both zero allocated basis and zero proceeds is rejected because it cannot
produce a monetary transaction. Zero proceeds otherwise records a total loss.

Record trades in nondecreasing date order within each symbol and investment
account. Sales on a given date are applied in the order entered. Lot selection uses
acquisition date and lot ID. Future-dated trades are
permitted; earlier historical holdings exclude them. References are unique per
company across all transactions. Duplicate references reject the whole trade;
there is no skip strategy for investment trades. Inspect the existing transaction
or lots before retrying an uncertain result.

## Value holdings without posting gains

`investment holdings` defaults to today's local date. `--as-of DATE` includes
only lots acquired and sales dated on or before that date. Each row describes
one symbol and investment asset account, with currency, quantity, remaining
cost basis, latest price/date, market value, and unrealized gain or loss.
It uses the latest price dated on or before the report date. With no such price,
valuation fields are null. Exhausted positions are omitted; `investment lots`
retains their original quantities, costs, and remaining zero quantities.

Prices are append-only observations, one per company, symbol, and date. A zero
price is allowed; negative prices and duplicate dates are rejected. Different
currencies stay in separate rows. Reports never post unrealized gains or alter
audit history. Market values round half-up to currency minor units.

All commands support tables, `--json`, `--format csv`, and `--quiet`. JSON keeps
money as integer minor units and adds decimal-string `_display` companions;
quantities are exact decimal strings. Tables and CSV show money in major units.
Trade responses include transaction ID, purchased lot ID when applicable,
quantity, basis, proceeds, realized gain/loss, and per-lot sale relief.

## MCP and audit behavior

MCP tools mirror CLI behavior: `security_create`, `security_list`, `security_price`,
`investment_buy`, `investment_sell`, `investment_lots`, and
`investment_holdings`. Pass `company` explicitly. Trade quantities and amounts
are strings. Buy/sell fields match CLI flags; `method` defaults to `fifo`, and
`lot` is required only for `specific`. `as_of` is optional for holdings.
`mcp serve --read-only` omits security creation, price recording, buys, and sells.

Security, price, lot, and relief creations append hash-chained audit snapshots,
with the named actor or MCP principal. Buy/sell monetary transactions use the
existing audited posting path. Migration 17 preserves existing audit hashes and
posting protection triggers. Investment tables reject updates, deletes, and
replacement inserts. A generic `txn reverse` is rejected for investment trades,
even if their descriptive metadata is changed: a monetary reversal alone would
not restore quantities. Trade corrections need a future lot-aware operation.

Keep the SQLite database as the complete backup. The existing ledger export
contains monetary postings, without the security, lot, and price tables.

Splits, dividends, corporate actions, wash sales, lot-level tax forms, trade
corrections, and price revisions remain follow-up work.
