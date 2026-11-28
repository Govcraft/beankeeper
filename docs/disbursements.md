# Reviewed vendor disbursements

A disbursement reserves an amount against one or more issued bills for the same vendor, payable account, and functional currency. Partial payments are supported. Classify the funding account as cash before preparing an instruction. Amounts are positive integer minor units.

| Signed grant | Operations |
|---|---|
| `ledger/disbursement-draft` | Draft an instruction with exact bill allocations. |
| `ledger/disbursement-approve` | Preview the provider destination, independently approve the terms, revoke approval, or reject the instruction. |
| `ledger/disbursement-dispatch` | Submit a reviewed instruction once, query provider status, and reconcile confirmed events. Use a direct credential. |
| `ledger/read` | Show and list instructions and retained payment evidence. |

Each command uses `bk --company acme disbursement`. MCP exposes the same commands through the `disbursement_*` tools. In CLI examples below, change the agent credential between the drafter, reviewer, and dispatcher.

```sh
bk --company acme disbursement draft --terms '{"cash_account":"1000","bills":[{"document_id":"BILL_ID","amount_minor":4200}]}'
bk --company acme disbursement preview PAYMENT_ID
bk --company acme disbursement approve PAYMENT_ID --destination REVIEWED_SHA256
bk --company acme disbursement dispatch PAYMENT_ID
bk --company acme disbursement reconcile PAYMENT_ID --date 2026-03-31
```

Review the instruction and beneficiary against the provider's records. Approval reserves the exact unallocated balance and the reviewed beneficiary fingerprint. Changed destination details require the reviewer to revoke approval and review the new fingerprint before dispatch. Use `reject --reason` to decline an instruction before dispatch.

A successful dispatch may report an accepted transfer or a provider-confirmed outcome. Repeat `dispatch` to query the same payment key if the reply was lost, if the process restarted, or while the transfer is pending. After an earlier attempt, Beankeeper queries the payment key and leaves an unconfirmed payment unresolved if the host cannot find it. Restored payment instructions are also lookup-only. Check the provider's records before creating a replacement instruction.

Reconcile after the provider confirms settlement. Choose an open accounting date on or after the confirmed provider date. Beankeeper debits accounts payable, credits cash, and applies the exact bill allocations in one transaction. A verified return credits accounts payable, debits cash, and reopens the original allocations. Both provider events stay visible even when a closed period delays the journal. Repeating reconciliation returns the records already posted.

## Host adapter

The trusted host runs a single domestic payment provider bridge on a numeric loopback address. Set `BK_DISBURSEMENT_HOST` to the bridge base URL and `BK_DISBURSEMENT_HOST_TOKEN` to a dedicated host authentication token in the environment of the CLI or MCP server. The adapter supplies bearer authentication. Provider credentials and banking details belong in the bridge. Beankeeper sends vendor and payment identifiers, a destination fingerprint, currency, and amount.

- `GET /destinations/VENDOR_ID` returns `{"fingerprint":"SHA256"}`. The fingerprint must change whenever the provider account or bank destination changes.
- `POST /payments` takes `key`, `vendor`, `company`, `cash_account`, `destination`, `currency`, and `amount_minor`. The `Idempotency-Key` header carries the same key. The bridge must compare the expected destination fingerprint with the current destination when it accepts a new payment, use that exact destination, select the funding source identified by the company and cash account, and retain a permanent mapping from the key to the provider order. The key must return the same order if presented again.
- `GET /payments/KEY` returns the authenticated status for the key, or HTTP 404 when the bridge has no evidence for it. The same response format applies to submission. Return `key`, `order_id`, `status` (`accepted`, `settled`, `failed`, or `returned`), `on` (YYYY-MM-DD), `company`, `cash_account`, `amount_minor`, `currency`, and `destination`. Include `settled_on` in a returned event so a client that missed settlement can recover both events. Use `failed` only when the provider confirms the transfer did not settle.

The repository includes a [Wise bridge](wise-bridge.md) for domestic USD vendor payments.
