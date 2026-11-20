# Changelog

User-visible changes to the Beankeeper CLI (`bk`) are recorded here, grouped by
Added, Changed, and Fixed. CLI versions follow Semantic Versioning. GitHub binary
releases use `beankeeper-cli-vX.Y.Z`; library releases have independent versions.

## [Unreleased]

## [0.26.0] - Unreleased

### Added

- Per-agent, per-company/currency spend limits through `agent limit set/show/clear`,
  enforced inside the shared posting savepoint. Total debits measure each posting;
  transaction-date daily buckets include approved proposals charged to the proposer.
- Pending posting review with `txn propose/proposals/approve/reject`, the `withdraw`
  alias, and matching MCP tools. Sibling `ledger/propose` and `ledger/approve` grants
  control ownership and review; approval posts atomically with approver attribution,
  a reserved proposal link, and the complete proposer request in the audit trail.
  ([#73](https://github.com/Govcraft/beankeeper/issues/73))

### Changed

- Schema v19 preserves policy and historical audit hashes while adding proposals,
  reference reservations, spend buckets and audited resolutions/limit changes.
- Importers preserve authorization error codes on spend denial. Posting dates are
  canonicalized so alternate date spellings cannot split the same day's bucket.

## [0.25.0] - Unreleased

### Added

- Authenticated MCP Streamable HTTP at `/mcp` with `bk mcp serve --http`,
  defaulting to `127.0.0.1:8750`. `--bind` selects an address; non-loopback binds
  require `--allow-remote` and serve plaintext HTTP without TLS.
- Every HTTP request verifies an `Authorization: Bearer` agent token, even when
  ledger enforcement is off. Process credentials are a fallback for absent
  headers. Discovery and tool authorization use the current requesting token and
  policy, with live expiry/revocation checks and the existing read-only ceiling.
  Stdio remains the default. No schema migration is added.
  ([#74](https://github.com/Govcraft/beankeeper/issues/74))

## [0.24.0] - Unreleased

### Added

- Per-ledger capability enforcement through `bk policy show` and
  `bk policy set --enforce on|off`, defaulting to off. Once enabled, CLI and MCP
  require valid tokens and explicit read/post/reverse/reconcile/budget/admin
  grants. Missing credentials return `UNAUTHORIZED` (exit 6); insufficient
  grants return `FORBIDDEN` (exit 7). Policy changes are audited.
  ([#72](https://github.com/Govcraft/beankeeper/issues/72))

### Changed

- MCP discovery reflects startup grants, with live policy and capability checks
  on every call. Bare `ledger` grants are rejected. Forced initialization cannot
  overwrite an enforced or unreadable ledger. Backup and restore require
  `ledger/admin` when source enforcement is on; restore also requires destination
  enforcement to be disabled before replacement.
- Schema v18 preserves historical audit hashes and adds default-off policy
  storage. New v3 audit records block older binaries from appending, including
  when enforcement is off. Back up the full database before upgrading.

### Fixed

- MCP `--read-only` now hides `txn_reconcile` along with all other write tools.

## [0.23.0] - 2026-11-20

Released as [beankeeper-cli-v0.23.0](https://github.com/Govcraft/beankeeper/releases/tag/beankeeper-cli-v0.23.0),
including the changes tracked under 0.21.0 and 0.22.0 below.

### Added

- `bk report schedule-c` groups canonical `sched-c:<line>` entry tags into
  Schedule C lines and keeps unmapped tags and untagged income/expense activity
  visible for review.
- `bk report 1099` (alias `1099-nec`) summarizes vendor payments from transaction
  `payee` metadata, preserves optional TIN/address details, and supports a
  configurable minimum total. Each annotated transaction contributes its full
  debit total, counted once even with split debit entries.
- Both tax reports support table, JSON, and CSV output, share `tax-summary`
  date/currency rules, and expose matching JSON payloads through the MCP `report`
  tool. These are bookkeeping summaries, not filed tax forms; no deduction limits
  or payment-method exclusions are applied. See the
  [tax export conventions](docs/tax-exports.md).
  ([#80](https://github.com/Govcraft/beankeeper/pull/80))

## [0.22.0] - 2026-11-20

Shipped as part of CLI 0.23.0; no separate 0.22.0 GitHub release was cut.

### Added

- `bk backup` creates a consistent snapshot of the entire ledger, including
  committed WAL data, and verifies it before publishing. `bk restore` verifies
  a snapshot before replacing an explicitly selected database. Both commands
  support encrypted ledgers with the original passphrase and require `--force`
  to replace an existing destination.
  ([#79](https://github.com/Govcraft/beankeeper/pull/79))
- Snapshots include attachment records, but not the receipt or invoice files
  stored outside the database. Back up those files separately and preserve the
  paths referenced by attachment records when restoring on another machine.

## [0.21.0] - 2026-11-20

This catch-up section covers changes since the published CLI 0.10.0 baseline
through CLI 0.21.0. The intermediate source versions are grouped here rather than
presented as separate GitHub releases. These changes shipped as part of CLI
0.23.0; no separate 0.21.0 GitHub release was cut. See the
[release procedure](docs/RELEASE.md).

### Added

- Local MCP server over stdio, with ledger tools for AI assistants and optional
  read-only mode: `bk mcp serve`.
  ([#58](https://github.com/Govcraft/beankeeper/pull/58))
- Hash-chained audit records, immutable posting protections, recorded posting
  actors, and expanded `bk verify` checks for the audit chain, posting snapshots,
  and accounting invariants. The chain provides tamper evidence against stored
  state, without an external trust anchor.
  ([#59](https://github.com/Govcraft/beankeeper/pull/59))
- Audited, linked transaction reversals with `bk txn reverse`.
  ([#52](https://github.com/Govcraft/beankeeper/pull/52))
- Optional verified `agent://` identity, local token issuance and revocation,
  and attribution in CLI/MCP output and audit records. Phase 1 records
  capabilities without enforcing them; tokens remain optional.
  ([#63](https://github.com/Govcraft/beankeeper/pull/63))
- CSV and JSON transaction imports alongside OFX, with validation, dry runs,
  references for safe retries, and atomic posting.
  ([#54](https://github.com/Govcraft/beankeeper/pull/54))
- Persistent, company-scoped bank-import categorization rules and rule testing.
  ([#64](https://github.com/Govcraft/beankeeper/pull/64))
- Audited transaction metadata annotations through `bk txn meta`, including
  filtering while protecting relationship keys.
  ([#65](https://github.com/Govcraft/beankeeper/pull/65))
- Recurring transaction templates and explicit schedule runs, with dry runs,
  due-date handling, and duplicate prevention.
  ([#67](https://github.com/Govcraft/beankeeper/pull/67))
- Loan amortization schedules and balanced, audited principal/interest payment
  splits through `bk loan schedule` and `bk loan pay`.
  ([#68](https://github.com/Govcraft/beankeeper/pull/68))
- Historical spending and net-worth trends by month, quarter, or year.
  ([#66](https://github.com/Govcraft/beankeeper/pull/66))
- AR/AP settlement reconciliation against charge applications, including
  statement selection and configurable metadata links.
  ([#61](https://github.com/Govcraft/beankeeper/pull/61))
- Investment securities, prices, fractional-share lots, FIFO/LIFO/specific lot
  sales, realized gains, and historical holdings valuation, with CLI/MCP support.
  ([#70](https://github.com/Govcraft/beankeeper/pull/70))

### Changed

- Schedule runs require `--allow-large-run` when more than 1,000 occurrences are
  due; dry runs still allow reviewing the full batch before posting.
  ([#71](https://github.com/Govcraft/beankeeper/pull/71))
- JSON monetary fields retain integer minor units and add decimal-string display
  companions. Transaction and audit lists expose pagination totals and whether
  more rows exist; table output signals truncation on stderr.
  ([#62](https://github.com/Govcraft/beankeeper/pull/62))
- Building from source requires Rust 1.91 or newer; stable Linux/macOS quality
  checks and a separate minimum-version check run in CI.
  ([#48](https://github.com/Govcraft/beankeeper/pull/48),
  [#51](https://github.com/Govcraft/beankeeper/pull/51),
  [#63](https://github.com/Govcraft/beankeeper/pull/63))

### Fixed

- Response-format flags work after subcommands; commands reject unknown
  companies and missing databases before operating; deletion validates dependent
  ledger data; initialization honors configured encryption passphrase sources.
  Report date defaults and currency-unit amount messages are consistent, and
  list counts go to stderr so stdout remains usable by scripts.
  ([#48](https://github.com/Govcraft/beankeeper/pull/48))
- Release archives include README and both license files; the installer checks
  the installed executable's reported version.
  ([#48](https://github.com/Govcraft/beankeeper/pull/48),
  [#43](https://github.com/Govcraft/beankeeper/issues/43))
