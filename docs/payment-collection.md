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
