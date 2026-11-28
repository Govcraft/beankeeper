# Beankeeper

The accounting runtime for AI agents. Your assistant manages your books -- `bk` makes sure it can't mess them up.

A CLI backed by SQLCipher-encrypted SQLite, with an embeddable Rust library underneath.

See [beankeeper.app/developers](https://beankeeper.app/developers/) for the
product overview, source repositories, and verified release files.
[Beankeeper Desktop](https://beankeeper.app/) adds a background service,
optional local browser views, and AI extension packaging under a separate
[source-available license](https://github.com/Govcraft/beankeeper-desktop/blob/main/LICENSE.md).
The ledger, CLI, and MCP server in this repository remain MIT/Apache-2.0 open source;
you can use them independently of Desktop.

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
when more rows exist. Audit pages default to 50 rows and accept up to 100. For
the next page, pass `meta.pagination.next_offset` to `--offset` with the same
filters. MCP tools expose the same amounts and put pagination in the tool
result’s `_meta.pagination`.

Named error codes (`NOT_FOUND`, `VALIDATION`, `DATABASE`, etc.), `meta.command` in dot notation, consistent field presence. The agent can handle `bk` output reliably without parsing human-readable text or guessing at shapes.

A successful `bk export --output-format json` writes a standalone `{"beankeeper_export": {"version": 1, "companies": [...]}}` object, without `ok`, `meta`, or `data`, even with `--json`. Read its companies with `jq '.beankeeper_export.companies'`.

### The audit trail is tamper-evident

Database triggers reject changes to posted financial fields, entry details, and audit records, including deletion and replacement. Transaction metadata and entry clearance status remain mutable for correlation, reversals, and reconciliation. Supported mutations append an actor, timestamp, and before/after snapshots in the same atomic operation.

Every new posting, including imports and reversals, stores `posted_by` and a `create` audit record containing its immutable transaction and entry details. Set the actor label with `--actor NAME` or `BK_ACTOR` on the CLI or MCP server. Both use the verified agent URI as the actor when a token is present and no label is configured. `bk txn show` displays the poster; JSON transaction output includes `posted_by`. Legacy transactions have an unknown poster (`null` in JSON).

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

With enforcement off, writes without a token can record an unverified client
hint from known runtime environment markers. The hint records only allowlisted
marker names and leaves the actor label independent. See it in `bk audit log`
and MCP `audit_log`. Use `--quiet` to suppress the stderr notice on CLI
commands or when launching a stdio MCP server.

Enforcement defaults to off. Use `bk policy show` and
`bk policy set --enforce on|off` to inspect or change it. Bootstrap by issuing a token with
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
| `ledger/reverse` | Reverse postings and undo eligible audit receipts |
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
Embedded hosts can
[rotate a managed credential atomically and recover by token ID](docs/agent-identity.md#rotate-a-managed-credential-in-an-embedded-host).

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

### Set up books over the agent connection

Bookkeepers can create or initialize books, add accounts, and apply a chart on
their normal connection in either write mode. Direct bookkeepers also post
opening balances and import CSV or OFX/QFX statement contents immediately.
The agent reports completed changes and offers one-step owner undo using the
receipt ID, with an optional Desktop detail path. Statement undo reverses the
postings that import created.

If the owner opted into propose-only, the same tools submit an opening
proposal and one proposal per new statement row for the existing transaction
sign-off. Existing bookkeeper credentials gain this setup access automatically.

[CLI and MCP onboarding commands](docs/beankeeper/references/commands.md#direct-onboarding)

### Direct posting and optional sign-off

The bookkeeper preset posts directly with `bk txn post` or MCP `txn_post`.
Tell the owner what posted and give them the transaction ID. To correct it,
the same bookkeeper can run `bk txn reverse ID` or use `txn_reverse`. Existing
direct bookkeeper credentials already have reversal access. A correction
appends an audited mirror posting and keeps the original in the ledger.

Use `bk --company acme --json audit receipts` or MCP `audit_receipts` to see
what changed. Run `bk --company acme audit undo RECEIPT` or MCP `audit_undo` with
your signed bookkeeper credential to reverse an eligible posting, restore
annotations, or restore an account label. An owner with a signed `ledger/admin`
credential can also archive an account created by a receipt or undo a grouped
setup change. A retry returns the recorded undo and its optional Desktop detail
link. Report the result to the owner where they asked.
[Audit receipt fields and errors](docs/beankeeper/references/json-api.md#audit-receipts)

A credential explicitly issued with `--write-mode propose-only` returns a
pending proposal for a post, reversal, or receipt undo. A separate signed
reviewer applies a pending correction with `bk txn approve` or `txn_approve`.

```sh
bk agent issue --name bookkeeper --public-key "$AGENT_PUBLIC_KEY" --write-mode propose-only
bk --agent-token "$PROPOSER_TOKEN" --company acme txn post \
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

### Existing spend limits

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

## Fixed assets

Register acquisitions, preview and post straight-line book depreciation, and
record disposals with gains or losses. See the [fixed asset guide](docs/fixed-assets.md).

## Expense claims

Stage receipts, confirm expense details, and review reimbursements and company
card expenses. See the [expense claim guide](docs/expense-claims.md).

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
GitHub Release binaries, through `install.sh`, AUR, or a manual download.
Published binaries cover Linux and macOS on x86_64 and ARM64. There is no Windows
CLI build in these releases. The CLI is not published to crates.io, so
`cargo install beankeeper-cli` and `cargo binstall beankeeper-cli` are not
available; build from Git instead. Only the `beankeeper` library is published
on crates.io.

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

## Inventory

Register stock items and record receipts, shipments, returns, and count adjustments with FIFO costing. See [perpetual inventory](docs/inventory.md) for stock accounts, invoice and bill matching, and as-of valuation.

## Vendor payments

Prepare and independently approve domestic vendor payments, then dispatch through a trusted host and reconcile confirmed settlement or return. See [reviewed vendor disbursements](docs/disbursements.md) for the workflow and host adapter contract.

## Connect an AI assistant through MCP

`bk mcp serve` exposes an existing ledger over the Model Context Protocol using
stdio. Initialize the ledger and its companies with the CLI first, then configure
your assistant to launch the server. Each process opens one database and resolves
its passphrase once at startup. Missing databases and incorrect passphrases fail
before the protocol starts.

Both stdio and HTTP choose the recorded actor in this order: `--actor`,
`BK_ACTOR`, verified agent URI, then a sanitized `mcp:<client>` name. MCP uses
`mcp:unknown` when none is available. A verified request also records its signed
`agent_uri` and `token_jti`, including when the server has a configured label.
Find changes with `audit_log` using `actor` for the exact label or `agent` for
the verified URI. The CLI equivalents are `bk audit log --by-actor NAME` and
`bk audit log --agent URI`. Supplying both filters matches both fields.

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

For VAT on GBP invoices and bills, see [configured UK invoice VAT](docs/document-vat.md).
Use the [contractor worksheet](docs/contractor-tax.md) to review actual payments and prepare a restricted 2025 1099-NEC export.

### Available tools

| Tools | Purpose |
| --- | --- |
| `group_income_statement` | Read combined income for selected companies in one ledger. Supply `members`, `currency`, `from`, and `to`. |
| `group_balance_sheet` | Read combined balances for selected companies in one ledger. Supply `members`, `currency`, and `as_of`. |
| `ledger_setup_begin`, `ledger_setup_finish`, `ledger_setup_revoke` | Use a live owner Admin credential to issue or end an encrypted ledger's setup session |
| `ledger_setup_book_create`, `ledger_setup_book_initialize`, `ledger_setup_chart_apply`, `ledger_setup_account_add` | Configure books and accounts on the normal bookkeeper connection or in an opted-in setup session |
| `ledger_setup_book_update` | Record a new profile revision in an active setup session |
| `ledger_setup_opening_post`, `ledger_setup_statement_import` | Post opening balances or import statement contents immediately and return an owner undo receipt |
| `ledger_setup_opening_preview` | Validate a balanced opening journal within an active setup generation |
| `ledger_setup_opening_reserve` | Hold the exact previewed opening journal for the existing approval workflow |
| `ledger_setup_statement_preview` | Freeze the entries parsed from bounded CSV, OFX, or QFX content for review |
| `ledger_setup_statement_commit` | Post the reviewed batch with a separate live approval credential |
| `ledger_setup_statement_receipt` | Read statement provenance and the transaction IDs the batch created |
| `period_close`, `period_reopen`, `period_list` | Close company posting dates, explicitly reopen with a reason, and inspect cutoffs |
| `reconcile_statement` | Match and lock a monthly statement using existing clearings. Provide `company`, `account`, `period_end` and `statement_ending_balance`. Returns an audited undo receipt. |
| `reconcile_list` | List statement receipts and outstanding account months. Provide `company` and optionally filter by `account` or `currency`. |
| `company_list`, `company_show` | Inspect companies |
| `company_create` | Create and audit a company by slug and name, with an optional description |
| `company_delete` | Confirm and audit deletion of a company with no retained data |
| `counterparty_create`, `counterparty_amend`, `counterparty_archive`, `counterparty_associate` | Manage company customer/vendor records and their audited associations to postings |
| `bank_feed_connect`, `bank_feed_sync`, `bank_feed_disconnect` | Administer host-authorized bank and card feeds through the audited workflow |
| `bank_feed_show`, `bank_feed_list`, `bank_feed_review` | Inspect the connection, staged revisions and matching ledger transactions |
| `bank_feed_post`, `bank_feed_link` | Submit a reviewed bank feed revision for posting or attach it to an existing transaction |
| `counterparty_list`, `counterparty_show`, `counterparty_suggest` | Search records, inspect a record, or preview legacy metadata matches |
| `tax_configure` | Configure a company VAT code and effective-dated revision |
| `tax_list`, `tax_report` | Inspect historical VAT rates and reconcile a GBP VAT register against control accounts |
| `disbursement_draft` | Draft an instruction against issued vendor bills |
| `disbursement_preview` | Get the provider destination fingerprint for review |
| `disbursement_approve` | Approve and reserve a frozen instruction |
| `disbursement_revoke` | Revoke approval before dispatch |
| `disbursement_reject` | Reject an instruction before dispatch |
| `disbursement_dispatch` | Send once or query provider evidence for an earlier attempt |
| `disbursement_reconcile` | Post authenticated settlement or return evidence |
| `disbursement_show` | Show a payment instruction and provider evidence |
| `disbursement_list` | List payment instructions |
| `document_show`, `document_print`, `document_list`, `document_allocations`, `document_aging`, `document_proposals` | Inspect issued terms, allocations, due-date aging and document proposals |
| `contractor_tax_review` | Record or revise a reviewed contractor payment against posted evidence |
| `contractor_tax_worksheet`, `contractor_tax_preview` | Run the redacted 2025 worksheet or preview legacy candidates |
| `contractor_tax_w9`, `contractor_tax_export` | Import checked identity details and export a protected local artifact using a signed tax-details grant |
| `document_draft`, `document_amend`, `document_issue`, `document_allocate`, `document_credit`, `document_revoke`, `document_approve`, `document_reject` | Draft, issue, settle and review invoices and bills |
| `project_list`, `project_show`, `project_time`, `project_unbilled`, `project_report` | Read project history, budgets, profitability and unbilled work |
| `project_create`, `project_archive`, `project_record`, `project_approve`, `project_split`, `project_budget`, `project_draft_invoice`, `project_cancel_invoice` | Set up projects, record and approve work, split journal entries, and prepare invoices |
| `account_list`, `account_show`, `account_create`, `account_rename`, `account_classify` | Inspect, create, rename, and classify accounts |
| `txn_post`, `txn_settle_hold`, `txn_reverse`, `txn_recategorize`, `txn_meta_set`, `txn_meta_unset` | Record and correct postings, settle holds, or edit annotations |
| `txn_link` | Record an undoable relationship between two transactions in the same book |
| `txn_list`, `txn_show`, `txn_clear`, `txn_reconcile` | Inspect transactions, clear entries and check intercompany links |
| `txn_propose`, `txn_proposals`, `txn_approve`, `txn_reject` | Submit, inspect and resolve pending proposals |
| `security_create`, `security_list`, `security_price` | Register securities and record prices |
| `investment_buy`, `investment_sell`, `investment_lots`, `investment_holdings` | Trade, inspect lots and value holdings |
| `payroll_preview` | Preview aggregate payroll and return a reviewed import digest |
| `payroll_import` | Post or propose the exact reviewed aggregate payroll journal |
| `payroll_show` | Read the payroll liabilities and their reconciled withdrawals |
| `payroll_fund` | Match a posted withdrawal to a payroll liability |
| `collection_create_link` | Create or recover a Stripe-hosted link for an issued invoice |
| `collection_apply` | Authenticate and apply a signed Stripe delivery from the trusted host spool |
| `collection_resolve_receipt` | Post a freshly authenticated receipt blocked solely by a closed period on an explicit open date |
| `collection_show`, `collection_exceptions` | Inspect collection status and review exceptions |
| `collection_reconcile_payout`, `collection_payouts` | Reconcile signed net deposits and list payout evidence |
| `collection_reconcile_refund`, `collection_refunds` | Post approved verified refunds and list their evidence |
| `collection_disputes` | Inspect signed dispute movements pending accounting review |
| `financial_summary` | Read-only advisor facts and comparisons; `company`, optional dates, currency and account selector arrays; returns the full `bk summary` JSON envelope |
| `fixed_asset_register` | Register an existing acquisition or post its acquisition journal |
| `fixed_asset_show` | Show a fixed asset and movements as of a date |
| `fixed_asset_list` | List as-of assets and reconcile their control accounts |
| `fixed_asset_preview` | Preview unposted depreciation charges through a date |
| `fixed_asset_depreciate` | Post pending full-month depreciation charges |
| `fixed_asset_dispose` | Dispose of an asset and record the gain or loss |
| `expense_claim_receipt_intake` | Stage a local receipt and optional supplied OCR findings |
| `expense_claim_receipt_show` | Read receipt evidence and unconfirmed OCR findings |
| `expense_claim_create` | Create a draft from confirmed facts and staged evidence |
| `expense_claim_amend` | Amend a draft or rejected claim |
| `expense_claim_submit` | Freeze a draft for review |
| `expense_claim_approve` | Post an approved liability or match a posted card expense |
| `expense_claim_reject` | Reject a submitted claim with a reason |
| `expense_claim_record_payment` | Match an existing payment to a reimbursement |
| `expense_claim_void` | Cancel an approved and unpaid claim |
| `expense_claim_show` | Show the latest immutable claim revision |
| `expense_claim_list` | List a book’s most recently created claims |
| `inventory_register` | Register a stock item and its control accounts |
| `inventory_list` | List stock items |
| `inventory_show` | Show a registered stock item |
| `inventory_move` | Record a receipt, shipment, return or count adjustment |
| `inventory_movement` | Show the movement and journal |
| `inventory_movements` | List recent movements through a cutoff |
| `inventory_match_document` | Match a stock movement with an issued invoice or bill line |
| `inventory_valuation` | Show historical FIFO value and stock control account differences |
| `fx_post` | Record native minor units and their functional value with a rate and unique reference |
| `fx_revalue` | Revalue one or all foreign accounts as of a date and return an undo receipt for each adjustment |
| `ledger_setup_status` | Read ledger readiness, including company, account, and transaction counts and whether the approval key is configured. Takes no arguments. |
| `ledger_setup_book_show` | Read a book's current profile, fiscal year, base currency, and named accounts |
| `report` | Run CLI reports |
| `rule_list`, `rule_create`, `rule_amend`, `rule_undo`, `rule_delete` | Read and manage company categorization rules with audit history |
| `rule_match` | Preview the winning rule for a candidate description |
| `bookkeeping_policy_show`, `bookkeeping_policy_set` | Read or save standing sales-tax and processor-fee instructions |
| `inform_preference_show`, `inform_preference_set` | Read or save how often the owner hears about completed changes |
| `budget_list`, `budget_set`, `budget_delete` | Inspect and manage budgets |
| `verify`, `audit_log` | Verify integrity and inspect the audit trail |
| `audit_receipts`, `audit_undo` | Read grouped changes and apply or propose an eligible receipt undo |
| `restore`, `restore_undo` | Restore a server-side backup or undo a restore on the selected ledger with `ledger/admin` |

MCP discovery publishes input schemas and read-only hints from the ledger
operation catalog. Input schemas describe nested fields inline. Send objects and
arrays directly. Clients that encode a structured field as JSON text can send
the same arguments and receive the same validation. Capability enforcement can
restrict discovery further. Company-scoped tools require a `company` argument.
`report` takes a `kind` and its options, with all 14 CLI report kinds available, including `schedule-c`,
`1099` and `trend`. See the [report-kind list](docs/beankeeper/references/commands.md#report-kinds).
For `trial-balance`, `balance`, `balance-sheet`, and `reconciliation`, use
`as_of` or `to` in MCP and `--as-of` or `--to` in the CLI. Use `as_of` for
`aging`. For date-range reports such as `income-statement`, use `from` and `to`.

`txn_clear` and `bk txn clear` accept signed major-unit statement amounts such
as `-95.40` for a withdrawal (credit) or `+95.40` for a deposit (debit) on
the selected account. An unsigned amount clears an entry on either side.
Use `txn_reverse` or `bk txn reverse` to correct a mistaken posting. Use
`txn_link` or `bk txn link` to record a relationship within the same book.

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
`{code, message}`. `financial_summary` returns the full CLI envelope on both
success and failure.

`company_create` takes `slug`, `name`, and optional `description`. It creates a
company without a setup profile and returns the same row array as the CLI.
Use `ledger_setup_book_create` during authorized setup to create it together
with its profile. Both use the same audited company insert. Under enforcement,
`company_create` and `company_delete` require `ledger/admin`. Read-only mode
omits both.

Call `company_delete` with a `slug` to confirm deletion. The tool deletes an
eligible company, records its prior state in the audit log, and returns
`{"deleted":"slug"}`, matching the CLI's JSON data. Companies with retained
data return `VALIDATION`. Keep them for their history and use `txn_reverse`
when a posting needs correction.

For a separate set of sample books, run `bk init --demo --path demo.db`.
Desktop hosts can call `beankeeper_cli::commands::init::create_demo_ledger`
with a fresh path and a passphrase resolved by the host. It publishes a
verified demo ledger without overwriting an existing destination.

Call `txn_meta_set` with `company`, exactly one of `id` or `reference`, and a
`values` object such as `{"status":"pending"}`. Legacy `["status=pending"]`
arrays still work. `txn_meta_unset` takes the same selector and an array such
as `keys: ["status"]`. Both tools share the CLI validation and return a
`receipt` for each change. With a signed direct bookkeeper grant, call `audit_undo` with
that receipt and company to restore the prior annotations in one step.
Read-only mode omits both write tools.

Call `txn_reverse` with `company` and the original transaction `id`; `date`,
`description`, and `reference` are optional. A direct grant creates a linked
mirror posting and returns its audit receipt. Propose-only grants return a
pending proposal.
Supply the same explicit `reference` on retries to return the existing reversal
with `skipped: true` and `existing_id`. Reconciled transactions cannot be reversed;
existing clearings stay on the original and reversal entries start uncleared.

To move an imported line to a different account, call `txn_recategorize` with
`company`, exactly one of the original transaction's `id` or `reference`, and
`to_account`. On the CLI, use `bk txn recategorize --id 42 --to-account 5100`.
Add `entry_id` or `from_account` when more than one line could match.
A direct bookkeeper receives a summary, amounts and account codes, a
`desktop_path`, and a `receipt_id` for one-step undo with `audit_undo` or
`bk audit undo RECEIPT`. The new journal offsets the source account and
posts the same amount to the destination. The original bank entry and
statement history stay intact.

See the [MCP command reference](docs/beankeeper/references/commands.md#mcp) for
tool arguments and the [intentional CLI-only mutations](docs/beankeeper/references/commands.md#intentional-cli-only-mutations)
for actions that still need a local command.

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
`--agent-token` or `BK_AGENT_TOKEN` is a fallback for startup ledger requests
with no Authorization header. An invalid or malformed explicit header never
falls back. Missing, expired, revoked, and untrusted tokens return HTTP 401
with `UNAUTHORIZED`.
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

Rust hosts can call `beankeeper_cli::db::current_schema_version()` before
opening a ledger to get the schema version that `Db::open_snapshot` expects.
Convert the result with `i64::from` to compare it with `get_schema_version`,
which reads the version stored in an open ledger.

Hosts can add application tools with `http_router_with_host_tools` and a
`HostToolHandler`. Beankeeper filters their discovery and dispatch using the
current request's verified grants, revocations, and read-only ceiling. Navigation
uses an explicit `ui/navigate` grant on a separate UI-scoped token. See the
[host tool API and example](docs/mcp-host-tools.md).

If the host already opened the startup ledger, pass its `Db` directly to
`http_router_with_ledger_targets` to keep Streamable HTTP resumption via
`Last-Event-ID`. Hosts that prepare additional ledger files can read the
validated names and paths with `Cli::ledger_bindings()`.

Use `Db::with_write_authorizer` on each opened ledger that needs host-controlled
write access. The same policy applies to embedded CLI dispatch and MCP, and a
denied write returns `LEDGER_WRITE_DENIED` with the ledger target and reason.
Reads retain the usual credential checks. See the
[per-ledger write policy example](docs/mcp-host-tools.md#authorize-writes-per-ledger).

### Select a ledger by name

Bind an existing ledger to a name with `--ledger TARGET=PATH`. To use it in a
local command or a stdio MCP process, select it with `--ledger-target TARGET`.

```sh
bk --ledger demo=/absolute/path/demo.db --ledger-target demo --json company list
bk --db /absolute/path/books.db --ledger demo=/absolute/path/demo.db mcp serve --http
```

On HTTP, send `Beankeeper-Ledger-Target: demo` and a bearer issued by the demo
ledger with each request to it. Omitting the header selects the startup ledger
at `--db`. An existing MCP session can switch ledgers request by request. The
CLI HTTP server opens all configured ledgers with its resolved startup
passphrase. Hosts with per-ledger keys can pass already-opened databases to
`LedgerTargets` and register them while clients are connected. See the
[multi-ledger host guide](docs/mcp-host-tools.md#dispatch-to-another-ledger).

An embedded host can register separate books as a `HostOwnedTarget`. The agent
uses the same startup bearer and MCP session, with its signed grants, posting
mode, and audit identity. Hosts can detach a named target and wait for active
calls to finish before removing its file. Named MCP tool results carry
`_meta["beankeeper/ledgerTarget"]`, and CLI JSON carries `meta.ledger_target`.

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

Generated audit activity and postings are credited to `demo`. The initializer's
actor is recorded on the three company creations and one ledger-wide `demo_seed`
event. Each generated posting has `beankeeper_source` set to `demo` in its
metadata. [Demo report and audit notes](docs/reports.md)

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

### Tailor the demo to your situation

Tell your agent which types of books, accounts, and currencies you want to try.
It can pass those choices to `bk init --demo --path tailored.db --profile -` as JSON
on stdin. `--profile` also accepts inline JSON or an existing JSON file. Choose a
fresh path for tailored books. The generator keeps the profile in memory and
writes a separate, verified database with fictional names, merchants, and amounts.

```json
{
  "work_shape": "self_employed",
  "country": "MX",
  "currencies": {"secondary": ["USD"]},
  "books": [
    {"kind": "personal", "accounts": [{"kind": "checking"}, {"kind": "savings"}]},
    {"kind": "llc", "industry": "professional_services",
     "accounts": [{"kind": "checking"}, {"kind": "credit_card", "currency": "USD"}]}
  ],
  "features": {"include_investments": true, "include_owner_draws": true}
}
```

Use these choices in a profile:

| Field | Accepted choices |
|---|---|
| `work_shape` | `employee`, `self_employed`, `business_owner`, `retired`, `other` |
| `country` | An assigned uppercase two-letter country code |
| `currencies` | An optional `primary` and distinct `secondary` currencies supported by the ledger |
| `books[].kind` | `personal`, `sole_proprietorship`, `llc`, `corporation` |
| `books[].industry` | `general`, `professional_services`, `retail`, `trades` |
| `books[].accounts[].kind` | `checking`, `savings`, `credit_card`, `payment_app`, `loan`, `investment`, `cash` |

An empty profile generates one personal book with checking. The primary currency
defaults to a supported currency associated with the country, or USD. An account
without a currency uses the primary currency. Each selected currency gets Q1
2025 activity and a synthetic checking account if it needs a cash source.
Investments are opt-in. When both personal and business books exist, linked draws
use the primary currency and the first personal book. Set `include_owner_draws`
to `false` to omit those examples.

Embedded hosts can call `create_demo_ledger_from_profile` or expose the same
profile through the authenticated `demo_ledger_create` MCP host tool.
[Host configuration](docs/mcp-host-tools.md#tailored-demo-books)

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
keys containing a double quote or NUL, and duplicate keys are rejected.
Reserved keys are `correlate`, `proposal_id`, `reverses`, `reversed_by`,
`applies_to`, and `beankeeper_source`. Set custom settlement `--link-key` values
at post time and treat them as relationships too.

Each changed batch writes one `metadata_set` or `metadata_unset` audit row with
whole-object `before`/`after` snapshots and the resolved actor, atomically with
the edit. Removing the last key leaves `{}`. Setting an identical string or
removing an absent key is a no-op with no audit row. Other keys and numeric
precision survive. Use the returned `receipt` with
`bk audit undo --company COMPANY RECEIPT` under a signed direct bookkeeper grant to
restore the exact prior metadata. Undo newer annotation edits first.
Dates, entries, amounts, currency, references, and creation attribution stay
unchanged. Tagging the original charge preserves its age and payment matching
in `report aging --filter-metadata status=disputed`.

JSON envelopes use `txn.meta.set` / `txn.meta.unset` and return
the transaction `id`, stored JSON text (or `null`) in `metadata`, and a
`changed` flag. Changes also return a `receipt` ID and a `desktop_path` to its
detail view. For a no-op, both fields are `null`. Table mode prints the
transaction ID, resulting metadata, and any new receipt ID.

### Link related transactions in one book

```sh
bk --company personal --json txn link 43 42 --kind refund
bk --company personal txn show 42
bk audit undo --company personal RECEIPT
```

Link the source transaction to its target, such as a refund to its original
charge or a payment to its invoice. The kinds are `transfer`, `refund`,
`payment`, `adjustment`, `recategorization`, and `related`. Both transactions
must be in the selected book. Duplicate active links of the same kind,
self-links, and directed cycles are rejected. Links leave balances and entries
unchanged. Either transaction shows the active link, and JSON transaction lists
include a `links` array per transaction. Lists hide links to either side of an
audited reversal by default. Use `--include-reversed` on the CLI or
`include_reversed: true` in MCP to include them. Inspect a transaction by ID to
see its active links regardless of reversal status.

MCP exposes the same operation as `txn_link` with `company`, `source_id`,
`target_id`, and `kind`. The result includes a `receipt` and an optional
Desktop detail path. Pass the receipt to `audit_undo` or `bk audit undo` with
your signed direct bookkeeper or owner grant. Undo removes the link from active
views and keeps its history. Use `txn post --correlate` for intercompany links.

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
`reverses`, both numeric transaction IDs. `txn show` and
`txn list --include-reversed` display these links. Apart from the added link,
the original's entries and metadata remain intact. The reversal copies neither
transaction metadata nor attachments. An atomic `reverse` audit record captures the actor, original
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

### Connected Bank Feeds

Poll a Plaid bank or card account into a review queue. Review an exact
provider revision before matching it to a ledger posting or creating a new
posting or approval proposal. Sync leaves balances unchanged. See
[bank feeds](docs/bank-feeds.md) for host credentials and commands.

### Bank Import Categorization Rules

Save recurring description-to-account mappings once, then apply them automatically
to future CSV and OFX/QFX imports:

```sh
bk --company personal rule create --contains Starbucks --account 5000 --priority 10
bk --company personal rule list --json
bk --company personal rule match --description 'STARBUCKS 102'
bk --company personal rule amend "$RULE_ID" --contains Starbucks --account 5100 --priority 10
bk --company personal txn import --file checking.ofx --account 1000 --suspense 9000
bk --company personal rule delete rule_01h455vb4pex5vsknk084sn02q --force
```

Use the active rule ID returned by `create`, `amend` or `list` when changing a rule. Rules belong
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

Rules apply during `--dry-run` too. Changes affect future imports and keep
existing postings and references intact. Amend with a full replacement
pattern, account and optional priority. The replacement gets a new ID and the
audit trail records both versions under `rule-supersede`. An explicit delete
removes the rule from future matching and records its last version.
`rule match` previews the winning rule or returns `no_match` without posting.

Use `bk --company COMPANY rule undo RULE_ID` or MCP `rule_undo` with a signed
`ledger/admin` credential to undo a creation in one step. The rule stops
matching immediately. The original rule and the linked undo receipt stay in
the ledger, and a retry returns the same receipt. For an amendment, restore
the earlier terms with another `rule amend` or `rule_amend` call.

Agents can also call `rule_list`, `rule_create`, `rule_amend`, `rule_match` and
`rule_delete` over MCP. The same operations are available in the CLI.

### Standing bookkeeping and inform preferences

```sh
bk --company personal bookkeeping-policy show
bk --company personal bookkeeping-policy set sales-tax=track-liability
bk --company personal bookkeeping-policy set processor-fees=separate-expense
bk --company personal inform-preference show
bk --company personal inform-preference set daily-digest
```

Agents can use `bookkeeping_policy_show`, `bookkeeping_policy_set`,
`inform_preference_show` and `inform_preference_set` over MCP. These settings
record company-specific instructions for the agent. Posting logic and capability
enforcement continue to follow the existing ledger rules. The inform preference
defaults to `per-change` and also supports `daily-digest` and `weekly-digest`.
The agent saves changes directly, tells the owner what changed, and can restore
prior values with the same set operation. Use `unset` to clear a bookkeeping
instruction. See the [full CLI and MCP contract](docs/beankeeper/references/commands.md#standing-bookkeeping-and-inform-preferences).

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

`bk txn list` and MCP `txn_list` hide both sides of reversed pairs by default,
including in counts and pagination totals. Use `--include-reversed` or
`"include_reversed": true` to include them alongside other matching transactions.
Use `txn show` to inspect either posting directly by ID.

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

When an existing account needs a different display name, rename it in place.
Keep using its existing code for all transactions and reports.

```sh
bk --company personal account rename 1000 "Operating Cash" --json
```

The equivalent MCP call is `account_rename` with
`{"company":"personal","code":"1000","name":"Operating Cash"}`.
The bookkeeper's direct grant returns the previous name, an undo receipt,
and a Desktop link to the change. A propose-only grant returns a proposal ID for
`txn_approve`. Use `bk --company personal audit undo RECEIPT_ID` or `audit_undo` with the
signed bookkeeper grant to restore the previous name.

Set `with_balances: true` in MCP `account_list` or pass `--with-balances`
to `bk account list` for posted debit and credit totals, a posted balance,
and an available balance that includes pending holds. Supply a
currency if the company has transactions in several currencies.

```sh
# List expense accounts with their debit/credit totals
bk --company personal account list --type expense --with-balances --json

# Search accounts by name, with balances for a period
bk --company personal account list --name "Cash" --with-balances --to 2026-06-30 --json
```

### Pending card holds

Record a card authorization with one debit and one credit on different accounts.
The hold affects the available balance as of its authorization date. Posted
balances, financial statements, tax reports and reconciliation use posted activity.

```sh
bk --company personal --json txn post -d "Pharmacy" --status pending \
  --debit 5000:43.45 --credit 1000:43.45 --date 2026-03-02 --reference hold-1

bk --company personal --json txn settle-hold --hold-reference hold-1 \
  --amount 43.88 --date 2026-03-04 --reference posted-1
```

For MCP, pass `status: "pending"` to `txn_post`. Call `txn_settle_hold` with
`company`, `hold: {"reference":"hold-1"}` or `hold: {"id":42}`, decimal
`amount`, `date`, and the posted `reference`. Settlement uses the hold's
accounts and currency. It releases the authorization and posts the actual
charge together. The result identifies both transactions and reports the
amount difference in minor units and as a decimal string. Retry with the same
posted reference, date and amount.

Direct grants settle immediately. A propose-only grant returns a proposal.
To undo a settlement, reverse the settled transaction or undo its audit
receipt. Undo restores the hold as of the reversal date. Settle a restored
hold with a fresh posted reference. To cancel an open hold, reverse the hold
itself.

CSV and OFX/QFX imports settle a unique open hold in the same company, bank
account, currency, and direction. The importer compares the CSV description or
OFX payee name with the hold's `payee` metadata, using the hold description when
no payee is set. The comparison ignores case and surrounding whitespace. By
default the amount must match exactly and the posted date must be within seven
days of the authorization, inclusive.

```sh
bk --company personal --json txn import --file card.csv --account 1000 --suspense 9000 \
  --hold-amount-tolerance 50 --hold-posting-window-days 5
bk --company personal --json summary --stale-hold-age-days 10
```

The tolerance is in minor units (50 cents for USD). A matched file row reports
the posted ID, original hold ID, signed amount difference, and Desktop
register link. Retrying the same bank reference and movement returns the same
settlement, including after an undo. When several holds match, the importer
reports the ambiguity and leaves the line unwritten. An agent that can identify
the right hold can settle it by ID with the posted bank reference, then retry.

The bounded import also matches holds through `bk setup statement-import` and
`ledger_setup_statement_import`. For MCP use `hold_amount_tolerance` and
`hold_posting_window_days`. Direct grants return an audited batch receipt for
one-step undo. Propose-only grants return proposal IDs and their matched hold IDs.

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

Reconciling records a **reconciled date**: with `--status reconciled`, `--date` (default today) is the day the clearings were locked in. Promoting a statement's clearings keeps their cleared date and sets the reconciled date (it may not be before the cleared date); a clearing created directly as reconciled gets the same cleared and reconciled date. Reconciled clearings are locked: `--status cleared` on them is an error. To redo an individual clearing, use `--status uncleared` with its `--statement`. For a clearing linked to a completed monthly statement, undo the statement receipt first.

An entry's status is derived from its clearings: `cleared` once fully cleared, `reconciled` once fully cleared by reconciled clearings, `uncleared` otherwise (including partially cleared items). Status indicators (`*C*` for cleared, `*R*` for reconciled) appear in `bk txn show` and `bk txn list` table views. In JSON mode, every entry includes a `status` field. `bk txn show` also lists each entry's id, cleared and remaining amounts, and individual clearings (amount, cleared date, reconciled date, statement, status).

Complete the month once its deposits, withdrawals, or card activity have been
matched with `txn clear`.

```sh
bk --company personal --json reconcile statement --account 1000 \
  --period-end 2026-03-31 --statement-ending-balance 4210.33 \
  --statement-ref BANK-2026-03
bk --company personal --json reconcile list --account 1000
```

Use the balance printed on the statement, in major currency units. Beankeeper
adds the month's signed clearings to the last completed statement's ending
balance. For the first statement, the opening balance is the signed total of
earlier reconciled clearings, or zero when there are none. Use `--currency` if
the company has multiple currencies. Amounts follow the account's normal
balance, so a positive credit-card balance means money owed.

Beankeeper counts existing clearings dated in the statement month. An earlier
posting can count if it cleared during that month. Use `--statement-ref` to
limit the clearings to a statement identifier and repeat `--entry-id` or
`--reference` to select particular entries or transactions. A difference
returns `STATEMENT_DISCREPANCY` with the computed ending balance, the supplied
ending balance and their difference in minor units. The statement stays open
and the clearings stay as they were.

A match locks the included clearings and returns a receipt. `--close-period`
also advances the company-wide posting cutoff to that month-end and requires
the same authority as `period close`. Pass the receipt to `bk audit undo` to
restore the promoted clearings and, with admin authority, restore the prior
cutoff if the statement still controls it. Undo later statements for the same
account and currency first. Statement history remains available in
`reconcile list`, which also shows account months that still need a statement.

Agents can use the equivalent `reconcile_statement`, `reconcile_list` and
`audit_undo` MCP tools. Results include exact amounts and optional links to
the account's activity in Desktop.

### Financial Summary

Ask the agent "How am I doing?" It can call `financial_summary` with a `company`,
or run `bk --company personal summary --json`, to get plain-language highlights,
standing balances, income/spending comparisons, spending movers and attention
details with chartable amounts and an optional Desktop target. Statement checks
name each account and month, newest first, in both CLI and MCP output. No app is required.
The default is this month to date versus the full previous calendar month.
`--from`, `--to`, `--compare-from`, `--compare-to` and `--currency` select the scope.
Use real calendar dates in `YYYY-MM-DD` format for CLI and MCP report and
summary filters. When the end date defaults to today, Beankeeper uses the local
date. Invalid dates return `VALIDATION`.

The CLI and MCP tool return the same full envelope. Overdue status is explicitly
unavailable because the ledger has no payment due dates; older open items are
reported separately. The `stale_holds` attention item shows authorizations still
open at the end date and older than seven days by default. Use
`--stale-hold-age-days` or MCP `stale_hold_age_days` to change the threshold.
See the [summary contract and limitations](docs/financial-summary.md).

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
are retained. Each series includes a typed semantic `role` so agents can distinguish
assets, debt and net worth without guessing from labels or signs. Colors remain
the consumer’s choice. See the [schema and worked example](docs/chart-data.md).

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

`bk report reconciliation` compares those clearings with an account statement. It reports the book balance (all entries), the cleared balance, and the reconciled balance on the account's normal side, lists the outstanding entries (including partially cleared ones, with their reference, metadata, cleared and remaining amounts) with their total (book - cleared = uncleared total), and, given `--statement-balance`, shows the difference (statement - cleared) and whether the account is reconciled:

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

### Cross-company financial statements

Use this report to review companies together when they share a ledger and
currency. Repeat `--member` for at least two distinct active company slugs.
Dates are inclusive. A conflicting book currency or an in-scope posting in
another currency rejects the report.

```bash
bk --json group-report income-statement --member acme --member beta --currency USD --from 2026-01-01 --to 2026-03-31
bk --format csv group-report balance-sheet --member acme --member beta --currency USD --as-of 2026-03-31
```

Review each company's accounts under its own slug. Combined figures are
**before eliminations**. Intercompany evidence lists reciprocal pairs once
and flags selected postings whose reciprocal side is unavailable in the
report's scope. The figures include those postings as booked.

MCP's read-only catalog exposes the same reports as `group_income_statement`
and `group_balance_sheet`. Pass `members` as a JSON array of company slugs
and send `currency` plus `from` and `to`, or `as_of`.

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

Without `--cash-account`, reports include accounts explicitly classified as `cash` or `cash_equivalent`, independent of their names. Bundled templates classify their cash accounts. New custom accounts default to `non_cash`.

Classify a wallet with `bk --company acme account classify 1050 cash`, or a money-market cash equivalent with `bk --company acme account classify 1060 cash_equivalent`. MCP offers the same operation as `account_classify` with `company`, `code`, and `cash_class`.

The result includes an audit receipt; undo it with `bk --company acme audit undo RECEIPT` or `audit_undo`. Reclassifying to `non_cash` also records a new audited change.

Existing books retain their former cash selection through a one-time migration; classify any omitted assets explicitly. Summary and cash-flow use the same stored classification, and renaming an account never changes its cash membership. `--cash-account` remains an explicit replacement selection. Cash equivalents use ledger book values, never invented market prices. A cash-flow report with no classified accounts returns a validation error with classification guidance. Transfers between cash accounts net to zero and are excluded.

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

For enveloped responses, the `meta` block is present on both success and error. The `meta.command` field uses dot notation (`company.list`, `txn.post`, `report.trial-balance`). The `meta.company` field is present when the command operates on a specific company. Error codes are: `USAGE`, `VALIDATION`, `DATABASE`, `SCHEMA_MISMATCH`, `NOT_FOUND`, `UNAUTHORIZED`, `FORBIDDEN`, `IO`, `GENERAL`.

Embedded Rust hosts can use `beankeeper_cli::output::json::EnvelopeError::from(&error)`
to render a `CliError` with their own response metadata. This preserves the
shared CLI and MCP error code, message, and structured details when present.

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

`bk backup` writes a verified full snapshot, including committed WAL changes,
at the source schema version and encryption. `--force` replaces an existing
backup output. On an enforced ledger, backup requires `ledger/admin`.

```sh
bk --db books.db backup --output ledger.bk-backup
bk --db books.db restore --input ledger.bk-backup
bk --db books.db restore-undo UNDO_ID
bk --db recovered.db recover --input ledger.bk-backup
```

For encrypted books, supply the usual passphrase source. On an enforced ledger,
restore and undo require a live signed `ledger/admin` credential. A caller who
supplies a token must have that scope on any ledger. Use `--db` or a named ledger
target. In MCP, call `restore` with `input` or
`restore_undo` with `undo_id`. The input path is on the server. Both tools use
the selected live ledger and are omitted in read-only mode.

Restore verifies the standalone backup and its audit lineage, upgrades an
older backup in a private copy, and rejects newer schemas or backup files with
SQLite sidecars. The live ledger must already use the running schema. Restore
replaces bookkeeping rows in one transaction and appends a `ledger_restored`
event. The identity registry, current revocations, enforcement policy, and
existing audit rows stay in place. The audit log continues to show the full
history. Bookkeeping reads use the history selected by the restore.

Tell the owner what was restored and offer the returned `undo_id`. Undo puts
back the books from the verified snapshot saved beside the ledger and appends a
new receipt. Use the latest restore receipt before subsequent audited bookkeeping
changes. Keep the adjacent undo snapshot files with the ledger. Results include
the restore receipt, snapshot identifiers, and verification data with row counts
and a Desktop review link.

Use `bk recover` to open a full current-schema snapshot at a new file path.
It carries the snapshot's identity and audit data into the new ledger. Backup
and recovery use the CLI. Keep the passphrase and the external `attachments/`
directory separately. Snapshots include attachment records. Receipt and
document files live in that directory. Use `bk export` for accounting interchange.

Standalone `bk` and MCP startup automatically publish a verified backup before
upgrading an older ledger. The backup stays beside the ledger at
`.bk-pre-migration-*/books.bk-backup` with its original schema and encryption.
Successful commands report its path on stderr unless quiet mode is enabled.

For embedded Rust hosts, `Db::open`, `Db::open_existing`, and
`Db::open_snapshot` return `CliError::SchemaMismatch { stored, expected }`
when a supported ledger needs an upgrade. Both fields are `SchemaVersion`
values. Use your host's backup location and pass the same resolved secret to
both calls.

```rust
use beankeeper_cli::db::{Db, SnapshotOverwrite, VerifiedSnapshot};

let backup = VerifiedSnapshot::create(
    ledger_path,
    passphrase.as_ref(),
    backup_path,
    SnapshotOverwrite::Refuse,
)?;
let ledger = Db::open_after_snapshot(backup, passphrase.as_ref())?;
```

The proof exposes the backup path, captured schema version, and verification
evidence. The migrating open consumes the proof and uses its bound source path.
If another SQLite connection commits after the snapshot starts, take a fresh
snapshot before retrying. Coordinate file moves and replacements with the upgrade.
Pass the opened connection to `commands::dispatch_with_db`, `mcp::serve_with_db`,
or `mcp::LedgerTargets` for HTTP. See the [host integration guide](docs/mcp-host-tools.md).

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

The CLI and MCP accept active currencies with a defined decimal minor unit
from the [ISO 4217 currency list](https://www.six-group.com/en/products-services/financial-information/market-reference-data/data-standards.html),
including KRW (0 decimal places), INR (2), and TND (3). Pass the three-letter
code as `--currency` in the CLI or `currency` in MCP. Fund codes, codes
without a defined minor unit, and test codes return a validation error.
The Rust `Currency` type exposes the alphabetic code, numeric code, and
minor-unit precision.

Each journal chooses its currency at construction. Debit and credit amounts are minor units in that currency. Adding a prebuilt entry with a different currency returns `EntryError::CurrencyMismatch`. Posting validates the count and balance, then retains the currency and balanced total in the resulting `Transaction`.

Reports total one currency. A configured book defaults to its functional currency.
Legacy books without a profile infer the currency from their postings, and require
`--currency CODE` when those postings use several currencies.

#### Foreign-currency balances and revaluation

After setting the book's functional currency through setup, declare the native
currency of an asset or liability. The following example assumes a GBP book with
a functional-currency cash account (1000) and an FX gain/loss account (4900).
Omit the native currency for accounts that use the book's currency. Once a book
has a foreign-currency account, ordinary postings use the functional currency.
Supply it explicitly when it differs from your CLI or MCP posting default.

```bash
bk --company personal account create 1100 "Euro cash" --type asset --native-currency EUR
bk --company personal fx post --account 1100 --offset-account 1000 --direction debit \
  --native-minor 10000 --rate 0.8 --date 2026-01-01 --description "Bought euros" --reference fx-1
bk --company personal fx revalue --account 1100 --as-of 2026-01-31 \
  --rate 0.85 --fx-gain-loss-account 4900 --json
```

This records EUR 100.00 at GBP 80.00. Revaluation raises the carrying value to
GBP 85.00 and records a GBP 5.00 unrealized gain while the account still holds
EUR 100.00. Rates are exact decimal functional major units per native major unit,
and the conversion rounds ties to even at the functional currency's precision.
The movement's functional value must round to at least one minor unit.

Each revaluation that changes the carrying value returns an `undo_receipt`.
Use `bk audit undo RECEIPT` to post the opposite adjustment, subject to the
ledger's period and reconciliation rules. Omit `--account` to revalue all active
foreign accounts and omit `--rate` to reuse the latest rate recorded for each
currency pair on or before the requested date. An all-account request with an
explicit rate must cover a single pair.

The matching MCP tools are `account_create` with `native_currency`, `fx_post`,
`fx_revalue`, and `audit_undo`. Account listings, balance sheets, and summaries
return the native balance, functional carrying value, and last active revaluation
in structured output, with links to the account register in Desktop.

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
- Library depends on `chrono`, `sha2`, `data-encoding`, and `num-bigint`, with optional `serde` and `rusqlite` features
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

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.
