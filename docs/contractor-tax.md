# 2025 US contractor payment worksheet

Use this worksheet to prepare ordinary USD payments for Form 1099-NEC Box 1.
It applies the IRS's 2025 $600 aggregate threshold per recipient. Direct
payments for business services can qualify. Payment card and qualifying
third-party network payments belong to the processor's reporting workflow.
Corporate payments are usually outside Box 1, with an exception for legal
services. Missing W-9 details, ambiguous payment methods, unsupported
classifications and backup withholding go to preparer review. Treat uncertain
processor status as unknown. Supported rule versions are fixed to the stated
calendar year. [2025 IRS instructions for Forms 1099-MISC and 1099-NEC](https://www.irs.gov/pub/irs-prior/i1099mec--2025.pdf)

## Review actual payments

1. Give each vendor a structured counterparty ID. Two vendors can share the
   same display name.
2. For a bill, apply each posted payment through the document allocation
   workflow. Accrued bills and their credits do not establish cash payments.
3. Review each eligible source using the posting ID, party ID, exact USD
   cents, method and purpose. A direct withdrawal must match the amount
   posted on a cash or cash-equivalent account. A payment card or qualifying
   third-party processor review can use its liability account. A refund
   must identify the original reviewed payment. A ledger-linked reversal
   appears automatically. For cross-year refunds, follow your preparer's
   instructions and set `confirm_cross_year` only when the adjustment
   belongs in the actual refund year.
4. Run the worksheet and resolve rows before sharing it. An included amount
   is qualifying evidence. The threshold flag shows whether the vendor's
   included total reaches the ordinary reporting threshold.

```sh
bk --company acme contractor-tax review --payment '{
  "posting_id":42,
  "party":"<party TypeID>",
  "amount_minor":40000,
  "source":{"kind":"bill_settlement"},
  "method":"direct",
  "service_use":"contractor_services",
  "backup_withholding":false,
  "reason":"source_reviewed"
}'
bk --company acme --json contractor-tax worksheet --year 2025
bk --company acme --json contractor-tax preview --year 2025
```

To revise a review, pass its current `--expected-receipt` and a reason
such as `corrected_payment_method`. The audit records the actor, previous
and new classifications and associated source posting. The worksheet
uses the posted payment date rather than the bill date or the date the
review was entered. Preview only identifies annotated legacy candidates.
The existing Schedule C and 1099 estimates retain their existing contracts.

## Verify identity and export

Check the original W-9 against the vendor's structured record. Choose the
federal tax classification reported there. Supply a private JSON file with
`legal_name` and `taxpayer_id`. Use an encrypted ledger and a live signed
`ledger/tax-details` grant. The restricted export is a new local JSON file
with the worksheet and verified identity fields, with mode 0600 on Unix.
Ordinary worksheet, review and audit results omit the legal name and TIN
supplied to this workflow. Standalone backups and restore include the
restricted data under the ledger's SQLCipher key. Existing annotations
are independent and should contain no TINs.

```sh
bk --company acme contractor-tax w9 '<party TypeID>' \
  --classification individual --input private-w9.json
bk --company acme contractor-tax export --year 2025 --output preparer.json
```

The MCP tools `contractor_tax_review`, `contractor_tax_worksheet` and
`contractor_tax_preview` use the same redacted workflow. For
`contractor_tax_w9` and `contractor_tax_export`, the host sets
`BEANKEEPER_CONTRACTOR_FILES` to an absolute private directory. Under
it, the operator creates a directory for the registered ledger (use
`startup` for the primary ledger), then a directory for the company slug.
Give the directories and inputs user-only permissions. Pass just a
plain `.json` file name, such as `private-w9.json` or `preparer.json`.
An artifact remains on the host and the tool reports only its file
name or a redacted profile.

This is a preparation worksheet. Government submission and separately
reported payments are outside its scope. Treat the exported identity
file as sensitive and share it through the preparer's secure channel.
