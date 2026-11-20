# Optional agent identity

Beankeeper records a verified `agent://` identity alongside the named actor on
ledger writes. Since CLI 0.22.0, each ledger can enforce the token's explicit
capability grants. Enforcement defaults to off for new and upgraded ledgers:
tokens remain optional and grants do not restrict commands until enabled.
An explicitly supplied token must always verify, including on reads. Missing
credentials when enforcement is on, or invalid, expired, foreign, or revoked
tokens, return `UNAUTHORIZED` (exit 6). A verified token missing the required
grant returns `FORBIDDEN` (exit 7), naming the required capability. Error messages
never print the token.

The ledger verifies the root's PASETO v4.public signature, issuer/URI binding,
agent public key claim, validity interval, local registration and revocation.
Clock leeway is zero because issuance and verification use the local clock.
This verifies attested claims, not possession of the agent's private key. Treat
the token as a sensitive credential in this phase. Writes have no agent
signatures. While enforcement is off, the caller can omit the token and use
the existing named actor.
Enforcement controls these CLI and MCP interfaces, not arbitrary access to the
database file and its keys. Proof of key possession, signed writes, root rotation,
key revocation and hosted discovery remain outside this feature.

## Setup

Start with an encrypted ledger and initialize its root once:

```sh
bk init --encrypt
bk key init
```

`key init` generates an Ed25519 root and a unique `<id>.bk.local` authority per
database. It requires SQLCipher encryption and refuses to overwrite an existing
key. The private root stays in the encrypted database; output contains only
`kid`, `trust_root` and `public_key`. Full database backups carry the root and
registry. Use the supported snapshot commands with the original passphrase:

```sh
bk --db books.db --passphrase-file /secure/books.key backup --output ledger.bk-backup
bk restore --input ledger.bk-backup --db restored.db --passphrase-file /secure/books.key
```

