# VAT on GBP invoices and bills

Beankeeper supports a configured UK invoice VAT profile for books posting in
GBP. Choose the applicable code and VAT treatment for each line. Beankeeper
calculates VAT on the rounded quantity times unit price, then rounds each
line's VAT to the nearest penny with ties rounded up. For a price that
includes VAT, it extracts the VAT share and leaves the gross price intact.
This profile uses a line rounding method described in HMRC's
[VAT Notice 700, section 17.5.1](https://www.gov.uk/guidance/vat-guide-notice-700#calculation-based-on-lines-of-goods-or-services).

Configure your own rates, effective dates and liability or recoverable-asset
accounts. These settings apply to invoice-based bookkeeping. The profile uses
the document posting date as the tax date and supports taxable, zero-rated and
exempt lines. Special schemes, automatic eligibility decisions and filing are
outside this profile.

## Configure a code

```sh
bk --company acme --json tax configure MAIN \
  --effective-from 2026-01-01 --treatment taxable --rate 20 \
  --output-account 2200 --input-account 1300
bk --company acme --json tax list
```

Use the returned `code` TypeID in draft lines. A later effective revision under
the same name retains that code ID. Repeating the exact configuration returns
the same revision. Zero-rated and exempt codes take a rate of zero and omit
both control accounts. For taxable purchases where the tax forms part of the
cost, configure a code without a recoverable input account. That tax stays in
the original asset or expense account.

## Issue, credit and review

Add this object to a line in `document draft --terms`, alongside its description,
quantity, unit price in minor units and account.

```json
{"tax":{"code":"<code TypeID>","price_mode":"exclusive"}}
```

Use `inclusive` when the supplied line price contains VAT. A line without a tax
object uses the existing document calculation. Issue an invoice or bill as
usual. The issued document freezes applied VAT rates and net, tax and gross
amounts. Its total and open balance use gross amounts.

Give a line credit's `amount_minor` as the gross amount. Beankeeper allocates
part of the original tax to each partial credit. Repeated partial credits sum
to the original tax when the line is fully credited, even if the configured
rate has changed. A proposal whose VAT changes before approval needs a fresh
review and cannot silently post the changed amount.

```sh
bk --company acme --json tax report --from 2026-01-01 --to 2026-03-31
bk --company acme --format csv tax report --from 2026-01-01 --to 2026-03-31
```

The JSON register shows sales, purchases, recoverable VAT, original revision
and journal IDs, and each control account's expected and posted movement.
Review differences from manual entries before using those totals elsewhere.
The CSV shows signed document-line evidence and unmatched control-account
postings. Monetary fields are integer minor units. Existing tag-based tax
summaries and tax-oriented exports use their original formats.

In MCP, use `tax_configure` with `company`, `code` and `terms` for owner setup.
`tax_list` returns rate revisions. `tax_report` takes `company`, `from`, `to` and
an optional `currency` of GBP. Configuration uses the admin grant. The list
and report use the read grant when ledger enforcement applies.
