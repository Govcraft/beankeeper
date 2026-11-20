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
- SQLite foreign keys and database integrity.

Failures use the existing `DATABASE` error envelope and exit code 4. A healthy
ledger retains the existing verify success output.

This is local tamper evidence, without a signing key or external checkpoint.
An administrator who can remove triggers can also recompute a replacement chain,
rewrite its high-water mark, or restore a complete older database. Verification
cannot distinguish those histories. For pre-v12 transactions it can check the
accounting invariants, but has no creation snapshot for detecting a balanced
rewrite made after bypassing the triggers. Raw SQL metadata or clearance edits
are allowed by storage protections and cannot be attributed by the chain.
