# Beankeeper

The accounting runtime for AI agents. Your assistant manages your books -- `bk` makes sure it can't mess them up.

A CLI backed by SQLCipher-encrypted SQLite, with an embeddable Rust library underneath.

## Why Beankeeper?

AI assistants can already categorise expenses, reconcile bank feeds, and generate financial reports. The missing piece is a backend that makes those operations *safe* -- one where the agent physically cannot produce an unbalanced ledger, silently duplicate a transaction, or corrupt the audit trail.

hledger, ledger-cli, and beancount are built around plain-text files a human edits by hand. That's the right design for manual bookkeeping. `bk` is built for when an agent is doing it.

### The backing store is a database, not a file

SQLite (optionally encrypted with SQLCipher) is the source of truth, not a `.journal` file. That means the agent can post transactions, query balances, and generate reports through a stable CLI contract without worrying about file locking, merge conflicts, or parse errors.

### The agent cannot create an unbalanced transaction

A `Transaction` can only be constructed through `JournalEntry::post()`, which checks at runtime that there are at least two entries, all in one currency, and that total debits equal total credits. Invalid entries return a `TransactionError`; the type's private fields prevent callers from bypassing those checks when constructing a `Transaction`. The CLI validates postings through this API before saving them and reports validation failures as `VALIDATION` errors (exit code 3).

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

Audit rows form a SHA-256 hash chain. `bk verify` checks the chain, its tail, new posting snapshots, and accounting invariants alongside SQLite integrity and foreign keys. It reports the first broken link or invalid transaction as a `DATABASE` error (exit code 4).

These protections detect tampering against the stored chain; they are not signatures or an external trust anchor. Someone with unrestricted database access can remove triggers and rewrite the ledger, hashes, and sequence together, or restore an older complete database. See [audit integrity](docs/audit-integrity.md) for the format, migration behavior, and limits.

### Optional verified agent identity

`bk key init` creates a local trust root inside an encrypted ledger.
`bk agent issue --name claude --public-key "$AGENT_PUBLIC_KEY" --ttl 8h` issues
an attestation. Supply it with `--agent-token` or `BK_AGENT_TOKEN` to record its
verified `agent_uri` and `token_jti` on new postings and audit rows. `txn show`,
`audit log --agent URI`, and JSON `meta.agent` expose attribution; `agent list`
and `agent revoke URI_OR_JTI` manage issued tokens. MCP accepts one token per
session and rechecks it on every call. Invalid, expired or revoked supplied
tokens return `UNAUTHORIZED` (exit 6).

Tokens remain optional. Capabilities are recorded, and do not restrict commands
in this phase. Token verification attests claims without proving possession of
the agent private key; writes have no agent signatures. See
[agent identity setup and limits](docs/agent-identity.md).

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

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper/main/install.sh | sh
```

Detects your OS and architecture, downloads the right binary, verifies the SHA-256 checksum, and installs to `/usr/local/bin`. Override the install directory with `BK_INSTALL=~/.local/bin` or pin a version with `BK_VERSION=X.Y.Z`, replacing `X.Y.Z` with a published CLI release version.

**Arch Linux (AUR)**:
```sh
yay -S bk-bin
```

**From source**:
```sh
cargo install --git https://github.com/Govcraft/beankeeper beankeeper-cli
```

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

The server provides `company_list`, `company_show`, `account_list`, `account_show`,
`account_create`, `txn_post`, `txn_reverse`, `txn_list`, `txn_show`, `report`, `budget_list`,
`budget_set`, `budget_delete`, `txn_clear`, `txn_reconcile`, `verify`, and `audit_log`.
Company-scoped tools require a `company` argument. `report` takes a `kind` and its
options, with all ten CLI report kinds available. Tool schemas describe the
accepted arguments. For example, `txn_post` takes:

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
The CLI's default `error` strategy rejects duplicates; its existing `upsert`
strategy remains unsupported. Inputs use decimal major-unit strings, and results
use the same data payload and integer minor units as `bk --json`. Results are
available as JSON text content and, where the negotiated MCP protocol permits the
payload's shape, `structuredContent`. Tool failures set `isError: true` with
`{code, message}`. Audited writes use `mcp:<client name>` from the MCP handshake,
with a sanitized name or `mcp:unknown`. Creation auditing is unchanged.

Call `txn_reverse` with `company` and the original transaction `id`; `date`,
`description`, and `reference` are optional. It creates a linked mirror posting.
Supply the same explicit `reference` on retries to return the existing reversal
with `skipped: true` and `existing_id`. Reconciled transactions cannot be reversed;
existing clearings stay on the original and reversal entries start uncleared.

This local server has no HTTP transport or authentication. Ledger initialization,
company creation/deletion, account deletion, import, export, and attachments remain
CLI operations. See the [MCP command reference](docs/beankeeper/references/commands.md#mcp).

## Demo Mode

Spin up a fully populated database to explore every feature without manual setup:

```sh
bk init --demo
```

This creates three companies with charts of accounts, regular transactions, and intercompany-linked mirror pairs:

| Company | Slug | Description |
|---------|------|-------------|
| Acme Consulting LLC | `acme-consulting` | Service revenue, rent, office expenses, payroll |
| Acme Products Inc | `acme-products` | Inventory, product sales, COGS, shipping |
| Personal | `personal` | Checking/savings, salary income, personal expenses |

Intercompany transactions demonstrate bidirectional correlation:

- Owner loans personal savings to acme-consulting (`--correlate`)
- acme-consulting purchases software licences from acme-products
- acme-consulting settles the intercompany payable to acme-products
- **Payroll with withholdings** -- a split transaction showing a $5,000 gross salary broken into federal tax, state tax, FICA, and net pay on both the employer and employee books

Try it out:

```sh
# Trial balance for the consulting company
bk --company acme-consulting report trial-balance

