# Bank feeds

A bank feed stages transactions for review. You can match a staged item to an
existing posting or use it to create a posting or proposal. Sync never changes
posted ledger balances.

## Connect a bank or card account

Complete Plaid Link in your host application and obtain the selected account's
Plaid account ID and Item access token. The feed adapter supports Plaid Sandbox
and Production, and maps one external account to one ledger account. The
ledger account must be an active asset or liability in the matching currency.
Native foreign-currency accounts are outside this workflow.

```sh
bk --company acme --json bank-feed connect \
  --account 1000 --provider-account '<Plaid account ID>' \
  --currency USD --environment sandbox
```

Keep the returned feed ID. Supply `BK_PLAID_CLIENT_ID`, `BK_PLAID_SECRET` and
`BK_PLAID_ACCESS_TOKEN` from the host credential store when running
`bank-feed sync <feed ID>`. The sync command receives those values via its
environment. It accepts no credential arguments and omits the secrets from
logs, ledger data, audit history and snapshots. Use credentials for the
provider account configured for that feed. Provider access needs to be
authorized and renewed through your host's Link flow.

A failed page leaves the previous cursor intact. Each sync holds its write
lock only while applying the complete window, and a concurrent worker whose
cursor is stale must retry. An update larger than the bounded window returns an
error. Disconnect stops further sync. Repeating the original connect command
reactivates that exact mapping and resumes from its previous cursor.

## Review and resolve

List an owner's configured feed IDs and stage new revisions before reviewing.

```sh
bk --company acme --json bank-feed connections
bk --company acme --json bank-feed sync "$feed"
bk --company acme --json bank-feed list "$feed"
bk --company acme --json bank-feed review "$feed" "$item"
bk --company acme --json bank-feed link "$feed" "$item" --revision 1 --transaction 42
bk --company acme --json bank-feed post "$feed" "$item" --revision 1 --contra 5000
```

Use `next_after` as `--after` to page through staged transactions. Review shows
posted transactions with a matching bank entry, currency and amount within
three days, plus the first applicable categorization rule. Review every
candidate before confirming a distinct posting with `--confirm-new`. A
unique matching card hold can settle through the existing hold workflow.
An existing ledger match records the relationship without marking the
transaction reconciled. Continue to use statement reconciliation for close.

Pending and removed rows stay in the queue for history. A later provider change
marks a resolved row as requiring correction and preserves the original
posting. A proposal for an outdated revision cannot be approved. Reject it and
review the current provider row before proposing a replacement.

Use `bank_feed_connections` in MCP to discover the selected company's feed IDs.
Call `bank_feed_sync` with the company and feed ID to stage new revisions. A
normal posting or approval-mode connection can stage them. Refresh does not
create a posting or a proposal. `bank_feed_connect` and `bank_feed_disconnect`
continue to require owner administration. Tool arguments and results carry no
API keys, cursor, or access tokens.

Use `bank_feed_show`, `bank_feed_list` and `bank_feed_review` to inspect the
staged transactions. Use `bank_feed_link` for a confirmed existing journal
when the agent can post directly, or `bank_feed_post` to apply or propose the
reviewed revision under the configured write mode.

If credentials are missing, the owner must authorize the feed on the host.
If Plaid reports that it is preparing transactions, retry the refresh later.
If Plaid requests a new login, use Link in update mode, then update the
credential if needed. A failed refresh never changes the cursor or stages
partial data. Owners using Desktop can store the provider authorization through
the local `bk bank-feed authorize` command in the native credential store.

Backups preserve the mapping, cursor and staged rows on the restored accounting
timeline. Credentials stay with the host. After restore, the next authorized
manual or host-scheduled poll resumes from the cursor in the restored data.
Review is still required before any financial change.

[Plaid Transactions Sync API](https://plaid.com/docs/api/products/transactions/)
