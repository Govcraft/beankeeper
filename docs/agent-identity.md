# Optional agent identity

Beankeeper 0.14.0 can record a verified `agent://` identity alongside the named
actor on ledger writes. This is Phase 1 of agent identity: tokens are optional,
and capabilities are recorded without restricting commands. Reads need no token.
An explicitly supplied token must verify, including on reads. Invalid, expired,
foreign, or revoked tokens return `UNAUTHORIZED` and exit code 6 before the
requested operation runs. Error messages never print the token.

The ledger verifies the root's PASETO v4.public signature, issuer/URI binding,
agent public key claim, validity interval, local registration and revocation.
Clock leeway is zero because issuance and verification use the local clock.
This verifies attested claims, not possession of the agent's private key. Treat
the token as a sensitive credential in this phase. Writes have no agent
signatures, and the caller can omit the token and use the existing named actor.
Capability enforcement, proof of key possession, signed writes, root rotation,
key revocation and hosted discovery are outside Phase 1.

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
registry. Accounting exports do not include them. There is no recovery flow;
keep the ledger and passphrase backed up.

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
capability paths under `ledger`. They do not control access yet.

`--ttl` accepts a positive integer followed by `s`, `m`, `h` or `d`, up to `30d`;
the default is `8h`. Issue output includes `agent_uri`, `token_jti`, `agent_key`
(the public key), `capabilities`, `issued_at`, `expires_at` and `token`.
`--generate-key` also prints `private_key` once, a 64-character hex Ed25519 seed.
The agent private key and complete token are never stored in the registry or
audit snapshots. Store that seed securely if future signing is needed.

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
named actor with the additional verified URI and token ID. Read-only tool
registration remains available with or without a token.

## Migration and integrity

Schema v13 automatically extends transactions and the audit trail. Legacy rows
keep null identities and their original v1 hashes and creation snapshots.
New audit rows use `beankeeper.audit.v2`, appending `agent_uri` and `token_jti`
to the v1 tuple before hashing, including nulls for named-only writes. No history
or anchor is invented. `verify` supports both formats in one chain and checks
transaction identity against the versioned creation snapshot. Protective
triggers cover the new transaction columns. Back up before upgrading; older
binaries cannot append the v2 audit format. See [audit integrity](audit-integrity.md).
