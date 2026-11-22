# Beankeeper

The accounting runtime for AI agents. Your assistant manages your books -- `bk` makes sure it can't mess them up.

A CLI backed by SQLCipher-encrypted SQLite, with an embeddable Rust library underneath.

## Why Beankeeper?

AI assistants can already categorise expenses, reconcile bank feeds, and generate financial reports. The missing piece is a backend that makes those operations *safe* -- one where the agent physically cannot produce an unbalanced ledger, silently duplicate a transaction, or corrupt the audit trail.

hledger, ledger-cli, and beancount are built around plain-text files a human edits by hand. That's the right design for manual bookkeeping. `bk` is built for when an agent is doing it.

### The backing store is a database, not a file

SQLite (optionally encrypted with SQLCipher) is the source of truth, not a `.journal` file. That means the agent can post transactions, query balances, and generate reports through a stable CLI contract without worrying about file locking, merge conflicts, or parse errors.

### The agent cannot create an unbalanced transaction

The CLI’s private transaction writer requires a validated `Transaction` and proof
that its company’s posting date falls in an open period. `JournalEntry` checks
entry currency as entries are added, and `JournalEntry::post()` requires at least
two entries with equal debit and credit totals. Account lookup, journal
validation, and the period check run in the savepoint that writes the posting.
Balance and closed-period failures return `VALIDATION` (exit code 3).

### Retries are safe

Agents retry. Import scripts fail mid-batch. `--reference` takes a non-empty string (a bank reference number, a statement row hash, whatever), trims surrounding whitespace, and stores it verbatim as a key unique per company. Subsequent posts with the same reference in that company are rejected with the existing transaction ID, or skipped with `--on-conflict skip`. Retrying a referenced post does not create duplicates.

### The agent gets structured feedback, not text to parse

JSON command responses use the same envelope, except for successful `export` output, which uses the file format described below:

```json
{ "ok": true,  "meta": { "command": "txn.post", "company": "acme", "timestamp": "..." }, "data": { ... } }
{ "ok": false, "meta": { "command": "txn.post", "company": "acme", "timestamp": "..." }, "error": { "code": "NOT_FOUND", "message": "..." } }
```

Monetary JSON fields retain integer minor units and add decimal-string companions,
for example `"amount": 180000, "amount_display": "1800.00"` for USD.
`txn list` and `audit log` keep their data arrays and expose
`meta.pagination` with filtered totals and `has_more`; table mode warns on stderr
when more rows exist. Continue with `--offset` and `--limit`. MCP tools expose
the same amounts and put pagination in the tool result’s `_meta.pagination`.

Named error codes (`NOT_FOUND`, `VALIDATION`, `DATABASE`, etc.), `meta.command` in dot notation, consistent field presence. The agent can handle `bk` output reliably without parsing human-readable text or guessing at shapes.

A successful `bk export --output-format json` writes a standalone `{"beankeeper_export": {"version": 1, "companies": [...]}}` object, without `ok`, `meta`, or `data`, even with `--json`. Read its companies with `jq '.beankeeper_export.companies'`.

### The audit trail is tamper-evident

Database triggers reject changes to posted financial fields, entry details, and audit records, including deletion and replacement. Transaction metadata and entry clearance status remain mutable for correlation, reversals, and reconciliation. Supported mutations append an actor, timestamp, and before/after snapshots in the same atomic operation.

Every new posting, including imports and reversals, stores `posted_by` and a `create` audit record containing its immutable transaction and entry details. Set the principal with `--actor NAME` or `BK_ACTOR`. `bk txn show` displays the poster; JSON transaction output includes `posted_by`. Legacy transactions have an unknown poster (`null` in JSON).