# View the payroll split transaction
bk --company acme-consulting txn list --description "payroll" --json

# Balance sheet as of quarter end
bk --company personal report balance-sheet --to 2024-03-31

# Trial balance for a date range
bk --company personal report trial-balance --from 2024-01-01 --to 2024-03-31

# Balance as of a specific date
bk --company personal report balance --account 1000 --to 2024-03-16
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

References are stored verbatim after trimming surrounding whitespace; empty or whitespace-only references are rejected. The same reference can be used in different companies. Transactions without `--reference` have no reference-based deduplication. `--on-conflict upsert` is not implemented.

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
entire batch on a duplicate. `upsert` is unsupported.

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

### OFX / QFX Bank Statement Import

Import transactions from bank and credit card statements exported in OFX format. Each OFX transaction becomes a balanced double-entry against a user-specified bank account and suspense/clearing account:

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

All filters can be combined. Amount filters use the `--currency` value (or `BEANKEEPER_CURRENCY` env, defaulting to USD) for major-to-minor unit conversion.

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
bk export --output-format json > backup.json

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

For enveloped responses, the `meta` block is present on both success and error. The `meta.command` field uses dot notation (`company.list`, `txn.post`, `report.trial-balance`). The `meta.company` field is present when the command operates on a specific company. Error codes are: `USAGE`, `VALIDATION`, `DATABASE`, `NOT_FOUND`, `UNAUTHORIZED`, `IO`, `GENERAL`.

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `BEANKEEPER_DB` | Database file path (default: `./beankeeper.db`) |
| `BEANKEEPER_COMPANY` | Default company slug (avoids `--company` on every command) |
| `BEANKEEPER_CURRENCY` | Default currency code for amount filters (default: `USD`) and for `report budget-variance` (inferred from transactions when unset) |
| `BEANKEEPER_PASSPHRASE_CMD` | Command to obtain encryption passphrase |
| `BK_ACTOR` | Named poster and audit actor |
| `BK_AGENT_TOKEN` | Optional local agent attestation; overridden by `--agent-token` |
| `NO_COLOR` | Disable colored output |

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
    )
    .debit(&cash, Money::usd(50_00))
    .unwrap()
    .credit(&revenue, Money::usd(50_00))
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
    )
    .debit_with_memo(&salary, Money::usd(5000_00), "Gross salary")
    .unwrap()
    .credit_with_memo(&cash, Money::usd(3800_00), "Net pay")
    .unwrap()
    .credit_with_memo(&tax, Money::usd(1200_00), "Federal withholding")
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

`JournalEntry::post()` rejects mixed-currency entries at runtime with `TransactionError::CurrencyMismatch`.

Reports (`trial-balance`, `balance`, `income-statement`, `balance-sheet`, `tax-summary`, `budget-variance`) are always computed in a single currency and never add amounts across currencies. Pass `--currency CODE` to choose it; otherwise it is inferred from the company's transactions (USD when there are none). If a company has transactions in more than one currency, a report without `--currency` fails with a validation error listing the currencies found.

### Error Handling

All fallible operations return `Result` with descriptive variants. The top-level `BeanError` aggregates all domain errors for ergonomic `?` usage:

```rust
use beankeeper::prelude::*;

fn record_sale(ledger: &mut Ledger) -> Result<(), BeanError> {
    let cash = Account::new(AccountCode::new("1000")?, "Cash", AccountType::Asset);
    let revenue = Account::new(AccountCode::new("4000")?, "Revenue", AccountType::Revenue);
    let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
    let txn = JournalEntry::new(today, "Sale")
        .debit(&cash, Money::usd(50_00))?
        .credit(&revenue, Money::usd(50_00))?
        .post()?;
    ledger.post(txn);
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
