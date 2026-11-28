# Wise bridge for vendor disbursements

The `bk-wise-bridge` host executable provides a Wise adapter for the
vendor disbursement commands and MCP tools. It uses Wise’s fixed 2026Q4 HTTPS API for a US business profile, a standard USD balance and domestic ABA recipient
accounts. It supports quotes that show the same amount going out and arriving, with zero fees. Sandbox is the default and uses a separate ledger from live accounting.

Wise has two separate operations. The bridge creates a transfer for the reviewed
recipient with the disbursement's stable Wise customer transaction ID, then
funds that transfer from the reviewed balance. Beankeeper approval is required
before the bridge receives the request. Wise can also require its own approval
or authentication. If Wise does, the bridge leaves the payment unresolved so
the operator can inspect it in Wise.

[Wise payouts for businesses](https://docs.wise.com/guides/product/send-money/use-cases/payouts-smbs),
[standard transfers](https://docs.wise.com/api-reference/standard-transfer),
[transfer funding](https://docs.wise.com/api-reference/transfer/transferfund)

## Configure the host

The signed release archive includes `bk-wise-bridge` from CLI 0.41.0 onward.
Verify and extract the archive for your platform, then run the companion
executable. [Release verification](RELEASE.md#sigstore-verification-contract)

To build from a checkout, run:

```sh
cargo build --release --locked -p beankeeper-cli --bin bk-wise-bridge
target/release/bk-wise-bridge
```

Set these environment variables for the bridge process before launching it.

| Variable | Value |
|---|---|
| `BK_WISE_BRIDGE_CONFIG` | Path to a host-owned JSON configuration file |
| `BK_WISE_BRIDGE_STATE` | Path to a dedicated SQLite state file in a private directory, mode 0700 on Unix |
| `BK_WISE_BRIDGE_TOKEN` | Wise credential authorized for the selected environment, business profile and balance |
| `BK_DISBURSEMENT_HOST_TOKEN` | Dedicated host bearer token shared with the calling Beankeeper process |
| `BK_WISE_BRIDGE_LISTEN` | Numeric loopback socket, default `127.0.0.1:8791` |
| `BK_WISE_BRIDGE_ENVIRONMENT` | Defaults to `sandbox`. Use `live` only for production |
| `BK_WISE_BRIDGE_ENABLE_LIVE` | Set to `yes` alongside an explicit live environment to enable real transfers |

Set `BK_DISBURSEMENT_HOST=http://127.0.0.1:8791` and the matching
`BK_DISBURSEMENT_HOST_TOKEN` on the CLI or MCP host. Wise credentials stay
with the bridge. Provision recipients in the selected Wise environment first. Map one
company and cash account per bridge. The ID in each `recipients` entry is
the immutable Wise recipient account ID. Example with synthetic IDs:

```json
{
  "company": "acme",
  "cash_account": "1000",
  "profile_id": 10001,
  "balance_id": 20002,
  "recipients": {
    "party_01j00000000000000000000000": 30003
  }
}
```

Live mode uses the official production origin and can move money. Configure
a separate ledger, Wise credential, host token and state path for production.
The bridge rejects state files belonging to the other environment and includes
the selected environment in each approval fingerprint. Wise may charge a fee
for an external recipient. The bridge refuses those quotes before creating
a transfer because the approved journal covers the principal exactly. The
bridge records a failed instruction if it cannot verify payment terms before
creating a transfer. It preserves that outcome for lookup and releases the
bill reservation. After resolving the cause, draft a new instruction.

[Wise API environments](https://docs.wise.com/guides/developer/environments)

## Recover and reconcile

Keep the bridge's state file durable and separate from ledger backups. Keep
one running process for a state file. Beankeeper's `disbursement dispatch`
and `disbursement_dispatch` MCP calls look up the previous key after an
uncertain reply. The bridge looks up the stored transfer ID, or searches Wise
for the stable customer transaction ID when the create response was lost.
It never creates or funds a transfer on a lookup. Restore the ledger through
Beankeeper's recovery commands so restored instructions stay lookup-only.

An authentic completed funding response identifies the debit created against
the reviewed Wise balance. An ambiguous funding response stays unresolved even
if the transfer later changes status, because the completed-payment list does
not identify the exact balance debited. Inspect an uncertain payment in Wise
before taking further action. The bridge performs at most one funding attempt
per key. The same rule covers SCA, insufficient balance and interrupted
requests.

The bridge treats Wise's outgoing-payment-sent status as the bookkeeping
milestone. It records the date when it first authenticated that status.
That date is neither the beneficiary's receipt date nor a backdated
provider event timestamp. A refund reopens bills only after the bridge
already observed that the same funded transfer was sent. Use the standard
refund-to-balance configuration for the selected profile. Intermediate,
unknown or previously unobserved refund history stays unresolved.

[Wise transfer statuses](https://docs.wise.com/guides/product/send-money/tracking/transfer-statuses),
[completed funding evidence](https://docs.wise.com/api-reference/transfer/transferpaymentslist),
[refunds to a Wise balance](https://docs.wise.com/guides/product/send-money/refunds/refund-to-balance)
