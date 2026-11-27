# Changelog

User-visible changes to the Beankeeper CLI (`bk`) are recorded here, grouped by
Added, Changed, and Fixed. CLI versions follow Semantic Versioning. GitHub binary
releases use `beankeeper-cli-vX.Y.Z`; library releases have independent versions.

## [Unreleased]

- Hosts can discover typed MCP operation/tool pairs before setup and filter them with verified effective grants, including bookkeeper Chart and Reverse rules. (#329)

## [0.40.0] - 2026-11-27

Released as [beankeeper-cli-v0.40.0](https://github.com/Govcraft/beankeeper/releases/tag/beankeeper-cli-v0.40.0).
This release includes the changes previously tracked under 0.28.0 through
0.39.0, none of which shipped separately, plus later work collected under
Unreleased. It advances the ledger schema from v20 (in 0.27.0) to v39; each
migration makes older binaries refuse to append to an upgraded ledger. Back
up the full database before upgrading.

### Added

- Library hosts can list the MCP ledger tools for an access level with `LedgerAccess::tools()` without opening a ledger, so desktop clients see the same tools before and after setup. (#323)

- CLI and MCP chart series expose typed semantic roles for assets, liabilities, net worth, and other report quantities without changing exact amounts. (#319)

- Classify cash and cash-equivalent assets through CLI/MCP with audited undo so summary and cash-flow include wallets independently of account names. ([#317](https://github.com/Govcraft/beankeeper/issues/317))

- Embedded hosts can authorize writes per ledger across CLI and MCP, with structured denials that identify the ledger and explain why it is read-only. ([#313](https://github.com/Govcraft/beankeeper/issues/313))

- Embedded MCP hosts can read typed CLI ledger bindings and resume SSE streams when serving a single preopened ledger. ([#310](https://github.com/Govcraft/beankeeper/issues/310))

- Restore and undo bookkeeping snapshots through `bk` and MCP while preserving live identity, revocations, enforcement, and audit history. `bk restore` updates an existing ledger in place. Use `bk recover` to restore a full snapshot to a new file. ([#306](https://github.com/Govcraft/beankeeper/issues/306))

- Embedded Rust hosts can require a verified pre-migration snapshot before upgrading a ledger, and `bk backup` preserves the source schema version. ([#305](https://github.com/Govcraft/beankeeper/issues/305))

- Embedded Rust hosts can convert `&CliError` to `EnvelopeError` and preserve the CLI and MCP error code, message, and structured details. ([#303](https://github.com/Govcraft/beankeeper/issues/303))

- Track native foreign-currency balances and post auditable, undoable revaluations through the CLI and MCP. ([#292](https://github.com/Govcraft/beankeeper/issues/292))

- Post, import, and report in active ISO 4217 currencies with defined decimal minor units through the CLI and MCP. ([#296](https://github.com/Govcraft/beankeeper/issues/296))

- Link related transactions in the same book with a typed relationship through the CLI or MCP and undo it with one audit receipt. ([#291](https://github.com/Govcraft/beankeeper/issues/291))

- Reconcile monthly account statements through CLI and MCP, undo with an audit receipt, and see outstanding months in financial summaries. ([#290](https://github.com/Govcraft/beankeeper/issues/290))

- Move a posted line between accounts through the CLI or MCP and undo the move with one audit receipt. ([#289](https://github.com/Govcraft/beankeeper/issues/289))

- Generate verified synthetic demo books tailored to a typed profile from the CLI or an authenticated MCP host. ([#293](https://github.com/Govcraft/beankeeper/issues/293))

- Settle unique pending card holds during bank imports and flag stale authorizations in financial summaries through the CLI and MCP. ([#281](https://github.com/Govcraft/beankeeper/issues/281))

- Record and settle pending card holds through CLI or MCP, see posted and available balances, and undo settlements in one audited step. ([#280](https://github.com/Govcraft/beankeeper/issues/280))

- Rename account labels from the CLI or MCP with one-step owner undo and sign-off for propose-only bookkeepers. ([#283](https://github.com/Govcraft/beankeeper/issues/283))

- Bookkeepers can set up books and accounts, post opening balances, and import statements on their normal CLI or MCP connection with one-step owner undo. ([#273](https://github.com/Govcraft/beankeeper/issues/273))

- Embedded Rust hosts can read the linked build's expected ledger schema
  version without opening a database.
  ([#250](https://github.com/Govcraft/beankeeper/issues/250))

- Embedded hosts can stage a private replacement credential, then register it
  and revoke the exact prior token in one owner-attributed transaction. Token
  status by ID lets a host recover after interruption.
  ([#256](https://github.com/Govcraft/beankeeper/issues/256))

- Create, list, supersede, remove, and preview company categorization rules from
  either CLI or MCP. Amendments preserve both rule versions in the audit trail.

- Save and amend standing sales-tax, processor-fee, and inform preferences from
  either interface. Schema v27 adds audit hash version 7 and preserves earlier
  hashes. ([#253](https://github.com/Govcraft/beankeeper/issues/253))

- Embedded hosts can page through company audit receipts and undo a posting or
  account creation with a verified owner grant. Posting undo appends a linked
  reversal. Account undo archives the account and retains its history. Schema
  v26 stores receipt links as foreign keys, preserves prior audit hashes, and
  migrates accounts as active. Receipt summaries and undo errors are typed.
  ([#249](https://github.com/Govcraft/beankeeper/issues/249))

- Embedded MCP hosts can register separate books that accept the startup
  ledger's verified bearer and keep its signed grants, posting mode, and audit
  identity. Hosts can detach named ledgers after active requests drain. Named
  results identify their ledger in MCP metadata and CLI JSON.
  ([#252](https://github.com/Govcraft/beankeeper/issues/252))

- Embedded hosts can attest a local owner and read a typed financial summary
  on enforced books. The read provides attributed access data to the host's
  audit sink and works with a read-only ledger snapshot.
  ([#260](https://github.com/Govcraft/beankeeper/issues/260))

- Embedded hosts can read the financial summary's inclusive current and
  comparison date windows and plain-language highlights directly from the typed
  response. ([#257](https://github.com/Govcraft/beankeeper/issues/257))

- Record an unverified runtime hint on CLI and stdio MCP audit rows when known
  environment markers are present and the caller has no verified token.
  Schema v25 adds hash version 5 and preserves earlier hashes.
  ([#242](https://github.com/Govcraft/beankeeper/issues/242))

- Authenticated MCP clients can select a registered ledger for each request on
  an existing HTTP session. Beankeeper verifies the bearer and applies its
  signed write mode against that ledger. Hosts can register opened ledgers
  while clients are connected, and local CLI commands use the same target names.
  ([#246](https://github.com/Govcraft/beankeeper/issues/246))

- Signed per-agent write modes let the bookkeeper preset post directly by
  default. Issue a grant with `--write-mode propose-only` to make `bk txn post`
  and MCP `txn_post` reserve pending proposals. The owner can undo a direct
  post with an immediate audited reversal in either interface.
  ([#244](https://github.com/Govcraft/beankeeper/issues/244))

- MCP `company_create` uses the same audited insert as CLI and setup book
  creation. All creation commands and tools trim company names and require
  a nonblank name. An MCP `company_delete` call confirms and audits deletion
  of an eligible company. Companies with retained data keep their history.
  ([#234](https://github.com/Govcraft/beankeeper/issues/234))

- Trusted hosts can initialize a separate, optionally encrypted demo ledger
  through `create_demo_ledger`. The initializer refuses preexisting files.
  ([#234](https://github.com/Govcraft/beankeeper/issues/234))

- Read-only `bk summary` and `financial_summary` MCP snapshots with matching JSON
  envelopes: standing balances, period comparisons, spending movers, attention
  counts/totals, plain-language highlights, chartable data and optional Desktop
  targets. Missing invoice due dates are explicit; age-based open items are
  reported separately. ([#116](https://github.com/Govcraft/beankeeper/issues/116))

- Period comparisons and what-changed answers for income statements, trial
  balances, spending/net-worth trends, cash flow, account balances and balance
  sheets through CLI and MCP. `--compare previous|year-ago|START..END` returns
  ranked movers, chartable current/prior/change values, nullable percentages,
  plain summaries and optional Desktop register links. `--explain-change CODE`
  ranks signed transaction and recorded-payee contributions. Existing
  `prior-period` and `prior-year` names remain accepted. (#117)

- Optional version 1 `data.chart` projections via `--json --chart` and MCP
  `chart: true` for financial summaries, spending trends/categories, net-worth
  history, cash flow and period/comparison reports. Exact decimal strings,
  currency units and Desktop detail targets let agents draw their own charts.
  Charts default to eight categories/series plus Other; `--chart-all` / `chart_all`
  returns all categories without changing source report fields.
  ([#118](https://github.com/Govcraft/beankeeper/issues/118))

### Changed

- Clarify the README’s open-source core and source-available Desktop licensing, link the developer site, and name ARM64 CLI artifacts explicitly. (#325)

- Summary and statement lists name the accounts and monthly statements needing checks, newest first, across CLI and MCP. (#318)

- Return typed ledger schema mismatches to Rust hosts, require a verified snapshot for migrations, and report automatic upgrades and the verified backup path in standalone CLI JSON and MCP. ([#309](https://github.com/Govcraft/beankeeper/issues/309))

- CLI and MCP transaction lists and counts hide both sides of audited reversals by default, with an option to include them. ([#294](https://github.com/Govcraft/beankeeper/issues/294))

- CLI and MCP reject invalid calendar dates and blank audit actor filters at input parsing with `VALIDATION`, and share typed local-day report defaults. ([#255](https://github.com/Govcraft/beankeeper/issues/255))

- Regular PRs can record a change under Unreleased without a version bump. Release PRs still require a matching dated section for the proposed CLI tag. ([#268](https://github.com/Govcraft/beankeeper/issues/268))

- Owners can undo categorization rule creation through the CLI or MCP. The rule and an audit receipt remain on record. ([#267](https://github.com/Govcraft/beankeeper/issues/267))

- Read grouped audit receipts and undo eligible changes immediately from the CLI or MCP with the owner's signed grant. ([#262](https://github.com/Govcraft/beankeeper/issues/262))

- CLI, MCP, and embedded hosts can execute the same typed ledger requests.
  MCP discovery generates schemas and capability hints from the operation
  catalog. Omitted transaction posting currency follows
  `BEANKEEPER_CURRENCY` when set and otherwise defaults to USD.
  ([#240](https://github.com/Govcraft/beankeeper/issues/240))

- CLI and MCP `verify` return local verification evidence, including per-company
  currency totals, audit-chain head and row counts. Integrity errors name the
  first failing check and identify the affected record where available.
  ([#235](https://github.com/Govcraft/beankeeper/issues/235))

- Comparison CSV uses a `row_type` column for accounts, totals, movers and
  explanation rows. Non-comparison output formats are unchanged.

### Fixed

- Embedded hosts can allow audited receipt and rule undo in read-only books through the same write policy for CLI and MCP. ([#315](https://github.com/Govcraft/beankeeper/issues/315))

- Let existing bookkeepers reverse postings and undo eligible receipts through CLI and MCP, with pending corrections only for propose-only credentials. ([#287](https://github.com/Govcraft/beankeeper/issues/287))

- Existing propose-only bookkeepers can configure books and accounts directly and submit openings and statement rows through transaction sign-off on the CLI and MCP. ([#275](https://github.com/Govcraft/beankeeper/issues/275))

- MCP tool schemas describe nested objects and arrays inline, and clients such as Claude Desktop can send those arguments as JSON text. ([#277](https://github.com/Govcraft/beankeeper/issues/277))

- Tag posted transactions through MCP using an object such as `{"status":"pending"}`, with clear validation errors and one-step annotation undo through CLI or MCP. ([#279](https://github.com/Govcraft/beankeeper/issues/279))

- Demo books credit generated activity to `demo` and record the initializer's
  actor on company creation and one `demo_seed` audit event. Generated postings
  carry the reserved `beankeeper_source` metadata key. Schema v24 preserves
  existing audit rows and hashes. ([#243](https://github.com/Govcraft/beankeeper/issues/243))

- CLI and MCP clearing accept signed statement amounts. Point-in-time reports
  accept an as-of date alias and reject requests that supply both date names.
  MCP discovery explains report options and how to request account balances.
  Same-company correlation errors point to the audited reversal and metadata
  tools. Audit history pages have a 100-row maximum and return a continuation
  offset when more rows remain.
  ([#236](https://github.com/Govcraft/beankeeper/issues/236))

- MCP writes honor the configured actor and use the verified agent URI when no
  label is configured, matching CLI attribution. Filter audit rows by actor
  with `audit_log.actor` or `bk audit log --by-actor`. ([#233](https://github.com/Govcraft/beankeeper/issues/233))

- Transaction list and count interpret amount filters in one currency. They
  infer a sole transaction currency and require an explicit choice when the
  company uses several. Applies to both CLI and MCP.
  ([#155](https://github.com/Govcraft/beankeeper/issues/155))

- Multi-column and cash-flow aggregation reject arithmetic overflow instead of
  silently saturating amounts used in comparison answers.

## [0.27.0] - 2026-11-20

Released as [beankeeper-cli-v0.27.0](https://github.com/Govcraft/beankeeper/releases/tag/beankeeper-cli-v0.27.0).
This release includes the changes previously tracked under 0.24.0, 0.25.0,
0.26.0 and 0.26.1, none of which shipped separately. It adds three schema
migrations (v18, v19, v20); each makes older binaries refuse to append to an
upgraded ledger. Back up the full database before upgrading.

### Added

- Per-ledger capability enforcement through `bk policy show` and
  `bk policy set --enforce on|off`, defaulting to off. Once enabled, CLI and MCP
  require valid tokens and explicit read/post/reverse/reconcile/budget/admin
  grants. Missing credentials return `UNAUTHORIZED` (exit 6); insufficient
  grants return `FORBIDDEN` (exit 7). Policy changes are audited.
  ([#72](https://github.com/Govcraft/beankeeper/issues/72))
- Authenticated MCP Streamable HTTP at `/mcp` with `bk mcp serve --http`,
  defaulting to `127.0.0.1:8750`. `--bind` selects an address; non-loopback binds
  require `--allow-remote` and serve plaintext HTTP without TLS.
- Every HTTP request verifies an `Authorization: Bearer` agent token, even when
  ledger enforcement is off. Process credentials are a fallback for absent
  headers. Discovery and tool authorization use the current requesting token and
  policy, with live expiry/revocation checks and the existing read-only ceiling.
  Stdio remains the default.
  ([#74](https://github.com/Govcraft/beankeeper/issues/74))
- Per-agent, per-company/currency spend limits through `agent limit set/show/clear`,
  enforced inside the shared posting savepoint. Total debits measure each posting;
  transaction-date daily buckets include approved proposals charged to the proposer.
- Pending posting review with `txn propose/proposals/approve/reject`, the `withdraw`
  alias, and matching MCP tools. Sibling `ledger/propose` and `ledger/approve` grants
  control ownership and review; approval posts atomically with approver attribution,
  a reserved proposal link, and the complete proposer request in the audit trail.
  ([#73](https://github.com/Govcraft/beankeeper/issues/73))
- Company-scoped period close/reopen/list commands and MCP tools. Inclusive
  cutoffs reject backdated posts, imports, schedule occurrences, loan payments,
  investment trades, reversals, and proposal approvals. Reopen requires an
  audited reason; close/reopen require `ledger/admin` under enforcement.
  ([#102](https://github.com/Govcraft/beankeeper/issues/102))
- Public `beankeeper_cli::mcp::http_router` factory for embedding the authenticated
  `/mcp` endpoint beside host application routes on one listener. Hosts supply
  transport configuration and control cancellation; the CLI keeps its existing
  credential, capability, read-only and bind behavior.
  ([#106](https://github.com/Govcraft/beankeeper/issues/106))
- Public `beankeeper_cli::mcp::http_router_with_passphrase` factory for sharing
  one resolved in-memory secret with a read-only snapshot across platforms.
  The existing CLI-source factory delegates to it and preserves authentication.
  ([#109](https://github.com/Govcraft/beankeeper/issues/109))
- UI-scoped `ui/navigate` tokens and authenticated HTTP host tool composition
  through `http_router_with_host_tools` and `HostToolHandler`. Host discovery
  and calls require explicit current-request grants, live verification, and
  the read-only ceiling; existing ledger tokens retain their scope.
  ([#110](https://github.com/Govcraft/beankeeper/issues/110))
- Release archives and `checksums-sha256.txt` receive Sigstore keyless signing
  bundles (`<artifact>.sigstore`) and GitHub build provenance attestations.
  The installer verifies both bundles when cosign is available and stops on
  verification failures; without cosign it warns and checks SHA-256 only.
  ([#103](https://github.com/Govcraft/beankeeper/issues/103))

### Changed

- Schema v18 preserves historical audit hashes and adds default-off policy
  storage. New v3 audit records block older binaries from appending, including
  when enforcement is off.
- MCP discovery reflects startup grants, with live policy and capability checks
  on every call. Bare `ledger` grants are rejected. Forced initialization cannot
  overwrite an enforced or unreadable ledger. Backup and restore require
  `ledger/admin` when source enforcement is on; restore also requires destination
  enforcement to be disabled before replacement.
- Schema v19 preserves policy and historical audit hashes while adding proposals,
  reference reservations, spend buckets and audited resolutions/limit changes.
- Importers preserve authorization error codes on spend denial. Posting dates are
  canonicalized so alternate date spellings cannot split the same day's bucket.
- Schema v20 preserves existing policy, proposals, and historical audit hashes.
  New v4 audit records prevent older binaries from appending. `verify`, backup,
  and restore detect missing or changed period lock state.
- The CLI is no longer published to crates.io (`publish = false`); it ships only
  as GitHub Release binaries through `install.sh`, AUR, or manual download. The
  release workflow publishes only the `beankeeper` library, and `cargo binstall`
  metadata was removed.
- Feature and docs PRs keep crate and skill versions unchanged and collect notes
  under Unreleased. Tagged release preflight requires a matching dated changelog
  section; versions are assigned once during release preparation.
  ([#91](https://github.com/Govcraft/beankeeper/issues/91))

### Fixed

- MCP `--read-only` now hides `txn_reconcile` along with all other write tools.
- Refreshed `init --demo` with Q1 2025 examples for budgets, import rules,
  recurring schedules, investment purchases and a partial sale, mapped tax
  expenses and contractor payments, a funded loan and first payment, and a
  short-pay settlement. The README walkthrough uses matching dates and covers
  these examples; `--demo` help now describes multi-company data.
  ([#84](https://github.com/Govcraft/beankeeper/issues/84))

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
