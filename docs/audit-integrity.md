# Audit integrity

Schema v12 adds storage protections and tamper detection. Opening an older
ledger migrates it automatically in one atomic savepoint. Back up your ledger
before upgrading; older binaries cannot write the new audit format.

SQLite triggers reject deletion or replacement of transactions and entries.
They reject changes to every existing transaction column except `metadata`,
and every existing entry column except `status`. Audit rows cannot be changed,
deleted, replaced, or appended without linking to the preceding row. These
checks also cover SQL `INSERT OR REPLACE` with recursive triggers disabled.
Metadata and clearing changes through the supported APIs remain audited.
The triggers do not authenticate callers or require an audit record for raw SQL
writes to the allowed mutable fields.

All shared `post_transaction` callers, including CLI post, CSV/JSON/OFX imports,
demo data, and reversal posting, store the resolved actor in `posted_by` and
append a `create` record. Explicit `--actor` takes precedence over `BK_ACTOR`,
then `USER`/`USERNAME`, then `cli`. The creation record contains a snapshot of
the immutable transaction fields and entries ordered by entry ID. Posting,
correlation, creation auditing, and any outer reversal or import batch commit
or roll back together. Skipped retries and dry runs add no persisted records.

`bk txn show` displays the poster; JSON show/list output includes `posted_by`.
`bk audit log --action create` shows creation records. JSON audit rows add
`prev_hash` and `row_hash`. Existing filters and descending ID order still apply.

## Hash format

The genesis predecessor is 64 ASCII zeroes. Both hashes are lowercase
hexadecimal SHA-256 digests. For each row in ascending audit ID order:

```text
row_hash = SHA-256(UTF-8(prev_hash) || UTF-8(canonical_row))
```

`canonical_row` is a compact JSON array serialized by `serde_json` in this order:

```text
["beankeeper.audit.v1", id, changed_at, actor, company_slug,
 entity, entity_id, action, before, after]
```

Nulls remain JSON nulls. `before` and `after` are the exact stored strings,
including whitespace, escaped as JSON strings. IDs and timestamps participate
in the digest. The version tag and JSON boundaries distinguish null, empty
strings, separators, and Unicode values without ambiguous concatenation.

The migration preserves all existing audit IDs, timestamps, actors, and snapshot
strings, then backfills the chain before installing triggers. Historical ID gaps
are preserved. Rebuilding the audit table establishes its current maximum ID as
the AUTOINCREMENT baseline. Legacy transactions keep `posted_by = NULL`;
no historical creator or missing creation event is invented. Migration cannot
establish whether data was altered before the upgrade.

## Verification and limits

`bk verify` reads one database snapshot and checks:

- Every audit predecessor and recomputed digest, reporting the first broken row.
- The final audit ID against SQLite's AUTOINCREMENT high-water mark to detect
  removal of the tail, including an emptied chain. Appending through the normal
  path after tail removal is rejected.
- Every transaction has at least two entries, positive amounts, valid directions,
  matching company ownership, a supported currency, and equal debits and credits.
  Totals use 128-bit integers. Currency is stored once on the parent transaction,
  so all its entries share that currency.
- Newly audited postings match their creation snapshots and poster.
- The current policy, period locks, proposal resolutions, spend limits and
  statement batches match their audit records. Transaction relationship links
  resolve to the expected records.
- SQLite foreign keys and database integrity.

