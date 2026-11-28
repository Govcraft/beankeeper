# Stripe collection and payout reconciliation

Configure one Stripe account and mode for the Beankeeper host. Set
`BK_STRIPE_COMPANY`, `BK_STRIPE_ACCOUNT`, `BK_STRIPE_MODE` (`sandbox` or
`live`), and `BK_STRIPE_CONNECT` (`true` for the configured connected account).
The restricted Stripe key, fixed HTTPS redirect, and integration identifier
are host settings `BK_STRIPE_RESTRICTED_KEY`, `BK_STRIPE_RETURN_URL`, and
`BK_STRIPE_INTEGRATION`. Start in sandbox mode.

A trusted webhook receiver puts the original bytes and Stripe signature in
`NAME.json` and `NAME.sig` in the private `BK_STRIPE_SPOOL` directory. Deliver
within the webhook signature's five-minute window and authenticate a fresh
provider retry after that time. Keep `BK_STRIPE_SIGNING_SECRET` and the
spool private. Tool callers give only the delivery name.

| CLI command | MCP tool | What it does |
| --- | --- | --- |
| `collection create-link` | `collection_create_link` | Reserve part of an issued invoice and request a hosted URL. Repeat the exact request after a timeout. |
| `collection apply NAME` | `collection_apply` | Verify one raw delivery and post a matching paid checkout. Retain payout and refund evidence for reconciliation. |
| `collection resolve-receipt NAME --booked-on DATE` | `collection_resolve_receipt` | Post the same verified paid delivery on an open date after a period lock blocked it. |
| `collection show ID` | `collection_show` | Show the current checkout state and its known exceptions. |
| `collection exceptions` | `collection_exceptions` | Review the most recent collection mismatches and blocked receipts. |
| `collection reconcile-refund --terms JSON` | `collection_reconcile_refund` | Reopen the invoice by the verified refund amount on the reviewed open accounting date. |
| `collection disputes` | `collection_disputes` | Read signed dispute balance movements and their original receipt when present. |
| `collection refunds` | `collection_refunds` | Read verified refunds and their journal status. |
| `collection reconcile-payout --manifest JSON` | `collection_reconcile_payout` | Apply the authenticated net deposit and fees when the selected receipts and refunds tie exactly. |
| `collection payouts` | `collection_payouts` | Read pending and reconciled verified payouts. |

For link creation supply `--invoice`, `--amount-minor`, `--request-key`,
`--clearing-account`, and `--as-of`. MCP accepts the same fields in `terms`.
An active request reserves invoice capacity. A successful browser redirect
leaves the invoice open until the verified paid event posts. Retry uncertain
checkout creation with the same key and terms within 20 hours. After that,
review the provider outcome before attempting a new checkout. Local posting
and the network call use separate transactions.

When a paid delivery was stored for review solely because its original
period was closed, ask Stripe to resend the same event with a fresh signature.
Run `collection resolve-receipt NAME --booked-on YYYY-MM-DD` with an open
date on or after the original event date. MCP accepts `delivery` and
`booked_on`. The original exception remains visible with its resolution
posting, and retrying the same date returns that posting. Account, mode,
session, amount, currency and outstanding balance are validated again.
Other exception reasons require manual accounting review.

For a partial refund, use the verified refund ID and a `booked_on` date on or
after the receipt and refund. The debit to receivables and credit to processor
clearing reopen the invoice by the same amount. A period lock prevents posting
and preserves the refund evidence for later review. A reopened invoice can
receive another payment, or a separate credit note if the price changed.

Dispute movements retain the reported principal and link to the receipt when
available. They leave the allocation unchanged pending review of the full
processor balance report, which can include other adjustments and fees.

Use the Stripe payout balance report to select the payment and refund IDs.
The manifest contains `payout`, `payments`, `refunds`, `fees_minor`,
`bank_account`, and `fee_account`. Gross less refunds and fees must equal
the authenticated net payout. After posting, link the imported bank deposit
to that journal using `bank-feed link` or `bank_feed_link`; posting the feed
item separately would record the deposit twice.

Writes require ledger posting authority. The status and reporting tools
appear on the read-only surface. Inspect provider IDs, timestamps, accounting
amounts, and journal references through the tools. Raw payloads and provider
credentials stay in the trusted host.

## Reconcile data read by an agent

Connect Stripe's official MCP to the agent and have it read the correct Stripe
account. On Stripe, review and approve any requested write individually. The
reconciliation below only needs Stripe reads. It works without a Beankeeper
webhook receiver or Stripe credentials on the Beankeeper host.

Use `collection_report_evidence` or `bk collection report-evidence --evidence
JSON` in the selected company. Each report uses `facts` with `account`, `live`,
`date`, `currency`, and a positive `amount_minor`. Normalize timestamps to the
UTC calendar date. Use Stripe's payout arrival date for a payout. Supply the
actual dispute balance movement date and direction for a dispute. Example:

```json
{
  "kind": "payment",
  "facts": {
    "account": "acct_example", "live": true, "date": "2026-11-15",
    "currency": "USD", "amount_minor": 15000
  },
  "payment": "pi_example",
  "target": {
    "type": "invoice", "invoice": "<Beankeeper invoice ID>",
    "clearing_account": "1150"
  }
}
```

For a managed checkout, give `target` as `{"type":"checkout","collection":"…",
"session":"cs_…"}`. For a paid payout, use `kind: payout`, the `payout` ID,
its net amount, and `fees_minor` from the payout's balance transactions.
For a succeeded refund use `kind: refund`, the `refund` and `payment` IDs,
and the refunded amount. For a dispute use `kind: dispute`, the `dispute`
and `payment` IDs, and `movement` of `withdrawn` or `reinstated`.
Dispute reports leave the invoice allocation unchanged.

Report the payments first, then record and reconcile any refunds. Report the
payout and call `collection_reconcile_payout` with its exact payment and refund
IDs, fees, and ledger accounts. A payment can settle in one payout, and refunds
cannot exceed the original receipt. With approval-only access, submitting a
report or reconciliation returns a pending request for independent approval.
Owners can review it with `txn_proposals` and approve or reject it with the
usual proposal tools or Desktop review.

Match the payout's bank entry to the actual feed item with `bank_feed_link`
or mark that exact debit from the imported bank statement and complete the
reconciliation. Use the existing payout journal when recording the bank match
to avoid a duplicate deposit. An agent-reported payout remains in
`collection_exceptions` until the bank evidence covers the entire debit.

The status tools expose `source`, `original_source`, and `signed_conflict`.
A matching signed delivery can confirm an agent report. A conflicting signed
delivery creates a review exception without changing the posted journal.
Resolve conflicting evidence before further reconciliation. Read the journal's
initial source in its evidence audit record. `verify` checks Stripe evidence
provenance against the audit chain.

[Stripe's official MCP server](https://docs.stripe.com/mcp)
