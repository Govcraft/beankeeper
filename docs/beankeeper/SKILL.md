---
name: beankeeper
version: 0.26.1
description: >
  This skill should be used when interacting with the beankeeper accounting system
  via the `bk` CLI or MCP server. Use when the user asks to "record a transaction", "post an entry",
  "check a balance", "generate a report", "create a company", "set up accounts",
  "import bank statements", "set a budget", "compare budget vs actual",
  "reconcile entries", "verify the ledger", "export data", "attach a receipt",
  or any financial bookkeeping task. Also use when piping structured JSON output
  from `bk` into other tools or agents.
---

# Beankeeper (`bk`) -- Double-Entry Accounting CLI

Beankeeper is a double-entry accounting system operated through the `bk` CLI or its MCP server. It stores data in a local SQLite database (optionally encrypted via SQLCipher). All output supports three formats: human-readable tables, machine-readable JSON, and CSV.

For binary installation, `install.sh` verifies the archive and checksum file's
Sigstore bundles when `cosign` is on `PATH`, before extraction or installation.
Missing bundles or failed verification stop installation. Without cosign it
warns and verifies SHA-256 only. Older unsigned releases fail with cosign present.
See the [release verification contract](../RELEASE.md#sigstore-verification-contract)
for artifact/bundle names, exact release-tag identity, issuer, and manual
`cosign verify-blob` / `gh attestation verify` commands. Sigstore signing does
not replace Apple notarization or Windows Authenticode.

## Core Concepts

- **Double-entry**: Postings are validated at runtime before saving. Unbalanced or mixed-currency entries are rejected; the library's `Transaction` type can only be constructed through `JournalEntry::post()` after validation.
- **Companies**: Multi-tenant -- each company has its own chart of accounts and ledger. Specified via `--company SLUG` or `BEANKEEPER_COMPANY` env var.
- **Accounts**: Five types: `asset`, `liability`, `equity`, `revenue`, `expense`. Each has a code (e.g. `1000`) and a normal balance direction (debit or credit).
- **Amounts**: Always specified in **major units** (dollars, not cents) on the CLI. Stored internally as minor units (cents). Example: `2500` means $2,500.00.
- **Append-only ledger**: Database triggers reject edits, deletion, and replacement of posted financial fields and entry details. Transaction metadata and entry clearance status remain mutable. Corrections use reversing entries.
- **Audit trail**: Posts, imports, and reversals store a poster (`posted_by`) and a `create` record. Supported mutations record an actor, timestamp, and before/after snapshots atomically. Set the principal with `--actor NAME` or `BK_ACTOR`, including OFX imports. `bk audit log --action create` lists creations; JSON audit rows include `prev_hash` and `row_hash`.
- **Verification**: `bk verify` validates the SHA-256 audit chain and its tail, compares newly audited posting snapshots, and checks entry count, positive amounts, valid directions/currency, company ownership, debit/credit balance, SQLite integrity, and foreign keys. Failures use `DATABASE` (exit 4). Legacy audit rows are chained on migration without changing IDs or timestamps; legacy posters stay unknown. Tamper evidence has no external trust anchor and cannot prove history against a complete database/hash rewrite or rollback.
- **Idempotency**: `--reference KEY` is trimmed, stored verbatim, and unique per company. Empty references are rejected. Use `--on-conflict skip` for safe retry of duplicate posts; Only `error` and `skip` are accepted; use `txn meta set/unset` to annotate existing postings.

For detailed accounting concepts and account types, see [`references/accounting.md`](references/accounting.md).

## Annotating posted transactions

Use `bk --company acme txn meta set 42 status=disputed dispute_id=D-42` or
`txn meta set --reference CHARGE-42 status=disputed`. Remove annotations with
`txn meta unset 42 status dispute_id` or select by `--reference`.
Values are strings, including numbers and booleans, as with `txn post --meta`.
Do not edit `correlate`, `reverses`, `reversed_by`, `proposal_id`, or `applies_to`: those keys
are reserved for ledger relationships. Set custom settlement link keys at post
time and treat them as relationships too. Duplicate keys are rejected.
Each changed batch records whole-object snapshots under `metadata_set` or
`metadata_unset`; no-op retries append nothing. JSON uses `txn.meta.set` /
`txn.meta.unset` and returns `{id, metadata, changed}` with metadata as JSON text
(or null for a no-op on an unannotated transaction). Removing the last key
leaves `{}`. Dates and financial fields stay unchanged, so tagging an original
AR charge preserves its age under `report aging --filter-metadata status=disputed`.

## Spend limits and approval

`bk agent limit set URI --company acme --currency USD --max-post 500 --daily-limit 2000`
sets optional per-post and transaction-date limits, measured by total debits.
Use `agent limit show URI [--company acme]` to inspect and `agent limit clear URI
[--company acme]` to remove all currencies in scope. Set/clear requires
`ledger/admin` under enforcement; configured limits apply to verified identities
even with enforcement off. All tokens for one URI share usage. With any limit
for an agent/company, other currencies require matching rows; no limits means no
amount cap. Named actors without a token have no URI bucket.

On a spend `FORBIDDEN` response, use `txn propose` with the same posting flags
if authorized for `ledger/propose`. A verified token is always required to
propose. Proposals freeze accounts, tax defaults and payload, reserve their
reference and create no posted transaction or audit row. `txn proposals` lists
pending requests; `--include-rejected` includes all resolved statuses. Read/propose
grants see owned requests; approve/admin grants see all under enforcement.

A reviewer with `ledger/approve` runs `txn approve PROPOSAL_ID` or `txn reject
PROPOSAL_ID --reason TEXT`. A proposer can `txn withdraw PROPOSAL_ID` (alias of
reject) for an owned pending request. Approval may exceed the proposer's cap,
posts with the approver's identity, consumes the proposer's date/currency bucket,
and records the complete proposal in `proposal_approve` audit snapshots. The
transaction metadata key `proposal_id` is reserved. Rejection/withdrawal is
audited as `proposal_reject`; resolved proposals cannot be approved again.
Posting/admin grants do not imply propose/approve. MCP equivalents are
`txn_propose`, `txn_proposals`, `txn_approve`, `txn_reject`; read-only mode retains
only listing. Schema v19 preserves prior policy and audit hashes. See
[identity, setup and limit semantics](../agent-identity.md#spend-limits-and-proposals).

## Securities and investment lots

Use `security create SYMBOL --name NAME --currency USD` to register a
company-scoped symbol, and `security list` to inspect it. Before trading, create
distinct investment and cash asset accounts plus a revenue account for gains.
`investment buy SYMBOL --quantity 1.5 --cost 150.00 --date 2026-01-02
--asset 1200 --cash 1010 --reference broker-buy-001` posts a purchase and creates
a lot atomically. `investment sell SYMBOL --quantity 0.5 --proceeds 60.00
--date 2026-02-02 --asset 1200 --cash 1010 --gain 4100` relieves FIFO lots and
posts cash, basis relief, and realized gain/loss. Losses debit the gain account.

Use `--method lifo`, or `--method specific --lot LOT_ID` for one explicit lot.
`investment lots SYMBOL` returns original/remaining quantity and basis.
Quantities are exact positive decimal strings, up to eight decimal places.
Costs include fees; proceeds are net of fees. Money and per-whole-share prices
use currency-native major units on input and minor units in JSON, with `_display`
companions. Partial basis and valuations round half-up; final disposal conserves
remaining basis. Duplicate references reject the whole trade. Record trades in
date order per symbol/account. Generic `txn reverse` rejects investment trades.

`security price SYMBOL --date 2026-02-02 --price 120.00` appends an immutable
price observation. `investment holdings --as-of 2026-02-02` reports each
symbol/account position and its latest price at the cutoff, market value, and
unrealized gain/loss without postings. The date defaults to today; missing prices
produce null valuations. Price dates are unique per security; zero is allowed.

MCP equivalents are `security_create`, `security_list`, `security_price`,
`investment_buy`, `investment_sell`, `investment_lots`, and `investment_holdings`.
Pass `company`; use string quantities/amounts and optional `method`, `lot`,
`reference`, and `as_of`. Read-only mode omits the four writes. Corrections,
corporate actions, wash sales, and tax forms are follow-ups. Keep database backups;
ledger export excludes investment tables. See [investment documentation](../investments.md).

## Agent Integration

**Use `--json` for programmatic command responses.** Responses use a uniform envelope, except for successful exports:

```json
{
  "ok": true,
  "meta": { "command": "...", "company": "...", "timestamp": "..." },
  "data": { ... }
}
```

Errors return `"ok": false` with an `"error"` object containing `"code"` and `"message"`.

`bk export --output-format json` writes a standalone `{"beankeeper_export": {"version": 1, "companies": [...]}}` object, even with `--json`. It has no `ok`, `meta`, or `data` wrapper. Read exported companies with `jq '.beankeeper_export.companies'`.

Key conventions for agents:
- Monetary JSON fields keep **integer minor units** and add `<field>_display` decimal strings in major units, such as `amount: 250000` with `amount_display: "2500.00"` for USD. Use the display companion when presenting amounts. A null display means the value or currency scale is unknown; never assume USD.
- `txn list` and `audit log` default to 50 rows. Check `meta.pagination.has_more` and advance `--offset` by the returned row count until false; keep the same filters and a positive limit. The data remains an array. `txn list --count` stays a count-only response.
- Set `BEANKEEPER_DB` and `BEANKEEPER_COMPANY` env vars to avoid repeating `--db` and `--company` on every call
- Use `--quiet` to suppress stderr status messages
- Use `--reference` + `--on-conflict skip` for idempotent transaction posting

For the complete JSON envelope specification, see [`references/json-api.md`](references/json-api.md).

## Agent identity and capability enforcement

For an encrypted ledger, the owner runs `bk key init` once. Supply an agent's
base64 Ed25519 public key with `bk --json agent issue --name claude
--public-key "$AGENT_PUBLIC_KEY" --caps ledger/read,ledger/post --ttl 8h`.
For local setups `--generate-key` replaces `--public-key` and prints a private
hex seed once. Save the returned token securely and set `BK_AGENT_TOKEN`, or
pass `--agent-token` globally (the flag wins). `--ttl` accepts positive `s`,
`m`, `h`, `d` values up to `30d`, default `8h`.

Supplied tokens are verified against this database's root and revocations;
invalid, expired or revoked tokens fail with `UNAUTHORIZED` (exit 6). New
transactions and audit rows include `agent_uri` and `token_jti`; JSON envelopes
add `meta.agent`. `bk audit log --agent URI` filters identity. `bk agent list`
shows public claims and revocation dates without secrets. `bk agent revoke
URI_OR_JTI` revokes one token or all current tokens for a URI, idempotently.
Revocation does not invalidate historical chain verification.

Enforcement defaults off. `bk policy show` returns `enforce` and `updated_at`.
Before enabling, issue and save an admin token, for example with
`--caps ledger/read,ledger/admin`. A named actor can bootstrap with
`bk policy set --enforce on` while off. Once on, every ledger command requires a
token; changing policy back to off requires `ledger/admin`. Changes are audited
as `policy_change`; repeated state is a no-op. Missing tokens are `UNAUTHORIZED`
(exit 6), and insufficient grants are `FORBIDDEN` (exit 7).

| Grant | Operations |
| --- | --- |
| `ledger/read` | List/show/report/export/verify/audit, agent list, policy show, loan schedule |
| `ledger/post` | txn post/import/attach/meta set/unset, investment buy/sell, security create/price, loan pay, schedule create/run/cancel, rule mutations |
| `ledger/propose` | Propose, list/withdraw owned pending requests |
| `ledger/approve` | List all requests, approve, reject |
| `ledger/reverse` | txn reverse |
| `ledger/reconcile` | txn clear/reconcile |
| `ledger/budget` | budget set/delete |
| `ledger/admin` | Company/account create/delete, key init, agent issue/revoke, agent limit set/clear, policy set, backup/restore |

Admin does not imply other grants; post does not imply reverse. Issue explicit
unions. Bare `ledger` is rejected, and narrower descendant grants cannot cover
parent operations. CLI and stdio may omit tokens when enforcement is off; HTTP
always requires a verified token. Tokens remain
bearer credentials without private-key possession or write signatures. The root
stays in the encrypted DB and full backups. Root recovery/rotation and signed
writes remain outside this feature. `init` rejects supplied tokens; an admin
must disable policy before `init --force` can replace an enforced ledger. An
existing encrypted ledger must be readable with its current passphrase.
`init --demo` honors the named actor. Back up before upgrading to schema v18:
old audit hashes are preserved, but only new binaries can append v3 audit rows.

For stdio MCP, supply the process token in the server's environment or startup
arguments. It is reverified on every tool call, so expiry/revocation takes effect
without a restart. Enforcement filters discovery by startup grants; restart stdio
after policy changes to refresh the tool list. HTTP accepts per-request bearer
headers and filters discovery by current credentials and policy. Both transports
check grants/policy on every call. `--read-only` hides all writes including `txn_reconcile` regardless of
grants. Keep token values out of tool arguments. See the
[key and agent command reference](references/commands.md#agent-identity).

## Using MCP

When a `bk` MCP connection is available, call its tools directly. The server runs
as `bk --db /absolute/path/books.db mcp serve` against an existing ledger. Add
`--read-only` to omit all mutation tools. Configure secrets at startup through
`BEANKEEPER_PASSPHRASE_CMD`, `--passphrase-file`, or a descriptor other than stdin;
never include a passphrase in tool arguments. The database and secret are fixed
for that server process.

For a shared Streamable HTTP endpoint, start `bk mcp serve --http`, optionally
with `--bind 127.0.0.1:PORT` (default `127.0.0.1:8750`). The endpoint is `/mcp`;
the actual bound address is printed to stderr. Every request requires a verified
`Authorization: Bearer <agent token>`, even when ledger enforcement is off.
`--agent-token` / `BK_AGENT_TOKEN` supplies a fallback only for absent headers.
Explicit invalid credentials never fall back and return HTTP 401 `UNAUTHORIZED`.
With enforcement on, discovery filters the current caller's grants; insufficient
grants return `FORBIDDEN`. Credentials and policy are rechecked per call, including
expiry and revocations. `--read-only` always removes writes. Non-loopback binds
require `--allow-remote`, which serves plaintext HTTP without TLS and is not a
hosted-tier implementation. Browser Origin headers are rejected. Prefer per-request
tokens for multiple agents; do not put credentials in tool arguments or URLs.

Rust host applications can merge `beankeeper_cli::mcp::http_router(&cli, read_only,
config)?` beside their own axum routes. It already mounts `/mcp` and retains the
same authentication and capability rules. The host supplies
`StreamableHttpServerConfig`, controls its cancellation token, and owns the
listener, runtime, shutdown, and Host/Origin rules. See the API Rustdoc example.

- Pass `company` explicitly for account, transaction, report, and budget tools.
- Use `company_list` / `company_show` and `account_list` / `account_show` to inspect the ledger before posting. `account_create` can add an account.
- Call `txn_post` with `debits` and `credits` arrays of `{account, amount, memo?}`. Amounts are decimal strings in major units. Supply a stable `reference` and `on_conflict: "skip"` for retries; only `error` and `skip` are accepted conflict strategies. `meta` is a map of string keys to string values; `tax` is an array of `account=category` strings.
- Call `txn_meta_set` with `company`, exactly one of `id` or `reference`, and `values: ["status=disputed", "dispute_id=D-42"]`. `txn_meta_unset` takes the same selector and `keys: ["status", "dispute_id"]`. Values are strings; relationship keys are reserved. These writes preserve financial fields and use one audited batch per change. Both tools are absent in read-only mode.
- Call `txn_reverse` with `company` and the original `id` to create a linked mirror posting; optional fields are `date`, `description`, and `reference`. Supply the same explicit `reference` on retries to get `skipped: true` and `existing_id`. Reconciled transactions cannot be reversed. Existing clearings stay on the original; reversal entries start uncleared. This write tool is absent in read-only mode.
- Call `txn_list` or `txn_show` to inspect posted entries. Use `report` with `kind` (for example `trial-balance`, `ledger`, or `cash-flow`) and the corresponding CLI options in snake_case.
- For `report` with `kind: "reconciliation"`, `mode` accepts `bank` (default) or `settlement`. Settlement mode requires `statement` and accepts `link_key` (default `applies_to`) for payment-to-charge metadata links.
- Use `security_create`, `security_list`, and `security_price` to register symbols, inspect them and record price observations. Trade with `investment_buy` / `investment_sell`, inspect basis with `investment_lots`, and value positions with `investment_holdings`. Quantities and input money are decimal strings; `company` is required. Read-only mode retains the three inspection tools and hides the four writes. See [securities and investment lots](#securities-and-investment-lots) for account setup and trade rules.
- Use `budget_list`, `budget_set`, `budget_delete`, and `txn_clear` for their CLI equivalents. Budget deletion is confirmed by calling the tool. `txn_clear.batch` accepts an in-memory array of `{reference, amount?}` rows with an `account`; it never reads a file or stdin.
- `txn_reconcile` checks for orphaned intercompany links without changing data. `verify` checks accounting and audit integrity; `audit_log` includes `prev_hash` and `row_hash`, and accepts `action: "create"` for posting attribution. Audited MCP writes use the hash-chained actor `mcp:<client>`.

Successful results contain the CLI JSON **data payload**, without its envelope.
Read JSON text content, or `structuredContent` when present. The SDK uses JSON text
for array payloads on protocol versions that require structured content to be an
object. Monetary fields include decimal-string `_display` companions alongside
integer minor units. Read pagination from the tool result’s `_meta.pagination`
for `txn_list` and `audit_log`; both accept `limit` and `offset`. Errors set `isError: true`
and return `{code, message}` with the CLI's existing error codes. Audited mutations
record `mcp:<sanitized client name>`; current creation-auditing behavior is unchanged.

Initialization, backup, restore, company creation/deletion, account deletion, import, export, and
attachments use the CLI. See the [MCP reference](references/commands.md#mcp) and
[client configuration examples](../../README.md#connect-an-ai-assistant-through-mcp).

## Period close and locking

Use `bk --company COMPANY period close --through YYYY-MM-DD` to freeze every
transaction date on or before the inclusive cutoff for that company, across
currencies. An existing cutoff may only advance. `period reopen --reason TEXT`
clears the entire cutoff and requires a nonempty explanation. Both changes are
audited and require `ledger/admin` under enforcement. `period list` requires
`ledger/read`; JSON returns the current state in an array, including the last
reopen with `closed_through: null`, or `[]` if never closed.

The shared posting path enforces locks for posts, imports, schedule runs, loan
payments, investment trades, reversals, and proposal approval. Rejected approvals
remain pending; reopening allows backdated posting with its usual audit record.
Idempotent skips remain allowed because they create no posting. Metadata and
clearance operations keep their existing behavior. Inspect history with
`audit log --entity period` (actions `period_close`, `period_reopen`).

MCP equivalents are `period_close`, `period_reopen`, and `period_list`, with
`company`, and respectively `through`, `reason`, or no additional arguments.
Read-only MCP retains only listing. Back up before schema v20 upgrades: previous
hashes, policy, and proposals remain valid, while new v4 audit records block
older binaries. `verify` detects mismatched or missing cutoff state.

## Command Overview

| Command | Purpose |
|---------|---------|
| `bk mcp serve` | Serve MCP stdio, or authenticated HTTP with `--http`; `--read-only` omits writes |
| `bk init` | Create a new database (`--demo` for sample data, `--encrypt` for passphrase) |
| `bk period close/reopen/list` | Close company posting dates, explicitly reopen with a reason, and inspect cutoffs |
| `bk company create/list/show/delete` | Manage companies |
| `bk account create/list/show/delete` | Manage chart of accounts |
| `bk txn post` | Record a balanced journal entry |
| `bk txn meta set/unset` | Annotate posted transactions by ID or reference without changing financial fields |
| `bk txn propose` | Submit a balanced posting for approval without posting it |
| `bk txn proposals` | List visible pending requests, optionally including resolved proposals |
| `bk txn approve/reject` | Resolve a pending proposal; `withdraw` aliases rejection of an owned request |
| `bk txn list` | Search and filter transactions |
| `bk txn reverse ID` | Post a linked mirror transaction (alias `void`); reject reconciled originals |
| `bk txn show ID` | Display a single transaction with entries, their cleared/remaining amounts and individual clearings |
| `bk txn import` | Import OFX/CSV bank statements or JSON journal batches |
| `bk schedule create/list/run/cancel` | Manage pending recurring templates and post due occurrences |
| `bk security create/list/price` | Register company securities and record dated prices |
| `bk investment buy/sell/lots/holdings` | Post trades, inspect lot basis and value holdings |
| `bk loan schedule/pay` | Calculate fixed-rate amortization or post one scheduled payment |
| `bk key init` | Create the local trust root inside an encrypted ledger |
| `bk agent issue/list/revoke` | Issue attestations, inspect claims and revoke credentials |
| `bk agent limit set/show/clear` | Manage per-agent, per-company/currency spend limits |
| `bk policy show/set` | Inspect or change capability enforcement |
| `bk rule create` | Persist a company-scoped description-to-account mapping |
| `bk rule list` | List categorization rules in evaluation order |
| `bk rule delete` | Remove a mapping without changing existing postings |
| `bk txn clear` | Clear entries (in full or in part) against a statement, by id, by reference, or in an atomic batch; mark reconciled |
| `bk txn attach` | Link documents (receipts, invoices) to transactions |
| `bk txn reconcile` | Find orphaned intercompany correlations |
| `bk budget set` | Create or update monthly/annual budgets |
| `bk budget list` | List budget entries |
| `bk budget delete` | Remove budget entries |
| `bk report trial-balance` | All accounts with debit/credit totals |
| `bk report balance` | Single account balance |
| `bk report income-statement` | Revenue and Expenses sections with subtotals and net income for a period |
| `bk report balance-sheet` | Assets, Liabilities, Equity sections (incl. computed retained earnings) and balance check as of a date |
| `bk report tax-summary` | Entries grouped by tax category |
| `bk report schedule-c` | Schedule C lines plus unmapped/untagged activity |
| `bk report 1099` | Vendor debit totals from payee metadata, filtered by threshold |
| `bk report budget-variance` | Budget vs actual comparison |
| `bk report ledger` | General ledger / account statement with running balances |
| `bk report aging` | Receivables/payables aging buckets (FIFO or specific invoice matching, by transaction date) |
| `bk report reconciliation` | Bank balances by default; `--mode settlement --statement ID` compares AR/AP charge applications with statement net, listing short pays, overpays and linked/unmatched credits. Supports metadata filters/grouping |
| `bk report cash-flow` | Cash flow statement (direct method) for a period |
| `bk report trend` | Show revenue, spending or net worth across periods |
| `bk audit log` | Inspect the audit trail of ledger mutations/deletions |
| `bk verify` | Check ledger integrity |
| `bk export` | Export accounting data as JSON or CSV |
| `bk backup --output PATH` | Save and verify a full database snapshot, preserving encryption |
| `bk restore --input PATH --db DEST` | Restore a snapshot and verify it before publishing |

For the full command reference with all flags and examples, see [`references/commands.md`](references/commands.md).

## Essential Workflows

### Back up and restore a ledger

```sh
bk --db books.db --passphrase-file /secure/books.key backup --output ledger.bk-backup
bk restore --input ledger.bk-backup --db restored.db --passphrase-file /secure/books.key
bk --db restored.db --passphrase-file /secure/books.key verify
```

Omit passphrase flags for plaintext databases. Backups preserve the original key,
root, token registry, audit chain, schedules, investments, and all other tables.
Both commands verify the staged snapshot before publishing it. `--output`,
`--input`, and restore's explicit `--db` are required; the environment/default DB
is not a restore destination. `--company` is ignored. Add `--force` to replace an
offline destination. Close its connections first: SQLite sidecars cause rejection.
Same-file aliases and symlink destinations are rejected. Failed validation keeps
existing destinations intact. Restore requires the current schema and performs
no migrations. JSON data has `path`, `bytes`, `schema_version`, and `encrypted`.

Back up receipt files in the external `attachments/` directory separately;
snapshots contain attachment records, not those files. Keep the original
passphrase separately. Use `bk export` for accounting interchange, not recovery.
See the [backup reference](references/commands.md#backup-and-restore).

### Set up a new ledger

```bash
bk init
bk company create acme "Acme Corp"
bk --company acme account create 1000 "Cash" --type asset
bk --company acme account create 2000 "Accounts Payable" --type liability
bk --company acme account create 3000 "Owner Equity" --type equity
bk --company acme account create 4000 "Revenue" --type revenue
bk --company acme account create 5000 "Rent Expense" --type expense
```

### Post a transaction

Debit and credit entries use the format `account_code:amount`:

```bash
bk --company acme txn post -d "Office rent" \
  --debit 5000:2500 --credit 1000:2500
```

Multi-line entries (e.g. payroll):

```bash
bk --company acme txn post -d "March payroll" \
  --debit 5300:5000 \
  --credit 2600:600 --credit 2800:382.50 --credit 1000:4017.50 \
  --tax 5300=payroll --tax 2600=payroll-tax
```

### Reverse a mistake

```bash
bk --company acme txn reverse 42 --date 2026-11-19 --reference VOID-42
```

`txn reverse` (alias `void`) flips every debit/credit and preserves accounts,
amounts, memos, tax categories and currency. The date defaults to today,
cannot precede the original, and `--description TEXT` overrides the default
`Reversal of #ID: ORIGINAL DESCRIPTION`. The reference defaults to
`reversal-of-ID`. An already reversed transaction is rejected, unless an
explicit `--reference` matches its linked reversal, in which case it is a
successful skip. JSON uses `txn.reverse` and the `txn post` result shape.

Both links are numeric metadata IDs: `reversed_by` on the original and
`reverses` on the reversal. They appear in `txn show` and `txn list`. Original
metadata is preserved; the new transaction carries only the reversal link,
without copying transaction metadata or attachments. The posting, links and
`reverse` audit record are atomic. Reconciled clearings block the operation;
other original clearings remain intact. New entries start uncleared and
appear as separate outstanding opposite postings in reconciliation from
the reversal date. Clear or reconcile them explicitly when appropriate.

### Scheduled and recurring transactions

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

### Import bank statements

```bash
bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000 \
  --on-conflict skip

bk --company acme txn import \
  --file statement.csv --account 1000 --suspense 9000 --dry-run

cat journals.json | bk --json --company acme txn import --input-format json
```

CSV defaults to `date,description,amount[,reference]`. Amounts are signed major
units; positive means inflow unless `--positive-amount outflow` is set. Map
headers with `--date-col`, `--description-col`, `--amount-col`, `--reference-col`,
and parse dates with `--date-format` (default `%Y-%m-%d`). Set `--currency` when
needed (otherwise `BEANKEEPER_CURRENCY`, then USD).

JSON is an array of `{date, description, entries, currency?, reference?,
metadata?}`. Each entry needs `account_code`, `direction` (`debit`/`credit`), and
`amount`, with optional `memo` and `tax_category`. Amounts are positive major
units; use decimal strings (e.g. `"12.34"`), or integer numbers for whole major
units. Fractional JSON numbers are rejected. Metadata must be an object. JSON
uses its own accounts and balanced entries; it does not need bank flags.

CSV/JSON batches commit atomically. `--dry-run` posts inside a rolled-back
transaction, checking intra-batch duplicates too. Default `--on-conflict skip`
skips duplicates; `error` rolls back the batch. Only `skip` and `error` are accepted.
Missing references use a normalized row hash plus an occurrence ordinal,
starting at 1 for each distinct content in the batch. Identical unreferenced
rows are all posted; re-importing the same or an overlapping file skips matching
occurrences and posts only additional ones. Duplicate explicit references still
skip or error according to `--on-conflict`.
See [commands.md](references/commands.md) for the full import contract.

### Memorize bank categorization

```bash
bk --company acme rule create --contains Starbucks --account 5000 --priority 10
bk --company acme rule list --json
bk --company acme rule delete <RULE_ID> --force
```

Rules automatically apply to future CSV and OFX/QFX imports, including dry runs.
Patterns are trimmed and matched as literal substrings after Unicode lowercase
conversion. CSV matches its description; OFX matches combined name/memo.
Higher priority wins (default `0`), then ascending rule ID. Rules are scoped to
one company, require an existing destination account, and replace only the
suspense side. They apply to both inflows and outflows; unmatched rows retain
`--suspense`. Account tax defaults apply. A rule targeting the import bank is
an error. JSON journal imports retain their explicit entries.

Creating/deleting rules never changes posted transactions. CSV references are
computed before rule selection, preserving duplicate detection across rule
changes. Delete and recreate a rule to change its mapping. Creation/deletion
are audited under entity `rule`, with actions `create` and `rule_delete` in JSON.
Accounts referenced by rules cannot be deleted until those rules are removed.
Rule management is available via CLI; MCP tools remain unchanged.

### Generate reports

```bash
bk --company acme report trial-balance
bk --company acme report income-statement --from 2026-01-01 --to 2026-03-31
bk --company acme report balance-sheet --to 2026-03-31
bk --company acme report tax-summary --from 2026-01-01 --to 2026-12-31
bk --company acme report ledger --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report cash-flow --from 2026-01-01 --to 2026-03-31

# Companies with transactions in several currencies must pick one
bk --company acme report trial-balance --currency MXN

# Multi-column views (income-statement and trial-balance; need --from and --to)
bk --company acme report income-statement --from 2026-01-01 --to 2026-06-30 --period month
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-year
```

`--period month|quarter|year` gives one column per period plus a Total (and a
Net Income row per period on the income statement). `--compare
prior-period|prior-year` adds the earlier range with change and change %
(`null`/`n/a` when the prior amount is 0). The two options conflict. Amounts
are net on each account's normal side. JSON command ids are
`report.<report>.periods` and `report.<report>.compare`; see
[`references/commands.md`](references/commands.md) and
[`references/json-api.md`](references/json-api.md).

Reports never mix currencies. Without `--currency`, the report currency is
inferred from the company's transactions (USD if there are none); if the
company uses more than one currency the command fails with a `VALIDATION`
error listing them, and `--currency CODE` is required.

### Tax-oriented exports

Use `report schedule-c` with canonical, case-insensitive `sched-c:<line>` entry
tags. Inspect all three arrays: `lines`, `unmapped`, and `untagged`. Untagged
income/expense activity stays visible. Use `report 1099` with transaction
`--meta 'payee=Name'`, optional string `tin` and `payee_address`; total is the
transaction's full debit amount, with split transactions counted once. Conflicting
nonempty details per payee fail validation. The default `--threshold 600` is in
major currency units; `--threshold 0` lists everyone. Dates and currency follow
`tax-summary`. Use `--json`, `--format csv`, or `--output-format csv`.
MCP `report` accepts kinds `schedule-c` and `1099` with optional `from`, `to`,
`currency`, and decimal-string vendor `threshold`. Trace payments with
`txn list --filter-metadata 'payee=Name'`. These are bookkeeping summaries,
not tax advice or filed forms. See [tax export conventions](../tax-exports.md)
for signs, line meanings, metadata rules, and output fields.

### Receivables / payables aging

```bash
bk --company acme report aging --account 1100                     # AR as of today
bk --company acme report aging --account 2000 --as-of 2026-03-31  # AP
bk --company acme report aging --account 1100 --buckets 15,45 --group-by-metadata customer
bk --company acme report aging --account 1100 --group-by customer --detail   # list open invoices
bk --company acme report aging --account 1100 --method specific              # honour applies_to links
bk --company acme report aging --account 1110 --account 1120                 # several sub-ledgers
bk --company acme report aging --account 1100 --include-children            # 1100 plus 11xx children
bk --company acme report aging --account 1100 --filter-metadata status=disputed
```

There is no invoice or due-date model: each net increase to the account is a
charge, each net decrease a settlement, and ages are measured from transaction
dates. Asset accounts (AR) age debit-side charges and apply credit-side
settlements; liability accounts (AP) the reverse.

With several accounts, each is matched on its own (totals equal the sum of
single-account runs; a link to a charge on another account is reported as
`other_account`), and all must be AR or all AP. `--include-children` adds every
account whose code starts with the given code minus trailing zeros. With
`--group-by`, counterparties are grouped per account unless
`--group-across-accounts` is set; JSON has per-account `accounts[]` and the
combined `groups[]`. `--filter-metadata` keeps only matching open charges,
after all settlements were applied, so tagged items (e.g. `status=disputed`)
keep their original age; unapplied credit is not shown under a filter.

- To age per customer or vendor, post with JSON metadata such as
  `-m '{"customer":"Globex"}'`. Missing keys and non-JSON metadata go to
  `(unassigned)`.
- Settlements are applied FIFO by default. To pay a specific invoice, post the
  invoice with a reference (`-r INV-102`) and the payment, credit note or
  reversal with `-m '{"applies_to":"INV-102"}'` (a transaction id also works),
  then run with `--method specific`. Excess and unlinked amounts fall back to
  FIFO; broken links are warned about and listed in `data.unmatched_links`.
- Overpayments become unapplied credit: `net` is negative and `net_credit` is
  `true`. Refunds and chargebacks are charges that consume it.
- `--as-of` ignores later-dated transactions, so past period ends are stable.

### Reconcile an account against a statement

```bash
# Mark the entries that appear on the statement as cleared
bk --company acme txn clear 42 --entry 5

# Compare the cleared balance to the statement ending balance
bk --company acme report reconciliation --account 1000 --to 2026-03-31 \
  --statement-balance 1234.56
```

Balances are on the account's normal side (debit-normal for asset/expense,
credit-normal otherwise). `book - total cleared` always equals the total of the
listed uncleared entries' remaining amounts; `difference = statement - cleared`,
and the account is reconciled when the difference is zero.

Entries can be cleared **in parts**, each clearing with an amount, a date and a
statement id. Clear by transaction reference instead of ids, or a whole
statement from a file/stdin (atomic; re-running is a no-op because each
entry is cleared at most once per statement):

```bash
# Partial payment of one item
bk --company acme txn clear --reference POL-123 --account 1200 \
  --amount 40 --date 2026-03-31 --statement STMT-03

# A whole statement: one `reference[,amount]` per line (no amount = full remaining)
printf 'POL-123,40\nPOL-124\n' | bk --json --company acme txn clear \
  --from-file - --account 1200 --date 2026-03-31 --statement STMT-03

# Check that statement alone, and list what is still open by carrier
bk --company acme report reconciliation --account 1200 --statement STMT-03 --statement-balance 65
bk --company acme report reconciliation --account 1200 --group-by-metadata carrier \
  --filter-metadata region=west
```

Clearing output uses currency units and describes partial clears as `partially cleared`.
JSON retains existing status values and adds `partially_cleared` and each item’s
`currency`. Mixed-currency batch totals have a null currency and display companion.

`--to` is point-in-time: clearings count as cleared when cleared on or before it,
and as reconciled only when also reconciled on or before it (`--status reconciled
--date D` records reconciled date `D`). Over-clearing, a `--date` before the
transaction date, and a reconciled date before the cleared date are `VALIDATION`
errors. Reconciled clearings are locked: `--status cleared` on them fails; undo
with `--status uncleared`, then clear again. Outstanding items carry `reference`,
`metadata`, `cleared_amount`, `reconciled_amount`, `remaining_amount` and
`clearings`; `bk txn show` lists every entry's clearings. See the statement-reconciliation recipe
in [`references/workflows.md`](references/workflows.md).

`report cash-flow` treats asset accounts named like `cash`/`bank`/`checking`/`savings` as cash unless you pass `--cash-account CODE` (repeatable); see [`references/commands.md`](references/commands.md#bk-report-cash-flow).

### Close a carrier commission statement with a short pay

Use settlement mode for payments against expected commissions. In an existing
agency ledger with Cash 1000, Commission AR 1210 and Revenue 4000:

```sh
bk --company agency txn post -d 'Expected P1 October' --date 2026-10-01 \
  --debit 1210:1800 --credit 4000:1800 --reference P1-2026-10 \
  --meta policy=P1
bk --company agency txn post -d 'S1 payment P1' --date 2026-10-25 \
  --debit 1000:1620 --credit 1210:1620 --reference PAY-S1-P1 \
  --meta applies_to=P1-2026-10
bk --company agency txn clear --reference P1-2026-10 --account 1210 \
  --amount 1620 --date 2026-10-31 --statement S1
bk --company agency --json report reconciliation --mode settlement \
  --account 1210 --statement S1 --statement-balance 1620 --to 2026-10-31
```

The statement comparison is reconciled (`difference: 0`), while the original
October 1 charge remains open for $180, aged 30 days. The linked $1,620 payment
appears in `applied_settlements`, regardless of whether its entry was cleared.
Clear payment entries too if useful; they do not net away charge applications.
Inspect `charges[].short_pay` and `charges[].overpay` as well as `reconciled`.
Unlinked or unusably linked credits appear in `unmatched_settlements` with
reasons and never reduce open charges through FIFO. Charge reversals reduce
expected commissions and appear separately as negative charges. Use `--link-key`
for another metadata key. Filters and grouping select original charge metadata.
The account-wide explanations include all statements through `--to`; the
comparison uses only this statement's charge clearings. Bank mode is the default.

### Historical trends

```bash
bk --company acme report trend --metric spending --from 2026-01-01 --to 2026-12-31
bk --company acme report trend --metric net-worth --from 2026-01-01 --to 2026-12-31
```

Use `--period month|quarter|year` (default month). `--to` defaults to today and
`--from` to January 1 of the end year; dates are inclusive and partial periods
are clipped. Maximum 120 periods. Spending categories are expense accounts:
net debits per period, including refunds and zero-activity accounts, plus Total
Spending. Net worth is cumulative assets minus liabilities at each period end,
including history before `--from`. Empty periods preserve zero activity or
carry balances forward. Net worth uses book values, and spending means booked
expenses rather than cash payments. Select `--currency CODE` for multi-currency
companies. JSON uses `columns` with `label/from/to` and `series` with `code/name`,
`amounts` (minor units), and `amounts_display` (major units). CSV is
`code,name,<period labels>` with minor-unit integers. No sum of period-end
balances is emitted. MCP `report` accepts `kind: "trend"`, required `metric`
(`spending` or `net-worth`), and optional `from`, `to`, `period`, `currency`.

### Loan amortization and payment splits

```bash
# Calculate without a company or database
bk loan schedule --principal 200000 --rate 6 --months 360 --currency USD --json

# Existing accounts: 2000 liability, 5100 expense, 1000 asset
bk --company acme loan pay --principal 200000 --rate 6 --months 360 \
  --payment-number 1 --liability 2000 --interest-account 5100 \
  --paying-account 1000 --date 2026-11-20 --reference mortgage-2026-11 --json
```

The calculator assumes monthly payments and nominal annual interest divided by
twelve. `--rate` is a percentage from 0 to 100, with at most four decimal places;
`--months` is 1 through 1,200. Principal uses major currency units. Currency is
`--currency`, then `BEANKEEPER_CURRENCY`, then USD. Payments and interest round
half up in minor units, with the last payment clearing the remainder. Minimum
regular payment is one minor unit; small loans can pay off early.

`loan pay` debits principal to a liability, interest to an expense, and credits
the total to an asset, skipping zero-value entries. It uses the audited posting
path, actor/agent attribution, account tax defaults, and reference duplicate
detection. Reuse one unique reference per loan payment on every retry; add
`--on-conflict skip` to return the existing transaction. JSON command IDs are
`loan.schedule` and `loan.pay`; pay returns the same created/skipped payload as
`txn post`. Inspect the resulting split with `txn show`.

There are no persistent loan records. Always reuse the original principal,
rate and term with an explicit payment number. Verify the intended payment
against the lender's statement: this helper does not infer payment order,
check the liability balance, handle extra payments or calculate daily interest.
Fees and variable rates are outside this feature. Schedule needs no ledger
unless an agent token is supplied, in which case normal token verification
opens the database. Both commands are CLI-only; MCP tools are unchanged.

### Budgeting

```bash
# Set an annual budget (evenly distributed across 12 months)
bk --company acme budget set 5000 --year 2026 --annual 30000

# Set a specific month's budget
bk --company acme budget set 4000 --year 2026 --month 3 --amount 10000

# Compare budget vs actual
bk --company acme report budget-variance --year 2026
bk --company acme report budget-variance --year 2026 --from 1 --to 6 --type expense
```

### Intercompany transactions

```bash
# Post in company A
bk --company acme-consulting txn post -d "Invoice to Products" \
  --debit 1500:5000 --credit 4000:5000

# Post correlated entry in company B (links bidirectionally)
bk --company acme-products txn post -d "Payment to Consulting" \
  --debit 5400:5000 --credit 2500:5000 --correlate 15

# Check for broken links
bk txn reconcile
```

### Metadata

Transaction `metadata` is always `null` or a JSON object. `-m "free text"` is
stored as `{"memo":"free text"}`; `-m '{"k":"v"}'` is stored unchanged.
`--meta key=value` (repeatable) adds top-level string keys; duplicates are a
validation error. Filter with `--filter-metadata key=value` (exact, AND-ed), the
same rule on `txn list` (where `--meta` is an alias), `report ledger`,
`report reconciliation` and `report aging`; or `txn list --metadata TEXT`
(substring). Strings match exactly, numbers and booleans by their JSON text
(`qty=3` matches `{"qty":3}`); null, arrays, objects and non-JSON never match.

```bash
bk --company acme txn post -d "AWS bill" --debit 5000:120 --credit 1000:120 \
  -m "Q1 cloud" --meta vendor=AWS --meta project=apollo
bk --company acme txn list --filter-metadata vendor=AWS --json
bk --company acme report ledger --account 5000 --filter-metadata project=apollo
```

For additional workflow patterns including bank reconciliation and tax reporting, see [`references/workflows.md`](references/workflows.md).

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `BEANKEEPER_DB` | Database file path | `beankeeper.db` |
| `BEANKEEPER_COMPANY` | Default company slug | (none) |
| `BEANKEEPER_CURRENCY` | Default currency code (for `report budget-variance`, inferred from transactions when unset) | `USD` |
| `BEANKEEPER_PASSPHRASE_CMD` | Command to obtain encryption passphrase | (none) |
| `BK_AGENT_TOKEN` | Optional agent attestation, overridden by `--agent-token` | (none) |
| `BK_ACTOR` | Poster and audit principal, overridden by `--actor` | OS user, else `cli` |
| `NO_COLOR` | Disable colored output | (unset) |

## Error Handling

All errors return a non-zero exit code and, in JSON mode, a structured error:

| Exit Code | Meaning |
|-----------|---------|
| 0 | Success |
| 2 | Usage / argument error |
| 3 | Validation error (unbalanced transaction, invalid account) |
| 4 | Database error |
| 5 | Not found |
| 6 | Unauthorized agent token |

Validate before posting: ensure account codes exist (`bk account list`), amounts are positive, and total debits equal total credits. The CLI enforces balance at post time and will reject unbalanced entries.

## Reference Files

For detailed information beyond this overview:

- **[`references/accounting.md`](references/accounting.md)** -- Account types, normal balances, the accounting equation, and double-entry rules
- **[`references/commands.md`](references/commands.md)** -- Complete command reference with all flags, arguments, and examples
- **[`references/json-api.md`](references/json-api.md)** -- JSON envelope format, field types, and programmatic usage patterns
- **[`references/workflows.md`](references/workflows.md)** -- Multi-step workflow recipes for common bookkeeping tasks