`bk verify` checks every company in one ledger snapshot. It reports currency-by-currency
debit and credit totals, row counts, named checks, and the SHA-256 audit-chain head
after recomputing every row hash. Use `bk --json verify` or the MCP `verify` tool for
the same structured evidence. Failures identify the first failed check and the
affected record where available (`DATABASE`, exit code 4).
See [verification and its limits](docs/audit-integrity.md#verification-and-limits).

These protections detect tampering against the stored chain; they are not signatures or an external trust anchor. Someone with unrestricted database access can remove triggers and rewrite the ledger, hashes, and sequence together, or restore an older complete database. See [audit integrity](docs/audit-integrity.md) for the format, migration behavior, and limits.

### Verified agent identity and capability enforcement

`bk key init` creates a local trust root inside an encrypted ledger.
`bk agent issue --name claude --public-key "$AGENT_PUBLIC_KEY" --ttl 8h` issues
an attestation. Supply it with `--agent-token` or `BK_AGENT_TOKEN` to record its
verified `agent_uri` and `token_jti` on new postings and audit rows. `txn show`,
`audit log --agent URI`, and JSON `meta.agent` expose attribution; `agent list`
and `agent revoke URI_OR_JTI` manage issued tokens. Stdio MCP uses one process
token; HTTP accepts a bearer token per request and always requires verification.
Both recheck every call. Invalid, expired or revoked supplied tokens return
`UNAUTHORIZED` (exit 6 for CLI, HTTP 401 at the HTTP boundary).

Enforcement defaults to off. CLI 0.24.0 adds `bk policy show` and
`bk policy set --enforce on|off`. Bootstrap by issuing a token with
`ledger/read,ledger/admin`, saving it, then enabling as a named actor. Once on,
all reads and mutations require tokens. Missing credentials return `UNAUTHORIZED`
(exit 6); insufficient grants return `FORBIDDEN` (exit 7). Policy changes require
`ledger/admin` and append `policy_change` audit records.

| Grant | Operations |
| --- | --- |
| `ledger/read` | Lists, shows, reports, export, verify, audit, agent list, policy show, loan schedule |
| `ledger/post` | Posting/import/attachments/annotations, investment trades, security writes, loan pay, schedule writes, rule writes |
| `ledger/propose` | Propose, list and withdraw owned pending requests |
| `ledger/approve` | List all proposals, approve or reject |
| `ledger/reverse` | txn reverse |
| `ledger/reconcile` | txn clear/reconcile |
| `ledger/budget` | budget set/delete |
| `ledger/admin` | Company/account writes, key init, agent issue/revoke, agent limit set/clear, policy set, backup/restore |

Grants are siblings: admin does not imply posting, and post does not imply
reverse. Issue the union needed; bare `ledger` grants are rejected. Stdio MCP
filters tools by startup grants; HTTP filters discovery per request. Both recheck
every call, and `--read-only` remains a ceiling.
Tokens are bearer credentials without proof of private-key possession or signed
writes. Back up before schema v18 upgrades: existing hashes remain intact, but
older binaries cannot append the new v3 audit format. `init --force` requires an
admin to disable enforcement first. See
[agent identity setup, capability map, and limits](docs/agent-identity.md).

### Period close and locking

```bash
bk --company acme period close --through 2026-01-31
bk --company acme --json period list
bk --company acme period reopen --reason "Correct a missing January receipt"
```

Companies close independently. The inclusive cutoff blocks new postings on or
before that date, across currencies and all posting paths, including imports,
schedules, loans, investments, reversals, and proposal approval. Rejected approvals
stay pending. A close may advance the cutoff; reopening clears it with a required
reason. Close/reopen require `ledger/admin` under enforcement, and list requires
`ledger/read`. CLI and MCP share these rules; `period_close`, `period_reopen`, and
`period_list` expose them to agents.

Schema v20 preserves historical hashes, policy, and proposals. Back up before
upgrading: new audit records use version 4, which prevents older binaries from
appending. Close/reopen events record the actor and before/after state; `verify`
checks cutoff integrity. See [period commands](docs/beankeeper/references/commands.md#period-close-and-locking)
for output fields, history, and retry behavior.

### Spend limits and approval gates

CLI 0.26.0 adds per-agent, per-company spend limits and pending proposals:

```sh
bk agent limit set "$AGENT_URI" --company acme --currency USD --max-post 500 --daily-limit 2000
bk --agent-token "$POSTER_TOKEN" --company acme txn propose \
  -d "Equipment purchase" --debit 5000:750 --credit 1000:750 --reference equipment-001
bk --agent-token "$REVIEWER_TOKEN" --company acme txn proposals
bk --agent-token "$REVIEWER_TOKEN" --company acme txn approve PROPOSAL_ID
```

`ledger/propose` permits proposals and withdrawal of owned pending requests;
`ledger/approve` permits approval and rejection. These are sibling grants, so
posting and administration do not imply approval. A proposal requires a verified
token, reserves its reference, and stays outside posted transactions and their
audit chain until resolution. Approval posts atomically with the approver's
identity and a reserved `proposal_id` metadata link; its audit record preserves
the proposer and the reviewed payload.

Limits measure total debits, including split entries, and apply through the
shared posting path, including imports, loans, investments and schedules.
All tokens for an agent URI share each company/currency bucket. Daily totals use
the transaction date, so backdated and future-dated posts use those dates' buckets.
Approval may exceed the proposer's limits, while consuming the proposer's bucket.
With no limits configured there is no amount cap; once any currency limit exists
for an agent/company, unconfigured currencies are forbidden. Set/clear requires
`ledger/admin` when enforcement is on. Configured limits apply to verified agents
even when enforcement is off; named actors without tokens have no agent bucket.
See [spend limits and proposals](docs/agent-identity.md#spend-limits-and-proposals)
for setup, permissions and schema v19 migration behavior.

### Everything else

- **Multi-company tenancy** -- multiple entities in one database, isolated by slug; the agent manages your LLC and personal books in the same place
- **Document attachments** -- receipts and invoices linked to transactions via SHA-256 content addressing
- **Tax category tagging** -- per-entry or account-level defaults, free-form strings mapping to any tax form lines you need
- **Encryption at rest** -- SQLCipher baked in from the start, not bolted on
- **Intercompany linking** -- mirror transactions across entities stay paired through bidirectional correlation IDs; `bk txn reconcile` catches orphans
- **Receivables / payables aging** -- `bk report aging --account <code>` buckets open charges on asset (AR) or liability (AP) accounts into Current / 1-30 / 31-60 / 61-90 / 90+ days, FIFO or matched to specific invoices, optionally per customer or vendor from JSON metadata, with an open-item `--detail` view. Several sub-ledgers (or a control account and its children) age in one run, and `--filter-metadata status=disputed` shows a tagged slice at its original age
- **Statement settlement reconciliation** -- `bk report reconciliation --mode settlement --account <AR/AP code> --statement <id> --statement-balance <net>` compares charge applications with carrier or vendor payments, preserves original short-pay dates, and lists linked payments, overpayments, and unmatched credits separately. Bank mode remains the default
- **Budgeting and variance analysis** -- set monthly or annual spending/revenue targets per account, then run `bk report budget-variance` to see where actuals land relative to the plan

If you want to edit your ledger in `$EDITOR`, the plain-text tools are excellent. If you want your AI assistant to do the bookkeeping while you keep the guardrails, `bk` is the right foundation.

## Loan amortization

Calculate a monthly fixed-rate schedule without a database or company:

```sh
bk loan schedule --principal 200000 --rate 6 --months 360 --currency USD --json
```

Post a scheduled payment to existing accounts: principal debits a liability,
interest debits an expense, and the payment credits an asset.

```sh
bk --company personal loan pay --principal 200000 --rate 6 --months 360 \
  --payment-number 1 --liability 2000 --interest-account 5100 \
  --paying-account 1000 --date 2026-11-20 --reference mortgage-2026-11
```

Rates are nominal annual percentages divided by twelve, from 0 to 100 with
up to four decimal places; terms are 1 through 1,200 months. Payments and monthly
interest round half up to the currency's minor unit, including JPY and BHD.
The final payment clears the remaining principal. The regular payment is at
least one minor unit, so very small loans can finish early. The schedule supports
table, JSON and CSV output; JSON retains minor-unit integers and adds display
strings. CSV monetary columns contain minor-unit integers.

Payments use the existing audited transaction path and account tax defaults.
Reuse the same reference for retries, optionally with `--on-conflict skip`.
This helper is stateless: supply the original terms and the intended payment
number each time. It does not track payment order or compare the schedule with
the current liability balance. Extra payments, daily interest, fees, variable
rates and persistent loan records are outside this feature. A supplied agent
token is verified against the database, including for schedule calculations.
See the [command reference](docs/beankeeper/references/commands.md#loans).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper/main/install.sh | sh
```

Installs the latest **GitHub release**, which can lag behind `main`. The script
detects your OS and architecture, downloads the right binary, verifies the
SHA-256 checksum, and installs to `/usr/local/bin`. Override the install directory
with `BK_INSTALL=~/.local/bin` or pin a published CLI release with
`BK_VERSION=X.Y.Z`. When `cosign` is on `PATH`, the installer also verifies the
archive and checksum file against their Sigstore bundles before extraction.
Missing bundles (HTTP 404) warn that the release predates signing and fall back
to SHA-256; other download errors or failed verification stop installation.
Any present bundle must still verify. Without `cosign`,
installation checks SHA-256 and warns that authenticity was not verified.

New releases attach `<artifact>.sigstore` for every archive and for
`checksums-sha256.txt`, plus GitHub build provenance attestations. To verify a
download manually, set `VERSION` to the published version and `ARTIFACT` to the
downloaded archive (or `checksums-sha256.txt`), with its adjacent bundle:

```sh
VERSION=X.Y.Z
ARTIFACT="bk-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
cosign verify-blob "$ARTIFACT" --bundle "${ARTIFACT}.sigstore" \
  --certificate-identity-regexp '^https://github\.com/Govcraft/beankeeper/\.github/workflows/release\.yml@refs/tags/beankeeper-cli-v[0-9]+\.[0-9]+\.[0-9]+$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
gh attestation verify "$ARTIFACT" --repo Govcraft/beankeeper \
  --signer-workflow Govcraft/beankeeper/.github/workflows/release.yml \
  --source-ref "refs/tags/beankeeper-cli-v${VERSION}"
```

For automatic updates, require the **exact version's identity**, as the installer
does. The [verification contract](docs/RELEASE.md#sigstore-verification-contract)
includes download commands and that stricter check. Sigstore keyless signing
does not replace Apple notarization or Windows Authenticode.

See the [changelog](CHANGELOG.md) for user-visible changes
and the [release procedure](docs/RELEASE.md) for cutting and verifying a release.
Contributors add notes under `[Unreleased]` and leave crate and skill versions
unchanged until release prep; see the [contributor convention](docs/RELEASE.md#agent--contributor-convention).

**Arch Linux (AUR)**:

```sh
yay -S bk-bin
```

The CLI (package `beankeeper-cli`, executable `bk`) is distributed only as
GitHub Release binaries, through `install.sh`, AUR, or a manual download. It is
not published to crates.io, so `cargo install beankeeper-cli` and
`cargo binstall beankeeper-cli` are not available; build from Git instead.
Only the `beankeeper` library is published on crates.io.

**From source (builds from `main`)**:

```sh
cargo install --git https://github.com/Govcraft/beankeeper beankeeper-cli
```

**Rust library**:

```sh
cargo add beankeeper
```

This adds the embeddable library to your Rust project; it does not install `bk`.

## Quick Start

```sh
# Create a database
bk init

# Create at a specific path
bk init --path /data/books.db

# Create an encrypted database (prompts for passphrase)
bk init --encrypt

# Set up companies
bk company create personal "Personal Finances" --description "Day-to-day expenses"
bk company create govcraft "GovCraft LLC" --description "Consulting entity"

# Create accounts
bk --company personal account create 1000 "Cash" --type asset
bk --company personal account create 4000 "Revenue" --type revenue
bk --company personal account create 5000 "Rent" --type expense

# Post a transaction (amounts are in dollars, not cents)
bk --company personal txn post -d "March rent" --date 2026-03-01 \
  --debit 5000:1200 --credit 1000:1200

# Split entries with per-line memos
bk --company personal txn post -d "Paycheck" \
  --debit "1000:3800:Net pay" \
  --credit "4000:5000:Gross salary" \
  --debit "5100:1200:Federal tax"

# Reports
bk --company personal report trial-balance --from 2026-01-01 --to 2026-03-31
bk --company personal report balance --account 1000 --to 2026-03-31
bk --company personal report balance-sheet --to 2026-03-31
bk --company personal report income-statement --from 2026-01-01 --to 2026-12-31
bk --company personal report ledger --account 1000 --from 2026-01-01 --to 2026-03-31
# Multi-currency companies pick the report currency explicitly
bk --company personal report trial-balance --currency MXN
bk --company personal report reconciliation --account 1000 --statement-balance 4210.33
bk --company personal report cash-flow --from 2026-01-01 --to 2026-03-31

# Set a budget and compare against actuals
bk --company personal budget set 5000 --year 2026 --annual 14400
bk --company personal report budget-variance --year 2026
```

## Scheduled and recurring transactions

Pending templates describe future obligations without affecting posted balances or
reports. Create a one-time reminder or repeat it daily, weekly, monthly, or yearly:

```bash
bk --company acme schedule create -d "Monthly rent" \
  --start 2026-01-31 --frequency monthly --end 2026-12-31 \
  --debit "5000:1200:Office rent" --credit 1000:1200 --meta vendor=landlord
bk --company acme schedule list --json
bk --company acme schedule list --due-through 2026-03-31
bk --company acme schedule run --through 2026-03-31 --dry-run
bk --company acme schedule run --through 2026-03-31
bk --company acme schedule cancel <SCHEDULE_ID>
bk --company acme schedule list --all --json
```

`--start` is required; `--frequency` defaults to `once`. `--end` is inclusive.
Amounts use major currency units, just like `txn post`; repeated `--debit` and
`--credit` support splits and optional memos. `--currency` defaults to
`BEANKEEPER_CURRENCY`, then USD. Metadata (`-m`, repeated `--meta`) and explicit
`--tax account=category` values are copied to every occurrence. Account tax
category defaults are captured when the template is created.

`list` shows pending templates and their next due date; `--due-through` limits
reminders to templates due by that date, while `--all` also shows completed and
cancelled templates. JSON create/cancel responses contain one template; list
responses contain an array. Templates include their entries, currency, metadata,
start/end dates, cadence, next due date, and status. CSV/table output summarizes
templates; JSON entry amounts include integer minor units and decimal displays.

`run` posts pending occurrences through today in local time by default, with a
limit of 1,000 occurrences across all pending templates in the selected company.
A larger run fails with validation exit code 3 before posting anything, advancing
progress, or writing audit records. Preview the count with `--dry-run`, choose an
earlier `--through` date, or explicitly permit the full run with
`--allow-large-run`:

```bash
bk --company acme schedule run --through 2030-12-31 --dry-run
bk --company acme schedule run --through 2030-12-31 --allow-large-run
```

The limit also applies when `--through` is omitted. It counts only occurrences
still due, so previously posted, completed, cancelled, future-due, and other
companies' occurrences do not count. The override retains atomic posting and
retry safety; it does not split the run into batches.

`--through YYYY-MM-DD` supplies an explicit inclusive cutoff, including future
posting dates if intentionally requested. Monthly/yearly dates use the original
start as their anchor: January 31 becomes February 28 (29 in leap years), then
March 31; February 29 returns in later leap years. Daily/weekly intervals use
calendar days. One-time templates complete after their first posting; recurring
templates complete when their next occurrence would exceed `--end`.

The entire company-scoped run commits atomically with its audit records and
schedule progress. Each posting uses reference `schedule:<SCHEDULE_ID>:<YYYY-MM-DD>`.
Retries after a successful run return an empty array. `run --dry-run` previews
`{schedule_id, date, transaction_id: null}` rows without posting, advancing progress,
or writing audit records; real runs return the same shape with transaction IDs.
Dry runs are unrestricted and print the total due occurrence count to stderr in
all output formats. `--quiet` suppresses that summary. JSON stdout retains the
existing occurrence array; its length is the count for scripts.
New entries start uncleared, as with ordinary postings.

Nothing runs in the background. Invoke `schedule run` explicitly, or call it
from cron/systemd with `--db /absolute/path/books.db --company acme`. A cancelled
template keeps its history and existing postings. Cancellation is idempotent.
To change a template, cancel it and create a replacement. Accounts and companies
referenced by retained templates cannot be deleted. Schedule creation,
cancellation, and progress are audited under entity `schedule`, with actions
`create`, `schedule_cancel`, and `schedule_run`.

## Securities and investment lots

Track exact share quantities, acquisition lots, dated prices, and cost basis
alongside the monetary ledger. Buys and sells create balanced, audited postings
and lot records atomically. Sales use FIFO by default, with LIFO and specific-lot
selection available.

```sh
bk --company acme security create ABC --name 'ABC shares' --currency USD
bk --company acme investment buy ABC --quantity 1.5 --cost 150.00 \
  --date 2026-01-02 --asset 1200 --cash 1010 --reference broker-buy-001
bk --company acme investment sell ABC --quantity 0.5 --proceeds 60.00 \
  --date 2026-02-02 --asset 1200 --cash 1010 --gain 4100
bk --company acme security price ABC --date 2026-02-02 --price 120.00
bk --company acme investment holdings --as-of 2026-02-02 --json
```

Create the investment/cash asset accounts and gain revenue account first.
Quantities support eight decimal places. Costs, proceeds, and prices use major
currency units on input; JSON retains integer minor units with `_display`
companions. Partial sales round basis half-up and conserve residual cost.
Holdings report latest price, market value, and unrealized gain/loss without
posting them. Missing prices produce null valuations. Use `investment lots ABC`
to inspect remaining quantities and basis, or `--method specific --lot LOT_ID`
on a sale to select one lot.

Record trades in date order for each security and investment account. Investment
trades cannot use generic `txn reverse`; lot-aware corrections are follow-up
work. MCP provides all seven security/investment operations and omits writes in
read-only mode. See [investment lots](docs/investments.md) for setup, rounding,
historical reports, audit behavior, and phase-one limits.

## Connect an AI assistant through MCP

`bk mcp serve` exposes an existing ledger over the Model Context Protocol using
stdio. Initialize the ledger and its companies with the CLI first, then configure
your assistant to launch the server. Each process opens one database and resolves
its passphrase once at startup. Missing databases and incorrect passphrases fail
before the protocol starts.

For Claude Desktop, add this entry to `claude_desktop_config.json`, adjusting the
absolute binary and database paths:

```json
{
  "mcpServers": {
    "bk": {
      "command": "/usr/local/bin/bk",
      "args": ["--db", "/absolute/path/books.db", "mcp", "serve"],
      "env": {
        "BEANKEEPER_PASSPHRASE_CMD": "secret-tool lookup service beankeeper ledger books"
      }
    }
  }
}
```

For Codex, add this entry to `~/.codex/config.toml`:

```toml
[mcp_servers.bk]
command = "/usr/local/bin/bk"
args = ["--db", "/absolute/path/books.db", "mcp", "serve"]

[mcp_servers.bk.env]
BEANKEEPER_PASSPHRASE_CMD = "secret-tool lookup service beankeeper ledger books"
```

The example command retrieves a passphrase from an existing desktop keyring entry;
replace it with your password manager's command. The command must run without
interactive input and print only the passphrase. Omit the environment entry for an
unencrypted ledger. `BEANKEEPER_DB`, `--passphrase-file`, and a separate
`--passphrase-fd` also work. File descriptor 0 is reserved for MCP. Passphrases are
never tool arguments. Append `"--read-only"` to `args` to register only read tools;
mutation tools are absent and cannot be called.

### Available tools

| Tools | Purpose |
| --- | --- |
| `ledger_setup_begin`, `ledger_setup_finish`, `ledger_setup_revoke` | Use a live owner Admin credential to issue or end an encrypted ledger's setup session |
| `ledger_setup_book_create`, `ledger_setup_book_initialize`, `ledger_setup_book_update`, `ledger_setup_chart_apply`, `ledger_setup_account_add` | Configure personal and business books, charts, and accounts with an active setup grant |
| `ledger_setup_opening_preview` | Validate a balanced opening journal within an active setup generation |
| `ledger_setup_opening_reserve` | Hold the exact previewed opening journal for the existing approval workflow |
| `ledger_setup_statement_preview` | Freeze the entries parsed from bounded CSV, OFX, or QFX content for review |
| `ledger_setup_statement_commit` | Post the reviewed batch with a separate live approval credential |
| `ledger_setup_statement_receipt` | Read statement provenance and the transaction IDs the batch created |
| `period_close`, `period_reopen`, `period_list` | Close company posting dates, explicitly reopen with a reason, and inspect cutoffs |
| `company_list`, `company_show` | Inspect companies |
| `account_list`, `account_show`, `account_create` | Inspect and create accounts |
| `txn_post`, `txn_reverse`, `txn_meta_set`, `txn_meta_unset` | Post, reverse and annotate transactions |
| `txn_list`, `txn_show`, `txn_clear`, `txn_reconcile` | Inspect transactions, clear entries and check intercompany links |
| `txn_propose`, `txn_proposals`, `txn_approve`, `txn_reject` | Submit, inspect and resolve pending proposals |
| `security_create`, `security_list`, `security_price` | Register securities and record prices |
| `investment_buy`, `investment_sell`, `investment_lots`, `investment_holdings` | Trade, inspect lots and value holdings |
| `financial_summary` | Read-only advisor facts and comparisons; `company`, optional dates, currency and account selector arrays; returns the full `bk summary` JSON envelope |
| `ledger_setup_status` | Read ledger readiness, including company, account, and transaction counts and whether the approval key is configured. Takes no arguments. |
| `ledger_setup_book_show` | Read a book's current profile, fiscal year, base currency, and named accounts |
| `report` | Run CLI reports |
| `budget_list`, `budget_set`, `budget_delete` | Inspect and manage budgets |
| `verify`, `audit_log` | Verify integrity and inspect the audit trail |

This catalog covers the full router. Capability enforcement can restrict discovery
further. Company-scoped tools require a `company` argument. `report` takes a `kind`
and its options, with all 13 CLI report kinds available, including `schedule-c`,
`1099` and `trend`. See the [report-kind list](docs/beankeeper/references/commands.md#report-kinds).
Tool schemas describe the accepted arguments. For example, `txn_post` takes:

```json
{
  "company": "personal",
  "description": "March rent",
  "date": "2026-03-01",
  "reference": "rent-2026-03",
  "on_conflict": "skip",
  "debits": [{"account": "5000", "amount": "1200.00", "memo": "Rent"}],
  "credits": [{"account": "1000", "amount": "1200.00"}],
  "meta": {"vendor": "Landlord"}
}
```

Always supply a stable `reference` and `on_conflict: "skip"` for safe retries.
The CLI's default `error` strategy rejects duplicates. Only `error` and `skip`
are accepted conflict strategies. Inputs use decimal major-unit strings, and results
use the same data payload and integer minor units as `bk --json`. Results are
available as JSON text content and, where the negotiated MCP protocol permits the
payload's shape, `structuredContent`. Tool failures set `isError: true` with
`{code, message}`. `financial_summary` instead returns the full CLI envelope on
both success and failure. Audited writes use `mcp:<client name>` from the MCP handshake,
with a sanitized name or `mcp:unknown`. Creation auditing is unchanged.

Call `txn_meta_set` with `company`, exactly one of `id` or `reference`, and a
`values` array such as `["status=disputed", "dispute_id=D-42"]`. `txn_meta_unset`
takes the same selector and a `keys` array. They share the CLI validation and
audit behavior and are absent in read-only mode.

Call `txn_reverse` with `company` and the original transaction `id`; `date`,
`description`, and `reference` are optional. It creates a linked mirror posting.
Supply the same explicit `reference` on retries to return the existing reversal
with `skipped: true` and `existing_id`. Reconciled transactions cannot be reversed;
existing clearings stay on the original and reversal entries start uncleared.

Ledger initialization, company creation/deletion, account deletion, import,
export, and attachments remain CLI operations. See the [MCP command reference](docs/beankeeper/references/commands.md#mcp).

### Streamable HTTP

Start a shared server over the same ledger tools:

```bash
bk --db /absolute/path/books.db --passphrase-file /secure/passphrase mcp serve --http
# Choose an address or let the OS select a free port:
bk --db /absolute/path/books.db mcp serve --http --bind 127.0.0.1:0 --read-only
```

The default endpoint is `http://127.0.0.1:8750/mcp`. The actual bound endpoint is
printed to stderr; stdout stays empty. Without `--http`, stdio remains the default.
Initialize an encrypted ledger and its trust root with `bk key init`, then issue
agent tokens as described in [agent identity](docs/agent-identity.md).

Every HTTP request requires a verified `agent://` token in
`Authorization: Bearer <token>`, even when ledger enforcement is off. In Cursor's
`mcp.json`, configure the endpoint and a token from the client's environment:

```json
{
  "mcpServers": {
    "bk": {
      "url": "http://127.0.0.1:8750/mcp",
      "headers": {
        "Authorization": "Bearer ${env:BK_AGENT_TOKEN}"
      }
    }
  }
}
```

See [Cursor's MCP configuration reference](https://cursor.com/docs/mcp) for
configuration locations and environment substitution. The token is issued by
this ledger, not an OAuth provider; OAuth discovery is not implemented.

Prefer per-request headers when agents share a server. A process-level
`--agent-token` or `BK_AGENT_TOKEN` is a fallback only when the Authorization header
is absent. An invalid or malformed explicit header never falls back. Missing,
expired, revoked, and untrusted tokens return HTTP 401 with `UNAUTHORIZED`.
When `bk policy set --enforce on` is active, tool discovery reflects the requesting
token's grants and calls without the required capability return `FORBIDDEN`.
Policy and credentials are rechecked for every call; MCP session IDs confer no
permissions. `--read-only` always hides all write tools, even for broader tokens.

Non-loopback binds require `--allow-remote`. This serves **plaintext HTTP without
TLS** and is not a hosted-tier implementation. TLS, authentication gateways, and
hosted discovery are outside scope. Loopback binds validate Host headers; specific
remote binds also allow their bound IP, while explicitly permitted wildcard binds
accept any Host. Requests with browser Origin headers are rejected. Use an MCP
client that sends bearer headers without a browser Origin.

Rust hosts can share their own axum listener with MCP by merging
`beankeeper_cli::mcp::http_router(&cli, read_only, config)?` into their router.
The returned router already mounts `/mcp` and authenticates that endpoint only.
Pass a `beankeeper_cli::mcp::StreamableHttpServerConfig`, keeping a clone of its
`cancellation_token` to cancel MCP sessions at shutdown. The host owns its runtime,
listener, shutdown handling, and Host/Origin configuration. Construction opens an
existing ledger and resolves the passphrase synchronously without prompting;
request credentials and capability checks work as described above. See the
factory's Rustdoc for a complete hosting example.

Hosts that also open a read-only `Db::open_snapshot` can resolve the passphrase
once with `beankeeper_cli::passphrase::resolve_passphrase`, then pass
`passphrase.as_ref()` to both the snapshot and
`beankeeper_cli::mcp::http_router_with_passphrase(&cli, passphrase.as_ref(), read_only, config)?`.
Use `None` for an unencrypted ledger. This factory ignores CLI passphrase sources
and `BEANKEEPER_PASSPHRASE_CMD`, borrows the secret only during construction, and
keeps the same MCP authentication. It works across platforms without a file
descriptor bridge or plaintext passphrase file. Drop the secret after opening
both connections. See its Rustdoc for an example.

Hosts can add application tools with `http_router_with_host_tools` and a
`HostToolHandler`. Beankeeper filters their discovery and dispatch using the
current request's verified grants, revocations, and read-only ceiling. Navigation
uses an explicit `ui/navigate` grant on a separate UI-scoped token. See the
[host tool API and example](docs/mcp-host-tools.md).

## Demo Mode

Create sample books for three companies and explore the walkthrough below:

```sh
bk init --demo
```

The demo uses fixed dates in **January through March 2025**, USD amounts, and
fictional counterparties and securities. Use the explicit dates below: reports
that default to the current year can be empty after 2025. Generated IDs and audit
timestamps vary between runs; the sample dates, amounts, and report totals stay
the same.

| Company | Slug | Examples |
|---------|------|----------|
| Acme Consulting LLC | `acme-consulting` | Service revenue, payroll, budgets, import rule, recurring rent, contractor payments, loan, settlement |
| Acme Products Inc | `acme-products` | Inventory, product sales, COGS, shipping |
| Personal | `personal` | Checking/savings, salary, personal expenses, investment lots |

Intercompany mirror pairs link the owner's loan, a software licence purchase,
its settlement, and payroll with federal, state, and FICA withholdings. Other
examples include a $2,500 monthly rent budget, an office-supplies import rule,
and pending monthly rent from March through December. The schedule preview
below leaves those obligations unposted.

The fictional `DEMO` fund has two purchases (10 shares at $100 and 5 at $120),
a FIFO sale of 4 shares for $520, and a March 31 price of $130. The remaining
11 shares have a $1,200 cost basis and a $1,430 market value.

Consulting includes two fictional contractor payments totaling $1,500, canonical
`sched-c:` tags, and the original free-form tags for comparison. A $12,000
business loan at 6% over 12 months includes
funding and its first payment. Loans are stateless, so the schedule command
repeats the original terms. Settlement account `1150` has a $1,800 invoice and
a linked $1,620 payment on statement `DEMO-2025-03`, leaving $180 open at the
invoice's original age.

Try it out:

```sh
# Trial balance for the consulting company
bk --company acme-consulting report trial-balance --from 2025-01-01 --to 2025-03-31

# View the payroll split transaction
bk --company acme-consulting txn list --description "payroll" --json

# Personal reports over the actual demo dates
bk --company personal report balance-sheet --to 2025-03-31
bk --company personal report trial-balance --from 2025-01-01 --to 2025-03-31
bk --company personal report balance --account 1000 --to 2025-03-16

# Rent budget versus actuals, plus the import categorization rule
bk --company acme-consulting report budget-variance --year 2025 --from 1 --to 3
bk --company acme-consulting rule list

# Preview the next rent payment without changing the books
bk --company acme-consulting schedule list
bk --company acme-consulting schedule run --through 2025-03-31 --dry-run

# Mapped Schedule C lines and contractor totals above the default $600 threshold
bk --company acme-consulting report schedule-c --from 2025-01-01 --to 2025-03-31
bk --company acme-consulting report 1099 --from 2025-01-01 --to 2025-03-31

# Open lots after the partial sale, and their quarter-end valuation
bk --company personal security list
bk --company personal investment lots DEMO
bk --company personal investment holdings --as-of 2025-03-31

# Original loan schedule and the posted principal/interest split
bk loan schedule --principal 12000 --rate 6 --months 12 --currency USD
bk --company acme-consulting report ledger --account 2100 --from 2025-01-01 --to 2025-03-31

# A reconciled $1,620 statement with a $180 short-pay still outstanding
bk --company acme-consulting report reconciliation --mode settlement --account 1150 \
  --statement DEMO-2025-03 --statement-balance 1620 --to 2025-03-31

# Check accounting invariants and the audit trail
bk verify
```

Beyond the core guarantees above, `bk` provides the following capabilities for multi-entity accounting, automation, and reporting.

### Intercompany Transactions

Mirror entries across companies stay linked through bidirectional correlation IDs:

```sh
# Post one side
bk --company govcraft txn post -d "Owner draw" \
  --debit 3000:500 --credit 1000:500

# Post the mirror entry, linking bidirectionally
bk --company personal txn post -d "Capital from GovCraft" \
  --debit 1000:500 --credit 3100:500 --correlate 1

# Verify no orphaned links
bk txn reconcile
```

### Idempotency Keys

When `bk` is driven by AI agents or import scripts, retries can cause duplicate transactions. The `--reference` flag provides a deterministic idempotency key -- same reference, same company, always maps to the same transaction.

The `--on-conflict` flag controls how duplicates are handled:

```sh
# First post succeeds
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95

# Default is --on-conflict error: retry is rejected
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95
# -> error: transaction with reference 'chase-2026-03-15-001' already exists (id: 1)

# Use --on-conflict skip to succeed silently
bk --company personal txn post -d "AWS March" \
  --reference "chase-2026-03-15-001" \
  --debit 5200:49.95 --credit 1000:49.95 --on-conflict skip
# -> [skipped] duplicate reference; transaction already exists (id: 1)
```

In JSON mode, `txn post` returns a `data` object indicating the result:

```json
{
  "ok": true,
  "meta": { ... },
  "data": {
    "id": 1,
    "created": true,
    "skipped": false
  }
}
```

If skipped, `id` is null and `existing_id` is provided:

```json
{
  "ok": true,
  "meta": { ... },
  "data": {
    "existing_id": 1,
    "created": false,
    "skipped": true
  }
}
```

References are stored verbatim after trimming surrounding whitespace; empty or whitespace-only references are rejected. The same reference can be used in different companies. Transactions without `--reference` have no reference-based deduplication. `--on-conflict` accepts only `error` or `skip`. Annotate existing postings with `txn meta set/unset`.

### Annotate a posted transaction

```sh
bk --company personal txn meta set 42 status=disputed dispute_id=D-42
bk --company personal txn meta set --reference CHARGE-42 status=resolved
bk --company personal txn meta unset 42 status dispute_id
bk audit log --action metadata_set
```

`set` adds or replaces top-level string values, matching `txn post --meta`:
`qty=3` stores `"3"`, `paid=true` stores `"true"`, and `note=` stores an empty
string. Split on the first `=`. Select by transaction ID or trimmed `--reference`
in the selected company. Unknown transactions return `NOT_FOUND`. Empty keys,
keys containing a double quote, duplicate keys, and the relationship keys
`correlate`, `reverses`, `reversed_by`, and `applies_to` are rejected. Set custom
settlement `--link-key` values at post time and treat them as relationships too.

Each changed batch writes one `metadata_set` or `metadata_unset` audit row with
whole-object `before`/`after` snapshots and the resolved actor, atomically with
the edit. An absent metadata object is represented as `{}` in the snapshots;
removing its last key leaves `{}`. Setting an identical string or removing an
absent key is a no-op with no audit row. Other keys and numeric precision survive.
Dates, entries, amounts, currency, references, and creation attribution stay
unchanged. Tagging the original charge preserves its age and payment matching
in `report aging --filter-metadata status=disputed`.

JSON envelopes use `txn.meta.set` / `txn.meta.unset` and return
`data: {"id": 42, "metadata": "{...}", "changed": true}`. `metadata` is the stored
JSON text, or `null` for a no-op on a transaction without metadata. Table mode
prints the transaction ID and resulting metadata. Use `txn show` to inspect it.

### Reverse a mistaken transaction

```sh
bk --company personal txn reverse 42 --date 2026-11-19 --reference VOID-42
# Retry with the same explicit reference safely returns the existing reversal.
bk --company personal txn reverse 42 --reference VOID-42
```

`txn reverse` (alias `txn void`) posts a mirror of every entry, preserving
accounts, amounts, memos, tax categories and currency. The date defaults to
today and must be on or after the original date. Use `--description TEXT` to
override `Reversal of #ID: ORIGINAL DESCRIPTION`. The default reference is
`reversal-of-ID`. Repeating the command without an explicit reference, or
with a different reference, rejects an already reversed transaction.

The original gains `reversed_by` metadata and the new posting carries
`reverses`, both numeric transaction IDs. `txn show` and `txn list` display
these links. The original's entries and metadata remain intact apart from
that added link; transaction metadata and attachments are not copied to the
reversal. An atomic `reverse` audit record captures the actor, original
metadata, new link and reversal reference (`bk audit log --action reverse`).

A transaction with any reconciled clearing cannot be reversed. Other
clearings remain on the original, and the reversal starts uncleared. In
reconciliation, the original keeps its cleared/remaining amounts and the
reversal appears separately as an outstanding opposite posting from its
own date. Reversal does not automatically clear or reconcile either side.

### Period comparisons and what changed

Ask “why is spending up?”, “how does this quarter compare to last year?” or
“what changed in this account?” through the CLI or MCP `report` tool:

```sh
bk --json --company acme report trend --metric spending --from 2026-10-01 --to 2026-10-31 --compare previous
bk --json --company acme report income-statement --from 2026-07-01 --to 2026-09-30 --compare year-ago --explain-change 5000
```

Comparisons include current/prior amounts, changes, nullable percentages, ranked
movers, a plain-language summary and optional Desktop register links. Account
explanations rank the transactions and recorded payees contributing to the
difference. Cash flow and balances support the same options. Mixed currencies
require `--currency`; currencies are never added together or silently converted.
See [comparison examples and semantics](docs/comparisons.md).

### CSV Bank Statement and JSON Bulk Import

`bk txn import` accepts CSV, JSON, OFX, and QFX files. File extensions select
the input format automatically. For stdin, set `--input-format csv|json|ofx`;
`--file -` and an omitted `--file` both read stdin. The legacy local `--format`
alias still selects the input format. Use global `--json` for JSON responses.

CSV imports require existing `--account` and `--suspense` accounts. The default
headers are `date,description,amount`, with an optional `reference` column:

```csv
date,description,amount,reference
2026-03-15,"Coffee, shop",-12.34,bank-1001
2026-03-16,Customer deposit,250.00,bank-1002
```

```sh
bk --company personal txn import --file checking.csv --account 1000 --suspense 9000

# Map bank-specific headers, dates, currency, and positive amounts as spending
bk --company personal txn import --file card.csv --account 2000 --suspense 9000 \
  --date-col Posted --description-col Payee --amount-col Value --reference-col ID \
  --date-format '%m/%d/%Y' --positive-amount outflow --currency USD --dry-run
```

Amounts are signed **major currency units**, without currency symbols or
thousands separators. Positive amounts default to inflows (debit bank, credit
suspense); negative amounts are outflows. `--positive-amount outflow` reverses
that convention. Currency defaults to `--currency`, then `BEANKEEPER_CURRENCY`,
then USD. Zero amounts, excess precision, missing accounts, invalid dates, and
missing or duplicate headers are rejected. CSV descriptions support standard
quoting, including commas and newlines.

JSON bulk imports accept an array of transactions. Each object supplies its
own balanced entries, so `--account` and `--suspense` are unnecessary:

```json
[
  {
    "date": "2026-03-15",
    "description": "Office supplies",
    "currency": "USD",
    "reference": "receipt-1001",
    "metadata": {"vendor": "Stationer"},
    "entries": [
      {"account_code": "5000", "direction": "debit", "amount": "12.34", "memo": "Paper", "tax_category": "supplies"},
      {"account_code": "1000", "direction": "credit", "amount": "12.34"}
    ]
  }
]
```

```sh
bk --company acme txn import --file journals.json
cat journals.json | bk --json --company acme txn import --input-format json --dry-run
```

`date`, `description`, and `entries` are required. `currency` overrides the
command's default. Entry amounts are positive major units, just like `txn post`.
Use strings for decimal amounts to preserve exact precision; integer JSON
numbers also mean major units (`10` means ten dollars in USD). Fractional JSON
numbers are rejected. Entry directions are `debit` or `credit`; optional `memo`
and `tax_category` values are preserved, with account tax defaults applied when
omitted. Metadata must be an object. Unknown transaction and entry fields are
rejected. This bulk posting schema differs from the full-database export schema.

**CSV and JSON batches are atomic.** Any parse, validation, or posting error
fails with no new transactions or entries committed. `--dry-run` exercises the
same posting and duplicate checks, including duplicates inside the batch, then
rolls everything back. `--on-conflict skip` is the default; `error` rejects the
entire batch on a duplicate. Only `skip` and `error` are accepted.

References are trimmed and stored verbatim, with uniqueness scoped to the
company. CSV uses its `reference` column automatically (or `--reference-col`);
blank CSV references and missing/null JSON references use a deterministic
`csv:sha256:<hash>:<n>` or `json:sha256:<hash>:<n>` reference. The hash covers
normalized posting data, including accounts and currency; `n` is the occurrence
of that content among unreferenced rows in the batch, starting at 1. Identical
unreferenced rows in one file are all posted. Re-importing the same or an
overlapping file skips matching occurrences; a file with one more identical row
posts only the additional occurrence. File names and unrelated rows do not
affect these references. Duplicate explicit references still skip or error
according to `--on-conflict`. Explicit blank JSON references are rejected.

### Bank Import Categorization Rules

Save recurring description-to-account mappings once, then apply them automatically
to future CSV and OFX/QFX imports:

```sh
bk --company personal rule create --contains Starbucks --account 5000 --priority 10
bk --company personal rule list --json
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000
bk --company personal rule delete rule_01h455vb4pex5vsknk084sn02q --force
```

Use the rule ID returned by `create` or `list` when deleting a rule. Rules belong
to one company and select an existing account in that company. Matching trims
the pattern, lowercases it and the imported description using Unicode lowercase,
then checks for a literal substring. `%` and `_` have no special meaning. CSV
uses its description column; OFX uses the combined name and memo.

The first matching rule wins: higher `--priority` values run first (default `0`),
then ascending rule ID breaks ties. The selected account replaces only suspense:
an outflow debits it, and an inflow credits it. Unmatched rows keep `--suspense`,
which remains required. Destination account tax defaults apply. A rule selecting
the import's bank account is rejected. JSON journal batches keep their explicit
entries.

Rules apply during `--dry-run` too. Creating or deleting rules does not change
existing postings or their references. CSV generated references are calculated
before rule selection, so rule changes do not duplicate previously imported
rows. To change a mapping, delete its rule and create a replacement. These
operations are audited (`--entity rule`, actions `create` and `rule-delete`).
Delete referencing rules before deleting an account. Rule commands support
table, JSON and CSV output; they are available through the CLI, with no new MCP
tools.

### OFX / QFX Bank Statement Import

Import transactions from bank and credit card statements exported in OFX format.
Each OFX transaction becomes a balanced double-entry against a user-specified
bank account and the matching rule's account, or suspense/clearing when no rule
matches:

```sh
# Import an OFX statement -- format is auto-detected from the file extension
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000

# Preview what would be imported without posting
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000 --dry-run

# Import from stdin with explicit format
cat statement.qfx | bk --company personal txn import --file - --input-format ofx --account 1000 --suspense 9000
```

**Deduplication is automatic.** Each OFX transaction's unique `FITID` is stored as an idempotency reference, so re-importing the same file skips already-posted transactions. Use `--on-conflict error` to fail the whole import if any transaction already exists:

```sh
# Default is --on-conflict skip
Imported 47 transactions, skipped 3 duplicates.
```

Supports banking (`BANKMSGSRSV1`), credit card (`CREDITCARDMSGSRSV1`), and investment account (`INVSTMTMSGSRSV1`) OFX message sets. Investment statements extract the banking transactions (`INVBANKTRAN/STMTTRN`) from brokerage cash activity.

| OFX Field | Beankeeper Field | Notes |
|-----------|-----------------|-------|
| `DTPOSTED` | Transaction date | Formatted as YYYY-MM-DD |
| `NAME` + `MEMO` | Description | Joined with ` - ` if both present |
| `TRNAMT` (positive) | Debit bank, credit suspense | Inflows |
| `TRNAMT` (negative) | Debit suspense, credit bank | Outflows |
| `FITID` | Reference (idempotency key) | Namespaced as `ofx:<currency>:<account_id>:<fit_id>` |
| `TRNTYPE` | Metadata | Stored as `{"ofx_type": "CHECK"}` |
| `CURDEF` | Currency | Must be a supported currency |

### Document Attachments

Link source documents (receipts, invoices, statements) to transactions. Files are stored in a content-addressed directory alongside the database, with SHA-256 integrity verification:

```sh
# Attach a receipt to an existing transaction
bk --company personal txn attach 1 receipt.pdf --type receipt

# View transaction with attachments
bk --company personal txn show 1
```

Supported document types: `receipt`, `invoice`, `statement`, `contract`, `other`.

### Tax Categories

Tag entries with free-form tax categories that map to Schedule C lines (or any tax form). Categories can be set per-entry or inherited from an account default:

```sh
# Set a default tax category on an account
bk --company personal account create 5100 "Meals" --type expense \
  --default-tax-category "sched-c:24b"

# Override per-entry at posting time
bk --company personal txn post -d "Client lunch" \
  --debit 5100:25 --credit 1000:25 \
  --tax 5100=sched-c:24b

# View categorised entries
bk --company personal txn show 1

# Summarise by tax category for a tax year
bk --company personal report tax-summary --from 2026-01-01 --to 2026-12-31
```

Schedule C and vendor payment summaries support table, JSON, and CSV output:

```sh
bk --company personal report schedule-c --to 2026-12-31 --output-format csv
bk --company personal report 1099 --to 2026-12-31 --threshold 600 --json
```

Use canonical `sched-c:<line>` tags and transaction `--meta 'payee=Name'`
annotations. Both reports share `tax-summary` date and currency rules. Schedule C
keeps unmapped tags and untagged income/expense activity visible; vendor totals
use each annotated transaction's full debit total. These are book summaries,
not tax advice or filed forms. See [tax export conventions and supported Schedule C lines](docs/tax-exports.md).

Resolution order: explicit `--tax` flag > account `--default-tax-category` > none. Categories are free-form strings -- no tax-year specifics are baked into the schema.

### Querying the Ledger

`bk txn list` (aliased as `bk txn search`) supports rich filtering so agents and scripts get back precisely the data they need without dumping the entire ledger:

```sh
# Search transactions by description
bk --company personal txn list --description "AWS" --from 2026-01-01 --json

# Find large expenses (amounts are in dollars, not cents)
bk --company personal txn list --account 5000 --amount-gt 500 --json

# Count matching transactions without fetching them
bk --company personal txn search --description "payroll" --count --json
# -> {"ok": true, "meta": {...}, "data": {"count": 12}}

# Filter by tax category, direction, currency, reference, or metadata
bk --company personal txn list --tax-category "sched-c:24b" --direction debit --json
bk --company personal txn list --currency MXN --json
bk --company personal txn list --reference "chase-2026-03-15-001" --json
bk --company personal txn list --metadata "vendor" --json

# Exact match on structured metadata keys (repeatable, AND-ed; --meta is an alias)
bk --company personal txn list --filter-metadata vendor=AWS --filter-metadata project=apollo --json
```

Transaction metadata is always stored as a JSON object. Free text from `-m` becomes `{"memo": "..."}`, a JSON object passes through unchanged, and `--meta key=value` adds top-level string keys:

```bash
bk --company personal txn post -d "AWS bill" --debit 5000:120 --credit 1000:120 \
  -m "Q1 cloud" --meta vendor=AWS --meta project=apollo
# metadata: {"memo":"Q1 cloud","vendor":"AWS","project":"apollo"}
```

`--filter-metadata KEY=VALUE` works the same way on every command that reads entries (`txn list`, `report ledger`, `report reconciliation`, `report aging`): it matches a top-level key of the JSON metadata, comparing JSON strings exactly (case-sensitive) and numbers/booleans by their JSON text (`qty=3` matches `{"qty":3}`). `null`, arrays, objects, missing keys and non-JSON metadata never match, and repeated filters are AND-ed. Databases created before schema v9 have any free-text metadata rewritten to `{"memo": "..."}` automatically on first open; this one-off format migration is not recorded in the audit log.

All filters can be combined. Amount filters use major units and match only
transactions in the selected currency. A company with multiple transaction
currencies requires `--currency` (MCP: `currency`). With no selection,
`bk` uses the company's sole transaction currency, or USD if the company has
no transactions. `BEANKEEPER_CURRENCY` does not select a currency for this query.

`bk account list` can include balances in a single query:

```sh
# List expense accounts with their debit/credit totals
bk --company personal account list --type expense --with-balances --json

# Search accounts by name, with balances for a period
bk --company personal account list --name "Cash" --with-balances --to 2026-06-30 --json
```

### Output Formats

Every command supports `--format table` (default), `--format json`, and `--format csv`. The output `--format`/`--json` options work before or after the subcommand (e.g. `bk txn list --format csv`). Import and export use `--input-format` and `--output-format` for their file formats; their legacy local `--format` aliases remain supported. Use `--json` as shorthand:

```sh
# Pipe JSON to jq (data is inside the envelope's "data" field)
bk --company personal --json report trial-balance | jq '.data.accounts[] | select(.type == "asset")'

# CSV for spreadsheets
bk --company personal --format csv txn list > transactions.csv

# Export all companies, accounts, and transactions as standalone JSON (no envelope)
bk export --output-format json > accounting-export.json

# Machine-readable for scripts (exit codes: 0=ok, 3=validation error)
bk txn reconcile --json || echo "Orphaned correlations found"
```

> **Note:** Global flags like `--company` are ignored by commands that always operate database-wide, such as `export`, `reconcile`, `verify`, and `init`.

### General Ledger

`bk report ledger` prints an account statement: for each account an opening balance as of the day before `--from`, every entry in date / transaction / entry order with a running balance, and a closing balance. Balances are shown on the account's normal side (debit-normal for assets and expenses, credit-normal for liabilities, equity and revenue).

```sh
# Every account with activity
bk --company personal report ledger

# Cash statement for Q1, starting from the balance at 2025-12-31
bk --company personal report ledger --account 1000 --from 2026-01-01 --to 2026-03-31

# Pick a currency when the company books in more than one
bk --company personal report ledger --account 1000 --currency EUR

# Only entries tagged project=apollo (opening and running balances too)
bk --company personal report ledger --account 5000 --filter-metadata project=apollo

# Flat CSV: one row per entry plus opening/closing rows per account
bk --company personal --format csv report ledger > ledger.csv
```

Without `--currency`, the report uses the company's only transaction currency and fails with a validation error if there are several. With `--filter-metadata`, the opening balance, running balances and closing balance count matching entries only, so the statement stays internally consistent; the table says the filter is active and JSON lists it under `metadata_filters`.

### Bank Reconciliation

Clear entries against statements, in full or in part. Each clearing records an amount (default: whatever remains uncleared), a date (`--date`, default today) and an optional statement id (`--statement`), so an item can be paid across several statements:

```sh
# Mark the bank-leg of a transaction as cleared
bk --company personal txn clear 42 --entry 5 --status cleared

# Mark as reconciled (finalized)
bk --company personal txn clear 42 --entry 5 --status reconciled

# Clear part of an item, picking the entry by transaction reference + account
bk --company agency txn clear --reference POL-123 --account 1200 \
  --amount 40 --date 2026-03-31 --statement ACME-2026-03

# Clear a whole statement from stdin or a file: one `reference[,amount]` per line
printf 'POL-123,40\nPOL-124\n' | bk --json --company agency txn clear \
  --from-file - --account 1200 --date 2026-03-31 --statement ACME-2026-03
```

Batches are atomic (all lines or none) and report what was cleared and what was skipped. Each entry is cleared at most once per statement, so re-running a statement batch is a no-op. Clearing more than the remaining amount is rejected, and so is a `--date` before the transaction's date. Every clearing is recorded in the audit trail.

Reconciling records a **reconciled date**: with `--status reconciled`, `--date` (default today) is the day the clearings were locked in. Promoting a statement's clearings keeps their cleared date and sets the reconciled date (it may not be before the cleared date); a clearing created directly as reconciled gets the same cleared and reconciled date. Reconciled clearings are locked: `--status cleared` on them is an error. To redo one, remove it with `--status uncleared` (plus `--statement`), then clear it again.

An entry's status is derived from its clearings: `cleared` once fully cleared, `reconciled` once fully cleared by reconciled clearings, `uncleared` otherwise (including partially cleared items). Status indicators (`*C*` for cleared, `*R*` for reconciled) appear in `bk txn show` and `bk txn list` table views. In JSON mode, every entry includes a `status` field. `bk txn show` also lists each entry's id, cleared and remaining amounts, and individual clearings (amount, cleared date, reconciled date, statement, status).

### Financial Summary

Ask the agent "How am I doing?" It can call `financial_summary` with a `company`,
or run `bk --company personal summary --json`, to get plain-language highlights,
standing balances, income/spending comparisons, spending movers and attention
counts with chartable amounts and an optional Desktop target. No app is required.
The default is this month to date versus the full previous calendar month.
`--from`, `--to`, `--compare-from`, `--compare-to` and `--currency` select the scope.

The CLI and MCP tool return the same full envelope. Overdue status is explicitly
unavailable because the ledger has no payment due dates; older open items are
reported separately. See the [summary contract and limitations](docs/financial-summary.md).

### Historical Trends

Track monthly spending by category or net worth across time:

```sh
bk --company personal report trend --metric spending --from 2026-01-01 --to 2026-12-31
bk --company personal report trend --metric net-worth --from 2026-01-01 --to 2026-12-31
bk --company personal --format csv report trend --metric spending --period quarter --to 2026-12-31
```

`--period month|quarter|year` defaults to `month`. Dates are inclusive; `--to`
defaults to today and `--from` to January 1 of the end date's year. Calendar
periods include inactive periods and clip their first and last boundaries to
these dates. Reports support up to 120 periods.

Spending categories are expense accounts, with debits minus credits in each
period, followed by Total Spending. Refunds reduce spending and can make it
negative. This measures booked expenses, including unpaid expenses, rather than
cash payments. All expense accounts appear, including those with zero activity.
Net worth shows Total Assets, Total Liabilities, and Net Worth (assets minus
liabilities) at each period end. These balances include all earlier history,
even entries before `--from`; inactive periods carry balances forward. Values
reflect ledger book values. Period-end balances are not summed across time.

Use `--currency CODE` for a company with multiple transaction currencies; otherwise
the currency is inferred, with USD for an empty company. Table output presents
major units. JSON (`report.trend`) provides date-labelled `columns` and `series`
with integer minor-unit `amounts` and decimal-string `amounts_display`. CSV has
`code,name,<period labels>` columns and integer minor-unit amounts. The MCP
`report` tool accepts `kind: "trend"` with the same metric, dates, period and
currency options.

### Agent-drawn charts

Use `--json --chart` with `summary`, `report trend`, `report income-statement`,
`report cash-flow`, or period/comparison trial balances to add versioned
`data.chart` data. MCP `financial_summary` and `report` accept `chart: true`.
Charts carry exact major-unit decimal strings, currency, labels, account codes
and optional Desktop detail targets. They default to eight categories/series
plus Other; `--chart-all` / `chart_all: true` returns all. Existing report fields
are retained. See the [schema and worked example](docs/chart-data.md).

### Period-over-Period Reports

The income statement and trial balance can be broken into columns or compared with an earlier range. Both options need `--from` and `--to`, and they cannot be combined:

```sh
# One column per month (or quarter / year) plus a Total column and a Net Income row per month
bk --company personal report income-statement --from 2026-01-01 --to 2026-06-30 --period month
bk --company personal report trial-balance --from 2026-01-01 --to 2026-12-31 --period quarter

# Compare with the same range last year, or with the immediately preceding range
bk --company personal report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-year
bk --company personal report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-period
```

Each cell is the account's net activity on its normal side. Periods are labelled `2026-01`, `2026-Q1` or `2026`, and the first and last periods are clipped to `--from`/`--to`. Comparisons add `Change` and `Change %` columns (`n/a` when the prior amount is 0). Table, JSON (`report.income-statement.periods`, `report.income-statement.compare`, ...) and CSV output are supported.

`bk report reconciliation` uses those clearings to reconcile an account against a statement. It reports the book balance (all entries), the cleared balance, and the reconciled balance on the account's normal side, lists the outstanding entries (including partially cleared ones, with their reference, metadata, cleared and remaining amounts) with their total (book - cleared = uncleared total), and, given `--statement-balance`, shows the difference (statement - cleared) and whether the account is reconciled:

```sh
bk --company personal report reconciliation --account 1000 --to 2026-03-31 --statement-balance 4210.33

# Credit cards are credit-normal: a positive balance is the amount owed
bk --company personal report reconciliation --account 2100 --statement-balance 512.80

# Check one statement's clearings against its total
bk --company agency report reconciliation --account 1200 --statement ACME-2026-03 --statement-balance 175

# Outstanding receivables for one carrier, grouped by policy, with subtotals
bk --company agency report reconciliation --account 1200 \
  --filter-metadata carrier=Acme --group-by-metadata policy
```

`--to` is point-in-time: a clearing counts as cleared when its cleared date is on or before `--to`, and as reconciled only when it was also reconciled on or before `--to`. An item cleared later is still outstanding as of an earlier date, and a clearing reconciled later shows as cleared but not reconciled. `--statement ID` limits the cleared balance to that statement's clearings. `--filter-metadata KEY=VALUE` (repeatable, AND-ed) and `--group-by-metadata KEY` match top-level keys of JSON transaction metadata (anything else has no keys; ungrouped items go under `(unassigned)`). `--currency CODE` filters to one currency; when omitted, the company's single transaction currency is used (an error if it has several). CSV output is the outstanding entries, then group subtotals, then `summary` rows (see `docs/beankeeper/references/commands.md`).

### Budgeting and Variance Analysis

Set spending targets per account, then compare budgeted amounts to actual activity:

```sh
# Set an annual budget (evenly distributed across 12 months)
bk --company personal budget set 5000 --year 2026 --annual 14400 --notes "Monthly rent"

# Set a single month's budget
bk --company personal budget set 4000 --year 2026 --month 12 --amount 8000

# List all budgets for a year
bk --company personal budget list --year 2026

# Budget vs. actual for the full year
bk --company personal report budget-variance --year 2026

# Q1 expenses only
bk --company personal report budget-variance --year 2026 --from 1 --to 3 --type expense

# Include accounts that have actual spending but no budget set
bk --company personal report budget-variance --year 2026 --include-unbudgeted
```

The report currency comes from `--currency` (or `BEANKEEPER_CURRENCY`); when neither is set it is inferred from the company's transactions, as for the other reports.

Variance logic:
- **Expense accounts**: variance = budget - actual (positive = favorable, underspent)
- **Revenue accounts**: variance = actual - budget (positive = favorable, exceeded target)
- Status labels: `FAV`, `UNFAV`, `ON BUDGET`

Annual budgets are split exactly across 12 months -- any remainder cents are distributed to the earliest months so the total is precise.

### Receivables and Payables Aging

`bk report aging` ages the open balance of an asset (receivables) or liability (payables) account:

```sh
# Accounts receivable as of today (buckets: Current, 1-30, 31-60, 61-90, 90+)
bk --company personal report aging --account 1100

# Accounts payable as of quarter end, with custom buckets
bk --company personal report aging --account 2000 --as-of 2026-03-31 --buckets 15,45

# Per customer, using JSON transaction metadata like -m '{"customer":"Globex"}',
# listing every open invoice under its customer
bk --company personal report aging --account 1100 --group-by customer --detail

# Apply payments to the invoice they name (specific identification)
bk --company personal txn post -d "Invoice 102" -r INV-102 --debit 1100:300 --credit 4000:300
bk --company personal txn post -d "Payment" --debit 1000:100 --credit 1100:100 \
  -m '{"customer":"Globex","applies_to":"INV-102"}'
bk --company personal report aging --account 1100 --method specific

# Several receivable sub-ledgers at once, or a control account and its children
bk --company personal report aging --account 1110 --account 1120
bk --company personal report aging --account 1100 --include-children

# Disputed receivables only, still at their original age
bk --company personal report aging --account 1100 --filter-metadata status=disputed
```

The ledger has no invoice or due-date model, so the report works from transactions: entries on the account are netted per transaction, each net increase is a **charge** and each net decrease is a **settlement**, and ages are measured from **transaction dates**.

- **Sign convention.** An asset account (receivables) ages debit-side charges and applies credit-side settlements; a liability account (payables) ages credit-side charges and applies debit-side settlements. The same command works for AR and AP.
- **Matching (`--method`).** With `fifo` (default) every settlement goes to the oldest open charges. With `specific`, a settlement whose metadata links to a charge is applied to that charge first; any excess, and unlinked settlements, fall back to FIFO. The link is the metadata key `applies_to` (change it with `--link-key`), holding the charge's reference (`txn post -r`; references are unique per company, so the payment carries the link rather than sharing the reference) or its transaction id, or an array of these. A link to a missing transaction, to one that is not a charge on this account, to a charge dated later or after `--as-of`, or to another counterparty's charge is reported as a warning and under `unmatched_links` in JSON, and the settlement is applied FIFO.
- **Credits and reversals.** Credit notes, write-offs and invoice reversals are settlements like payments; with `--method specific`, link one to its invoice to cancel that invoice. Refunds paid out and payment chargebacks are charges (aged from their own date) that first consume any unapplied credit. Payables mirror this: vendor credits are settlements, vendor refunds are charges.
- **Net credit.** Settlements beyond the open charges are an *unapplied credit*. A counterparty (or account) in net credit shows a negative `Net` marked `CR` in the table, a negative `net` in CSV, and `"net_credit": true` in JSON.
- **As-of.** Only transactions dated on or before `--as-of` count, so rerunning for a past period end gives the same answer after later-dated payments are posted.
- **Counterparties.** `--group-by-metadata KEY` (alias `--group-by`) groups by a key of JSON object metadata; missing keys and non-JSON or non-object metadata go to `(unassigned)`. Matching runs per counterparty.
- **Several accounts.** Repeat `--account`, or add `--include-children` to also age every account whose code starts with an account's code minus trailing zeros (`1100` takes in `1110`, `1120`, ...). Each account is matched on its own, exactly as in a single-account run, so the totals equal the sum of single-account runs and a payment never settles a charge on another account (a link to one is reported as `other_account`). All accounts must be receivables or all payables. With `--group-by`, counterparties are grouped within each account, followed by an account subtotal; `--group-across-accounts` combines each counterparty's rows across accounts instead. JSON always has both: per-account sections in `accounts[]` and the combined `groups[]`.
- **Metadata filter.** `--filter-metadata KEY=VALUE` (repeatable) keeps only open charges whose metadata matches. Matching runs on every entry first, so a filtered charge keeps its original age and shows what is still open after its payments. That lets disputed receivables stay in the AR account, tagged `status=disputed`, without resetting their age. Unapplied credit is not shown under a filter.

Without a filter the totals always reconcile: total open - unapplied credit = account balance as of the date (the combined balance with several accounts). If the company uses more than one currency, pass `--currency`.

### Cash Flow Statement

`bk report cash-flow` produces a direct-method cash flow statement: the net cash movement of each transaction is attributed to its non-cash counter-accounts and grouped into Operating (revenue/expense), Investing (non-cash assets) and Financing (liabilities/equity), followed by the net change, opening cash and closing cash, with a check that opening + net change = closing.

```sh
# Default cash accounts
bk --company personal report cash-flow --from 2026-01-01 --to 2026-12-31

# Choose the cash accounts explicitly (repeatable)
bk --company personal report cash-flow --from 2026-01-01 --to 2026-03-31 --cash-account 1000

# Multi-currency ledgers must pick one currency
bk --company personal report cash-flow --from 2026-01-01 --to 2026-12-31 --currency EUR
```

Without `--cash-account`, every **asset** account whose name contains `cash`, `bank`, `checking` or `savings` (case-insensitive) is treated as cash -- this matches the bundled templates (`Operating Cash`, `Checking Account`, `Savings Account`). A code range is deliberately not used: `1100` is `Savings Account` in the personal template but `Accounts Receivable` in the business ones. If no account qualifies, the command exits with a validation error (exit code 3) asking for `--cash-account`. Transfers between two cash accounts net to zero and are excluded.

### JSON Envelope

JSON command responses follow a uniform envelope contract for reliable programmatic consumption by agents and scripts. Successful JSON exports instead write the standalone `beankeeper_export` object described above:

**Success:**
```json
{
  "ok": true,
  "meta": {
    "command": "company.list",
    "timestamp": "2026-03-18T15:30:00Z"
  },
  "data": [{"slug": "acme", "name": "Acme Corp"}]
}
```

**Error:**
```json
{
  "ok": false,
  "meta": {
    "command": "account.show",
    "company": "acme",
    "timestamp": "2026-03-18T15:30:00Z"
  },
  "error": {
    "code": "NOT_FOUND",
    "message": "account '9999' not found"
  }
}
```

For enveloped responses, the `meta` block is present on both success and error. The `meta.command` field uses dot notation (`company.list`, `txn.post`, `report.trial-balance`). The `meta.company` field is present when the command operates on a specific company. Error codes are: `USAGE`, `VALIDATION`, `DATABASE`, `NOT_FOUND`, `UNAUTHORIZED`, `FORBIDDEN`, `IO`, `GENERAL`.

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `BEANKEEPER_DB` | Database file path (default: `./beankeeper.db`) |
| `BEANKEEPER_COMPANY` | Default company slug (avoids `--company` on every command) |
| `BEANKEEPER_CURRENCY` | Default currency code for commands such as posting, importing and `report budget-variance` |
| `BEANKEEPER_PASSPHRASE_CMD` | Command to obtain encryption passphrase |
| `BK_ACTOR` | Named poster and audit actor |
| `BK_AGENT_TOKEN` | Optional local agent attestation; overridden by `--agent-token` |
| `NO_COLOR` | Disable colored output |

### Backup and restore

Use `backup` for a consistent snapshot of the entire ledger, including committed
writes from active connections. Encryption stays enabled with the original
passphrase. Both commands verify the staged file before publishing it.

```sh
bk --db books.db --passphrase-file /secure/books.key backup --output ledger.bk-backup
bk restore --input ledger.bk-backup --db /data/restored.db --passphrase-file /secure/books.key
bk --db /data/restored.db --passphrase-file /secure/books.key verify
```

Omit the passphrase option for unencrypted ledgers. `backup --output` and
`restore --input` are required. Restore also requires an explicit `--db` flag,
even when `BEANKEEPER_DB` is configured. Global `--company` is ignored.
JSON success envelopes report `path`, `bytes`, `schema_version`, and `encrypted`.

Existing destinations require `--force`. Close all connections to a destination
before replacing it; SQLite `-wal`, `-shm`, or `-journal` files cause rejection.
Source and destination must differ, including symlink and hard-link aliases.
Destinations must be regular files or new paths. Failed verification leaves an
existing destination untouched and removes temporary files. On Unix, published
files have owner-only permissions (0600).

Snapshots are standalone SQLCipher/SQLite databases, with no archive wrapper.
Restore accepts only a valid current-schema ledger and runs the same accounting,
audit-chain, foreign-key, and integrity checks as `bk verify`, without migrating
it. The file carries no proof that `backup` created it; any complete ledger
passing those checks is accepted. To restore an older snapshot, use its matching
CLI version first. A lost
passphrase cannot be recovered. Keep the backup and passphrase separately.

The snapshot includes all database tables: accounting, identity root, token
registry, audit history, budgets, rules, schedules, investments, spend limits, proposals, and attachment
records. Receipt and document **files** are stored outside SQLite in the
`attachments/` directory beside the source database. Back up that directory
separately and preserve the paths referenced by attachment records when moving
machines. External attachment storage changes are outside these commands.
`bk export` remains an accounting JSON/CSV export, not a recovery backup.
Backup and restore are CLI commands and are not exposed over MCP.

### Encryption

```sh
# Create an encrypted database
bk init --encrypt

# Provide passphrase via command (for automation)
export BEANKEEPER_PASSPHRASE_CMD="op read op://Vault/beankeeper/passphrase"
bk --company personal report trial-balance
```

## Agent Skill

The [`docs/beankeeper/`](docs/beankeeper/) directory contains a portable agent skill that teaches any LLM agent how to operate `bk`. It includes:

- **`SKILL.md`** -- core concepts, command overview, essential workflows
- **`references/commands.md`** -- full CLI reference with all flags and examples
- **`references/json-api.md`** -- JSON envelope format and response schemas
- **`references/accounting.md`** -- double-entry accounting fundamentals
- **`references/workflows.md`** -- multi-step recipes for common bookkeeping tasks

Point your agent framework's skill loader at the `docs/beankeeper/` directory, or copy it into your plugin/agent configuration.

## Library

The CLI is built on the [`beankeeper`](https://crates.io/crates/beankeeper) library crate, which you can embed directly in Rust projects that need accounting primitives without the CLI layer.

```sh
cargo add beankeeper
```

### Quick Start

```rust
use beankeeper::prelude::*;

let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let revenue = Account::new(AccountCode::new("4000").unwrap(), "Revenue", AccountType::Revenue);

let txn = JournalEntry::new(
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
        "Cash sale",
        Currency::USD,
    )
    .debit(&cash, Amount::new(50_00))
    .unwrap()
    .credit(&revenue, Amount::new(50_00))
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.description(), "Cash sale");
assert_eq!(txn.date(), NaiveDate::from_ymd_opt(2026, 3, 15).unwrap());
```

### Core Concepts

**Correctness by construction** -- `Transaction` can only be created through `JournalEntry::post()`, which enforces the balance invariant. Unbalanced transactions cannot exist as values.

**Exact arithmetic** -- all monetary values use `i128` minor-unit representation (cents, pence, centavos, yen). No floating-point. Deterministic across platforms.

**The accounting equation**:
```text
Assets + Expenses = Liabilities + Equity + Revenue
 (Debit normal)       (Credit normal)
```

### Multi-leg Transactions with Memos

```rust
use beankeeper::prelude::*;

let salary = Account::new(AccountCode::new("5000").unwrap(), "Salary", AccountType::Expense);
let cash = Account::new(AccountCode::new("1000").unwrap(), "Cash", AccountType::Asset);
let tax = Account::new(AccountCode::new("2200").unwrap(), "Tax Withheld", AccountType::Liability);

let txn = JournalEntry::new(
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
        "March Paycheck",
        Currency::USD,
    )
    .debit_with_memo(&salary, Amount::new(5000_00), "Gross salary")
    .unwrap()
    .credit_with_memo(&cash, Amount::new(3800_00), "Net pay")
    .unwrap()
    .credit_with_memo(&tax, Amount::new(1200_00), "Federal withholding")
    .unwrap()
    .post()
    .unwrap();

assert_eq!(txn.entries()[0].memo(), Some("Gross salary"));
```

### Multi-Currency Support

Ten ISO 4217 currencies with correct minor-unit precision:

| Currency | Code | Minor Units |
|----------|------|-------------|
| US Dollar | `USD` | 2 (cents) |
| Euro | `EUR` | 2 (cents) |
| British Pound | `GBP` | 2 (pence) |
| Japanese Yen | `JPY` | 0 |
| Swiss Franc | `CHF` | 2 |
| Canadian Dollar | `CAD` | 2 |
| Australian Dollar | `AUD` | 2 |
| Mexican Peso | `MXN` | 2 (centavos) |
| Bahraini Dinar | `BHD` | 3 |
| Kuwaiti Dinar | `KWD` | 3 |

Each journal chooses its currency at construction. Debit and credit amounts are minor units in that currency. Adding a prebuilt entry with a different currency returns `EntryError::CurrencyMismatch`. Posting validates the count and balance, then retains the currency and balanced total in the resulting `Transaction`.

Reports (`trial-balance`, `balance`, `income-statement`, `balance-sheet`, `tax-summary`, `schedule-c`, `1099`, `budget-variance`) are always computed in a single currency and never add amounts across currencies. Pass `--currency CODE` to choose it; otherwise it is inferred from the company's transactions (USD when there are none). If a company has transactions in more than one currency, a report without `--currency` fails with a validation error listing the currencies found.

### Error Handling

All fallible operations return `Result` with descriptive variants. The top-level `BeanError` aggregates all domain errors for ergonomic `?` usage:

```rust
use beankeeper::prelude::*;

fn record_sale(ledger: &mut Ledger) -> Result<(), BeanError> {
    let cash = Account::new(AccountCode::new("1000")?, "Cash", AccountType::Asset);
    let revenue = Account::new(AccountCode::new("4000")?, "Revenue", AccountType::Revenue);
    let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
    let txn = JournalEntry::new(today, "Sale", Currency::USD)
        .debit(&cash, Amount::new(50_00))?
        .credit(&revenue, Amount::new(50_00))?
        .post()?;
    ledger.post(txn)?;
    Ok(())
}
```

## Safety and Quality

- `#[deny(unsafe_code)]` -- no unsafe Rust in either crate
- `#[deny(clippy::unwrap_used)]` and `#[deny(clippy::expect_used)]` -- proper error handling everywhere
- `#[warn(clippy::pedantic)]` -- pedantic linting enabled
- Library depends only on `chrono`, `sha2`, and `data-encoding` -- minimal footprint
- Tests covering unit, integration, doctest, and real-world accounting scenarios

Every pull request and push to `main` runs formatting, Clippy with warnings
treated as errors, and the workspace tests (including doctests) on Linux and
macOS using stable Rust. A separate job checks all workspace and test targets
with Rust 1.91.0, the minimum supported version declared by both crates.
Compiled dependencies are cached per platform and toolchain.

Run the quality checks locally before submitting a change:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.91.0 check --workspace --all-targets --locked
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