Backup includes committed WAL writes even while the source is open. Restore
verifies the complete snapshot before publishing it, including the audit chain.
The restored root and registry accept the same unexpired, unrevoked tokens.
A supplied agent token is checked against the snapshot being restored, not an
existing destination. Replacing a destination requires `--force` and all its
connections must be closed. Accounting exports exclude the root and registry.
Keep the passphrase separately: these commands cannot recover a lost key.
External receipt files need a separate backup; see
[backup and restore](../README.md#backup-and-restore).

Normally the agent or its host generates its own Ed25519 keypair and supplies
the standard-base64 public key:

```sh
bk --json agent issue --name claude --public-key "$AGENT_PUBLIC_KEY" \
  --caps ledger/read,ledger/post --ttl 8h
```

For a local setup, request a generated agent keypair explicitly:

```sh
bk --json agent issue --name claude --generate-key --ttl 8h
```

`--name` is a valid agent TypeID prefix (lowercase letters and underscores).
The URI is `agent://<id>.bk.local/ledger/<name>_<typeid-suffix>`. The common
`ledger` path accommodates sibling grants such as `ledger/post`,
`ledger/reverse`, `ledger/reconcile`, `ledger/budget` and `ledger/admin`.
`ledger/read,ledger/post` is the default grant list. All grants must be valid
capability paths strictly below `ledger`. A bare `ledger` grant is rejected at
issuance and verification, including when enforcement is off.

`--ttl` accepts a positive integer followed by `s`, `m`, `h` or `d`, up to `30d`;
the default is `8h`. Issue output includes `agent_uri`, `token_jti`, `agent_key`
(the public key), `capabilities`, `issued_at`, `expires_at` and `token`.
`--generate-key` also prints `private_key` once, a 64-character hex Ed25519 seed.
The agent private key and complete token are never stored in the registry or
audit snapshots. Store that seed securely if future signing is needed.

## Enable capability enforcement

Initialize the encrypted ledger and root, then issue the tokens you will need
before enabling enforcement. Keep an admin token so you can manage the policy
and registry afterward. This token has read and admin access; posting requires
an additional `ledger/post` grant.

```sh
bk --json agent issue --name owner --public-key "$OWNER_PUBLIC_KEY" \
  --caps ledger/read,ledger/admin --ttl 8h
# Save the returned token as OWNER_TOKEN before enabling:
bk policy show
bk policy set --enforce on
bk --agent-token "$OWNER_TOKEN" policy show
bk --agent-token "$OWNER_TOKEN" policy set --enforce off
```

While enforcement is off, a named actor without a token can turn it on. Once
on, every ledger command, including reads and policy changes, requires a valid
token. Policy changes and key/token management require `ledger/admin`. Policy
changes append an atomic `policy_change` audit record with before/after states;
setting the same value again is a no-op. `policy show` returns JSON data with
`enforce` (boolean) and `updated_at` (UTC timestamp).

| Required grant | Commands and corresponding MCP tools |
| --- | --- |
| `ledger/read` | List/show commands, reports, export, verify, audit log, agent list, policy show, loan schedule |
| `ledger/post` | txn post/import/attach, txn meta set/unset, investment buy/sell, security create/price, loan pay, schedule create/run/cancel, rule create/delete |
| `ledger/reverse` | txn reverse |
| `ledger/reconcile` | txn clear, txn reconcile |
| `ledger/budget` | budget set/delete |
| `ledger/admin` | company/account create/delete, key init, agent issue/revoke, policy set |

Matching is hierarchical and respects path segments. The required path is the
leaf shown above, so a narrower `ledger/post/example` grant cannot authorize
`txn post`. `ledger/post` and `ledger/reverse` are siblings: neither grants the
other. `ledger/admin` grants administration only. To authorize all operations,
issue the union `ledger/read,ledger/post,ledger/reverse,ledger/reconcile,ledger/budget,ledger/admin`.
The current schedule API has `cancel`, which requires `ledger/post`; there is
no update command.

New-ledger `init` remains a named-actor bootstrap and rejects any supplied token.
`init --force` cannot overwrite an enforced ledger, including when its path comes
from `--path`. Disable enforcement using an admin token first. Resetting an
existing encrypted ledger requires its current passphrase so its policy can be
checked; an unreadable ledger is never erased. A standalone `loan schedule`
calculation still works without a database; with an existing ledger, it follows
that ledger's read policy.

## Use and inspect

Pass the token globally with `--agent-token`, or set `BK_AGENT_TOKEN` in the
agent's environment. The flag overrides the environment variable.

```sh
bk --agent-token "$BK_AGENT_TOKEN" --company acme txn post \
  -d 'Sale' --debit 1000:10 --credit 4000:10 --reference sale-001
bk --company acme txn show 1 --json
bk audit log --agent "$AGENT_URI" --json
bk agent list --json
bk agent revoke "$TOKEN_JTI"
# Or revoke all tokens currently registered for this URI:
bk agent revoke "$AGENT_URI"
bk verify
```

New transactions (including CSV/JSON/OFX imports and reversals) and their audit
rows carry `agent_uri` and `token_jti`. All CLI mutations to existing ledgers,
including company/account creation, attachments and key/token management, are
audited. `posted_by` and audit `actor` keep their previous named-actor meaning:
`--actor`, then `BK_ACTOR`, then the OS user, then `cli`. Transaction show/list
JSON exposes the two identity fields; table detail and audit output show them
too. JSON envelopes include `meta.agent` only for a verified supplied token.
Successful exports retain their standalone file format.

`agent list` returns public issuance records and `revoked_at`, with no token or
private key. Revocation by URI affects all matching tokens issued so far;
revocation by `token_jti` affects one. Repeated revocation changes nothing and
adds no audit record. An unknown target returns `NOT_FOUND`. Revocation does not
change historical postings, so `verify` continues to pass after expiry or
revocation. It checks the stored audit chain and posting snapshots, not past
tokens' current validity.

Initialization creates a new trust boundary, so `init` rejects a supplied token
before creating or overwriting a ledger. Bootstrap with the named actor and
initialize the new root afterward. `init --demo` honors `--actor`/`BK_ACTOR`.

## MCP sessions

Launch `bk --agent-token TOKEN mcp serve`, or set `BK_AGENT_TOKEN` in the MCP
server environment. There is one token per server process, with no per-tool
credential argument. Startup verifies it before starting the protocol; each
call rechecks expiry and revocations against the database. Revoking a token in
another CLI process takes effect on the next call without restarting MCP.
Errors use the normal MCP `{code, message}` tool error with `isError: true`.
Without a token, the actor remains `mcp:<client>`; with one it remains the same
named actor with the additional verified URI and token ID. When enforcement is on, startup registers only tools covered by the supplied
token. Each call rechecks the current policy and required grant as well as token
validity. A session started while enforcement was off cannot bypass a later
policy change: calls then fail with `UNAUTHORIZED` or `FORBIDDEN` as appropriate.
Tool lists reflect startup permissions; restart the server after policy changes
to refresh discovery. `--read-only` remains a process ceiling and removes every
write tool, including `txn_reconcile`, even with broader token grants.

## Migration and integrity

Schema v13 automatically extends transactions and the audit trail. Legacy rows
keep null identities and their original v1 hashes and creation snapshots.
Schema v18 adds the default-off policy singleton and expands audit events for
`policy_change`. It verifies the existing hash chain before and after migrating
atomically. New audit rows use `beankeeper.audit.v3`, with the same ordered fields
as v2 and a new domain tag. Existing v1/v2 hashes and snapshots are preserved.
Storage triggers require v3 on append so older binaries cannot write around
capability enforcement. `verify` supports all three versions in one chain and
checks transaction identity and current policy against their audit snapshots.
Policy deletion, replacement, and unaudited updates are rejected. Back up the
full database before upgrading: older binaries cannot append after schema v18,
even while enforcement is off. See [audit integrity](audit-integrity.md).
