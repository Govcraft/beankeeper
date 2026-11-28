# Perpetual inventory

Beankeeper tracks stock for one location in each company's functional currency. Each SKU has an inventory asset account, a cost of goods sold (COGS) expense account, a goods received not invoiced (GRNI) liability account, and a count adjustment expense account. Add these accounts before registering an item.

```json
{
  "sku": "PANEL-1",
  "name": "Sample panel",
  "unit": "piece",
  "currency": "USD",
  "stock_account": "1300",
  "cogs_account": "5000",
  "receiving_clearing": "2100",
  "adjustment_account": "5100"
}
```

Save this document as `item.json` and run `bk --company acme inventory register --terms item.json`. Use the resulting item ID in movement requests. Quantities are positive decimal strings with at most six fractional digits. Costs are integers in the currency's minor units. For a new receipt, use the vendor's counterparty ID.

```json
{
  "item": "<registered item ID>",
  "quantity": "2.5",
  "date": "2026-03-18",
  "reference": "delivery-0318-panel",
  "movement": {
    "kind": "receipt",
    "vendor": "<vendor counterparty ID>",
    "total_minor": 1050
  }
}
```

Save this document as `receipt.json` and run `bk --company acme inventory move --request receipt.json`. Inventory movements and their journals commit together. Resubmit the identical request to retrieve the original result. A reference already used for a different request is rejected. Dates before an item's most recent movement or within a closed period are rejected.

| Movement kind | Fields besides `kind` | Accounting |
|---|---|---|
| `receipt` | `vendor`, `total_minor` | Debit inventory and credit GRNI at receipt cost. |
| `issue` | Optional `customer` | Debit COGS and credit inventory using the oldest available lots. Supply a customer to match an invoice. |
| `customer_return` | `shipment` | Debit inventory and credit COGS at the original shipment's carrying cost. |
| `supplier_return` | `receipt` | Debit GRNI and credit inventory using the source receipt lot's remaining cost. |
| `count_in` | `reason`, `total_minor` | Debit inventory and credit the adjustment account at the recorded cost. |
| `count_out` | `reason` | Debit the adjustment account and credit inventory at FIFO cost. |

Returned or counted quantities can be fractional. A partial lot consumes a rounded share of the remaining cost, and the final units consume exactly the residual. A zero-cost movement has an audit receipt and changes units without posting a journal.

## Receipt and shipment matching

Enter a vendor bill with its stock receipt line charged to GRNI. Issue it to debit GRNI and credit accounts payable. Run the following command to match the posted bill line to the physical receipt. Line numbers start at 1. Match the same vendor, GRNI account, remaining receipt quantity, and cost.

```sh
bk --company acme inventory match-document RECEIPT_ID --document BILL_ID --line 1 --date 2026-03-21
```

Post a physical shipment to record inventory relief and COGS on the shipment date. Issue the sales invoice on its own date and use the same matching command with the shipment and invoice IDs. A match records their relationship without posting a second journal. A supplier return after a bill has been issued also requires the appropriate vendor credit in accounts payable.

## Reporting

```sh
bk --company acme inventory list
bk --company acme inventory movements ITEM_ID --as-of 2026-03-31
bk --company acme inventory valuation --as-of 2026-03-31
```

Valuation shows quantities and remaining FIFO cost per item, the corresponding general ledger control account balances and differences, and receipts awaiting a bill match. Use `--json` for structured output or `--format csv` for the valuation table. `inventory show` and `inventory movement` show a single item or movement. To correct a physical count, record an adjustment with the reason in an open period.

The same operations are discoverable through MCP. `inventory_register`, `inventory_move`, and `inventory_match_document` require a direct posting grant. Read-only sessions can list items, retrieve movements, and run valuations.
