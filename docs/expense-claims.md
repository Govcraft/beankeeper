# Receipt intake and expense claims

Stage receipt evidence, confirm the expense, and send it to a different reviewer.
An approved reimbursement records an expense and a liability. A card claim adds
receipt evidence to one existing expense posting.

```sh
bk --company acme --actor submitter expense-claim receipt-intake --file receipt.png
bk --company acme --actor submitter expense-claim create \
  --terms claim.json --reference draft-travel-001
bk --company acme --actor submitter expense-claim submit "$CLAIM_ID" \
  --revision 1 --reference submitted-travel-001
bk --company acme --actor reviewer expense-claim approve "$CLAIM_ID" \
  --revision 2 --reference reviewed-travel-001
```

The draft terms use minor units in the specified currency.

```json
{
  "claimant": "party_TYPE_ID",
  "receipt": "receiptin_TYPE_ID",
  "merchant": "Station Cafe",
  "incurred_on": "2026-02-01",
  "currency": "USD",
  "amount_minor": 1000,
  "splits": [
    { "account": "5100", "amount_minor": 600 },
    { "account": "5200", "amount_minor": 400 }
  ],
  "funding": { "kind": "reimbursable", "payable": "2100" }
}
```

Use a counterparty record in the same company for the claimant. A reimbursement
credits the chosen liability account. Follow actual payment evidence with
`expense-claim record-payment ID --revision N --reference KEY --transaction ID`.
Choose a posted transaction in the claim's currency with one debit to the
approved liability and a matching credit to a funding asset. The claim can
receive multiple partial reimbursements up to its exact approved amount.
Payment matching links the existing journal and does not send money.

For a company card, use
`"funding": { "kind": "company_card", "transaction": 42 }`. The posted
transaction must match the approved debit splits and have one credit to a
company funding account. If a card authorization is still pending, use its
posted settlement as the evidence. Approving the claim attaches the receipt
to that posting.

Every state change requires the current revision and a unique request
reference. Exact retries return the earlier result. Rejection records the reason.
Amend a draft or rejected claim using revised terms, then submit it for another
review. An approved and unpaid reimbursement can be cancelled with
`expense-claim void` if its accounting periods are open and the journal has no
clearings. A cancellation posts a compensating journal and keeps the original
approval and its receipt evidence. Claims with recorded reimbursements keep
their original payment links.

Submission and intake use `ledger/propose`, review uses `ledger/approve`,
recording a payment uses `ledger/reconcile`, and cancellation uses
`ledger/reverse`. Review uses the verified agent identity when available,
otherwise the local actor label. Use separate principals for submission and
review. Read-only sessions can look up claim and receipt details.

## OCR findings and evidence files

Intake accepts PNG, JPEG, or PDF evidence up to 5 MiB. Images have dimension and
pixel bounds. The MCP intake tool receives standard Base64 and the filename as
separate tool arguments. The transport can impose its own message size limit.

The host can run its OCR engine and send optional structured findings with
the upload. For the CLI, pass `--findings OCR.json`, optionally with
`--findings-hash SHA256`. Findings can have `merchant`, `date`, `currency`,
and `total_minor` objects with a `candidate`, an `evidence` excerpt, and optional
`confidence` from 0 to 100. The suggested values stay separate from the
confirmed claim terms and are never used to post a journal.

A repeat upload of the same bytes returns the same intake in that company.
Using the same receipt evidence for an additional claim requires a documented
`--duplicate-reason`. Keep the ledger's adjacent `attachments/` directory with
the ledger backup so restored claims retain their content files.
