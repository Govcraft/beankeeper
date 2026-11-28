# Fixed assets

Use the register for straight-line book depreciation of equipment and other
fixed assets. Set up a cost asset account, an accumulated-depreciation contra
asset account, and a depreciation expense account. Dedicated control accounts
make the register-to-journal comparison easier to interpret.

```sh
bk --company acme fixed-asset register --name Lathe --currency USD \
  --acquired-on 2026-01-15 --in-service 2026-01-15 \
  --cost-minor 10500 --salvage-minor 500 --life-months 3 \
  --cost-account 1200 --accumulated-account 1210 --expense-account 5100 \
  --offset-account 1000 --reference lathe-purchase

bk --company acme fixed-asset preview "$ASSET_ID" --through 2026-03-31
bk --company acme fixed-asset depreciate "$ASSET_ID" --through 2026-03-31
bk --company acme fixed-asset show "$ASSET_ID" --as-of 2026-03-31
bk --company acme fixed-asset list --as-of 2026-03-31

bk --company acme fixed-asset dispose "$ASSET_ID" --date 2026-04-16 \
  --proceeds-minor 5000 --proceeds-account 1000 \
  --gain-account 4900 --loss-account 5900
```

Amounts named `*_minor` are in the currency's minor units. For example,
10500 means 105.00 USD. For an existing purchase or opening journal, supply
`--acquisition-transaction ID` instead of the offset and reference. That
transaction must match the asset's acquisition date, currency, exact cost
debit and, if nonzero, the `--opening-depreciation-minor` credit. Each journal
can be registered against one asset. Set the explicit in-service date and
remaining life for imported opening assets.

Depreciation begins with the first whole calendar month. If service begins on
January 15, the first charge is dated the last day of February. The calculation
rounds cumulative depreciation down to the currency's minor unit at each
month-end. The final charge reaches salvage value exactly. Rerunning through
the same date cannot post an extra charge. The entire batch rolls back if any
new charge is dated in a closed period.

Before disposing of an asset, post all due depreciation through the last
month-end strictly before the disposal date. Dispose after the most recent
recorded depreciation movement. Omit both proceeds flags for scrapping. The
journal credits gross cost, debits accumulated depreciation, records any
proceeds and posts the difference between proceeds and book value as gain or
loss. The register shows zero remaining book value and retains the original
terms and movements. The as-of list compares active cost and accumulated
depreciation with the corresponding ledger accounts. A difference can reflect
unregistered purchases or manual adjustments on those accounts.

The MCP equivalents are `fixed_asset_register`, `fixed_asset_show`,
`fixed_asset_list`, `fixed_asset_preview`, `fixed_asset_depreciate`, and
`fixed_asset_dispose`. For the MCP show, preview and depreciation tools, use
`date` for the cutoff. Mutations require direct posting authority. Generic
transaction reversal and recategorization cannot change registered acquisition
and movement journals independently of the register. Opening registration,
depreciation, and disposal use the ledger's audit and backup/restore paths.
The register covers book depreciation with one method and does not calculate
tax depreciation.
