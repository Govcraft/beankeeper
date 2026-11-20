# Changelog

User-visible changes to the Beankeeper CLI (`bk`) are recorded here, grouped by
Added, Changed, and Fixed. CLI versions follow Semantic Versioning. GitHub binary
releases use `beankeeper-cli-vX.Y.Z`; library releases have independent versions.

## [Unreleased]

## [0.21.0] - Unreleased

This catch-up section covers changes since the published CLI 0.10.0 baseline
through CLI 0.21.0. The intermediate source versions are grouped here rather than
presented as separate GitHub releases. This section does not mean that a 0.21.0
tag or binary release has been published. See the [release procedure](docs/RELEASE.md).

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