`bk verify` prints the evidence in text. `bk --json verify` and the MCP
`verify` tool share the same [structured payload](beankeeper/references/json-api.md#verify).
It includes the schema version, row counts, each check's name and outcome,
and debit and credit totals for every company, grouped by transaction currency.
The audit report contains the head hash, length, and first and last IDs.
The head and IDs are null when the chain is empty.

Verification stops at the first failing check and names the relevant row,
transaction or company when available. Failures use the `DATABASE` error
envelope and CLI exit code 4.

This is local tamper evidence, without a signing key or external checkpoint.
An administrator who can remove triggers can also recompute a replacement chain,
rewrite its high-water mark, or restore a complete older database. Verification
cannot distinguish those histories. For pre-v12 transactions it can check the
accounting invariants, but has no creation snapshot for detecting a balanced
rewrite made after bypassing the triggers. Raw SQL metadata or clearance edits
are allowed by storage protections and cannot be attributed by the chain.

## Schema v13 agent attribution

Schema v13 preserves every existing v1 hash and snapshot, with null
`agent_uri`/`token_jti` and `hash_version = 1`. Rows written by schema v13 through v17 use `hash_version = 2`.
Their canonical tuple is the v1 tuple with tag `beankeeper.audit.v2`, followed
by `agent_uri` and `token_jti` in that order, including JSON nulls for named-only
writes. The predecessor/hash construction stays the same, and v2 rows extend
the v1 tail. Migration checks the old chain and tail atomically and adds no
invented identity or anchor. New transaction identity columns are immutable.

V2 creation snapshots include both identity fields. `verify` compares them to
the transaction and audit row; v1 snapshots keep their original shape. Company
and account creation, attachments and key/token management now append audit
records too. Root private keys, complete tokens and generated agent private keys
are excluded from snapshots. Token revocation/expiry leaves old hashes valid.
The token signature attests claims; Phase 1 has no signatures over ledger writes
or proof of agent private-key possession. See [agent identity](agent-identity.md).

## Posted annotation edits

Schema v15 adds `metadata_set` and `metadata_unset` audit actions. Its atomic
audit-table rebuild verifies the chain before and after migration and preserves
all historical hashes, IDs, timestamps, identity fields, indexes, and protections.
`txn meta set/unset` changes only transaction metadata, with one whole-object
before/after record per changed batch in the same savepoint. No-op requests do
not append history. The four built-in relationship keys (`correlate`, `reverses`,
`reversed_by`, `applies_to`) cannot be edited by these commands. Financial data
and creation snapshots are unchanged; the new rows extend the existing chain.

## Scheduled obligations

Schema v16 adds company-scoped schedules and template entries, plus audit entity
`schedule` and actions `schedule_cancel` and `schedule_run`. The migration verifies
the existing hash chain before and after rebuilding its constraints, preserving
historical hashes, IDs, identity fields, indexes, and triggers.

Template creation records the complete validated template. Cancellation and
posting progress record before/after snapshots in the same savepoint as their
changes. The runner obtains SQLite's writer lock before reading progress and
posts each due occurrence through the ordinary transaction creation path. A failed
posting or audit append rolls back the entire run, including progress. Repeating
a successful run creates no new postings or audit rows; previews write neither.
Pending and cancelled templates do not participate in posted ledger balances.


## Schema v18 capability policy

Schema v18 preserves historical v1/v2 audit rows and snapshots verbatim and adds
`policy` events with action `policy_change`. New appends require `hash_version =
3`, with the same ordered tuple as v2 except the domain tag is
`beankeeper.audit.v3`. This writer barrier prevents older binaries from appending
without enforcing capability grants. `verify` supports mixed v1/v2/v3 chains;
v2/v3 creation snapshots retain both identity fields.

The singleton policy defaults off without an invented audit event. Policy
changes and their before/after snapshots are atomic. Storage triggers prohibit
deletion/replacement and require a matching audit record for updates. Policy
reads compare the current state with its latest snapshot and fail closed if it
is missing or inconsistent. `verify` performs that check too.

Back up the full database before upgrading. Migration verifies the old chain
and tail before and after the atomic rebuild, preserves indexes and protections,
and rolls back on failure. After schema v18, older binaries cannot append,
including when enforcement is off. Tokens remain bearer credentials; this does
not add signatures over writes or defend against arbitrary database/key access.


## Schema v20 period cutoffs

Schema v20 preserves all existing v1/v2/v3 hashes and adds company-scoped `period`
audit events (`period_close`, `period_reopen`). New appends require `hash_version
= 4` and the `beankeeper.audit.v4` domain tag, retaining the v3 ordered fields.
The writer barrier prevents v19 and older binaries from posting without checking
cutoffs, even with capability enforcement off.

Close and reopen pair the current cutoff state with before/after snapshots in
one savepoint. Storage triggers require those snapshots for insert/update and
prohibit deletion/replacement. Reads compare persisted state with its latest
audit snapshot; verification also finds histories whose state row was removed.
A database insert trigger additionally rejects transactions in closed dates.
Existing transactions remain intact. Reopen requires a nonempty reason and
records the actor, verified identity if present, timestamp, and previous cutoff.

Migration verifies the historical hash chain and tail before and after rebuilding
the audit table atomically. It restores policy/proposal triggers and rolls back
all schema changes on failure. Back up before upgrading to v20.

## Schema v25 client hints

The CLI and stdio MCP can record a `client_hint` when a write runs with no
verified identity and known runtime markers are present. Detection uses only
environment variable names, including names whose values are empty. The first
matching group wins in this order:

| Runtime | Marker names |
| --- | --- |
| Claude Code | `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT` |
| Codex | `CODEX_THREAD_ID`, `CODEX_CI`, `CODEX_SANDBOX` |
| Gemini CLI | `GEMINI_CLI` |
| Cursor Agent | `CURSOR_AGENT` |
| Generic | `AI_AGENT` |

The hint includes the matched names from that group and leaves `actor` and
`posted_by` independent. It carries no identity or authorization. The CLI
and stdio server can print an attribution notice to stderr after a successful
write command. `--quiet` suppresses the notice.

New appends use the `beankeeper.audit.v5` domain tag and add one final field to
the v4 tuple. That field is `null` or the ordered JSON array
`[false, "env", runtime, marker_names]`. This representation fixes the field
order independently of JSON object serialization. Migration checks the chain
before and after the atomic rebuild, retaining v1 through v4 rows and hashes.
`bk verify` checks mixed chains and includes v5 hints in their integrity check.
