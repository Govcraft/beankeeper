# Command Reference

For beankeeper-cli 0.27.0 (`bk`).

All commands accept global options: `--db PATH`, `--company SLUG`, `--json`, `--format {table|json|csv}`, `--quiet`, `--verbose`, `--no-color`, `--actor NAME`, `--agent-token TOKEN`. General options work before or after the subcommand, for example `bk report trial-balance --format json`. On import/export, use the distinct file-format options described below.

`--actor NAME` sets the label recorded in the audit trail. The CLI and MCP fall
back to `BK_ACTOR`, then the verified agent URI. Without any of those, the CLI
uses the OS user or `cli`, and MCP uses a sanitized `mcp:<client>` label or
`mcp:unknown`. See [Audit](#audit).
`--agent-token TOKEN` / `BK_AGENT_TOKEN` optionally records verified local identity;
invalid, expired or revoked supplied tokens return `UNAUTHORIZED` (exit 6).
Tokens are optional while enforcement is off (the default). With enforcement on,
missing credentials return `UNAUTHORIZED` and insufficient grants return
`FORBIDDEN` (exit 7). See [capability policy](#capability-policy).


## Database

### `bk init`

Create a new accounting database.

```bash
bk init                          # Create beankeeper.db in current directory
bk init --encrypt                # Create with passphrase encryption
bk init --path /data/books.db    # Create at specific path
bk init --demo                   # Populate with sample multi-company data
bk init --force                  # Overwrite existing database
```

`init --demo` creates three companies with fixed Q1 2025 data, including budgets,
a rule, a pending rent schedule, investment lots, tax tags and payee metadata,
a funded loan with its first payment, and a short-pay settlement. Use explicit
2025 report dates; see the README Demo Mode walkthrough.

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

When enforcement is on, backup and restore require `ledger/admin` on the source
ledger or snapshot. Existing destinations require `--force` and must be readable
with the supplied passphrase (or unencrypted), at the current schema, and have
enforcement off. Disable destination enforcement with its own admin token before
restoring. Close all connections to a destination before replacing it; SQLite
`-wal`, `-shm`, or `-journal` files cause rejection.
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
registry, audit history, budgets, rules, schedules, investments, and attachment
records. Receipt and document **files** are stored outside SQLite in the
`attachments/` directory beside the source database. Back up that directory
separately and preserve the paths referenced by attachment records when moving
machines. External attachment storage changes are outside these commands.
`bk export` remains an accounting JSON/CSV export, not a recovery backup.
Backup and restore are CLI commands and are not exposed over MCP.

### `bk verify`

Verify the entire ledger and show the evidence. The report includes per-currency
balances for each company, the recomputed audit head, row counts, and the outcome
of each check. A passing verification exits 0. A failed check exits 4 and names
the check and the affected record where available.

```bash
bk verify
bk --json verify
```

The MCP `verify` tool returns the same evidence as the CLI JSON `data` field.
See [verify JSON](json-api.md#verify).

### `bk export`

Export all data as JSON or CSV.

```bash
bk export --output-format json                # JSON to stdout
bk export --output-format csv --output backup.csv  # CSV to file
```

Successful JSON exports write a standalone `{"beankeeper_export": {"version": 1, "companies": [...]}}` object rather than the command-response envelope, even with `--json`. Use `jq '.beankeeper_export.companies'` to read the exported companies.

`--output-format {json|csv}` selects the export file format. The legacy local
`export --format` spelling remains supported as a hidden alias. For import and
export commands, place general response `--format` before the subcommand (or use
`--json`) so it is distinct from the local file format. For example,
`bk --format json txn import --input-format ofx --file statement.ofx --account 1000 --suspense 9000`.

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

## Financial summary

`bk --company SLUG summary [--from DATE] [--to DATE] [--compare-from DATE
--compare-to DATE] [--currency CODE] [--cash-account CODE ...]
[--receivable-account CODE ...] [--payable-account CODE ...] [--json]`

Provides advisor-ready facts and chartable data without changing ledger records.
The matching `financial_summary` MCP tool accepts `company` plus snake_case CLI
options and returns the full CLI JSON envelope. See the [data contract, comparison
rules and attention limits](../../financial-summary.md).

## MCP

### `bk mcp serve [--read-only] [--http [--bind ADDRESS] [--allow-remote]]`

Serve an existing database over MCP stdio by default, or authenticated Streamable
HTTP at `/mcp` with `--http`. Stdout contains only JSON-RPC in stdio mode and stays
empty in HTTP mode; diagnostics go to stderr. No subprocess invocation of `bk`
is used for tools.

```bash
bk --db /absolute/path/books.db mcp serve
bk --db /absolute/path/books.db mcp serve --read-only
bk --db /absolute/path/books.db --passphrase-file /secure/passphrase mcp serve
BEANKEEPER_PASSPHRASE_CMD='secret-tool lookup service beankeeper ledger books' bk --db /absolute/path/books.db mcp serve
```

Database and passphrase sources are resolved once at startup. `BEANKEEPER_DB`
works as usual. Missing databases and wrong passphrases are startup failures.
Passphrase file descriptor 0 is rejected in stdio mode because stdin carries MCP
requests; HTTP can use it for startup passphrase resolution.
Passphrases and database paths are never tool inputs. Company-scoped tools require
an explicit `company` argument, regardless of the CLI's default company.

HTTP defaults to `127.0.0.1:8750`; `--bind 127.0.0.1:0` selects a free port.
The bound endpoint is printed to stderr. `--bind` and `--allow-remote` require
`--http`. Non-loopback addresses require `--allow-remote`, which serves plaintext
HTTP without TLS and does not implement a hosted tier.

Every HTTP request must supply `Authorization: Bearer <agent token>` or use the
startup ledger's process-level `--agent-token` / `BK_AGENT_TOKEN` fallback.
Explicit headers take precedence, including invalid headers. Verification is mandatory even with policy
enforcement off; missing, expired, revoked, malformed, or untrusted credentials
return HTTP 401 with `{code: "UNAUTHORIZED", message}`. The HTTP 401 challenge is
`WWW-Authenticate: Bearer`; OAuth and discovery are not implemented. With enforcement
on, `tools/list` filters the current request's grants, and a direct call without the
required grant returns an MCP tool error with `code: "FORBIDDEN"`. Session IDs never
replace authentication. Policy and credentials are rechecked on each tool call.
`--read-only` removes writes independently of credentials.

Loopback binds validate Host headers; specific remote binds additionally accept
the bound IP. Remote wildcard binds accept any Host. Browser Origin headers are
rejected. See the [HTTP client example](../../../README.md#streamable-http) and
[identity details](../../agent-identity.md#mcp-sessions).

### Select a ledger

- Use repeatable `--ledger TARGET=PATH` bindings to name local database paths.
  Names start with a lowercase ASCII letter, contain only lowercase ASCII
  letters, digits, and hyphens, and have at most 64 characters. Each name is
  unique. `startup` is reserved for `--db` or its existing default.
- Use `--ledger-target TARGET` with a local command or `mcp serve` over stdio.
  For example, `bk --ledger demo=demo.db --ledger-target demo --json verify`.
- In HTTP mode, the server opens all named bindings with the resolved startup
  passphrase. Send `Beankeeper-Ledger-Target: TARGET` and a bearer from that ledger
  with each request. Reuse the MCP session ID. A missing header selects the
  startup ledger and the token fallback applies there. Unknown targets return
  HTTP 404 `NOT_FOUND`. Invalid or duplicate target headers return HTTP 400
  `VALIDATION`. Multi-ledger endpoints return HTTP 405 for SSE replay requests.

### Available tools

| Tool | Arguments / behavior |
|------|----------------------|
| `ledger_setup_begin`, `ledger_setup_finish`, `ledger_setup_revoke` | Use a live owner Admin credential to issue or end an encrypted ledger's setup session |
| `ledger_setup_book_create`, `ledger_setup_book_initialize`, `ledger_setup_book_update`, `ledger_setup_chart_apply`, `ledger_setup_account_add` | Apply book, profile, chart, and account setup requests using a live generation and request reference |
| `ledger_setup_opening_preview` | Preview checked opening entries and totals with an active setup credential |
| `ledger_setup_opening_reserve` | Hold a previewed opening journal and return its approval proposal ID |
| `ledger_setup_statement_preview` | Freeze bounded statement content with a setup generation and request reference |
| `ledger_setup_statement_commit` | Approve the exact batch with `company`, `batch_id`, and `preview_sha256` |
| `ledger_setup_statement_receipt` | Read audited import provenance with `company` and `batch_id` |
| `period_close`, `period_reopen`, `period_list` | `company`; close requires `through` (YYYY-MM-DD), reopen requires nonempty `reason`; close/reopen require `ledger/admin` under enforcement |
| `company_list`, `company_show` | List companies; show accepts `slug` |
| `company_create` | `slug`, `name`, optional `description`. Uses the same audited insert as CLI creation and setup book creation |
| `company_delete` | `slug`. This call confirms deletion, records the prior company in the audit log, and returns `{"deleted":"slug"}`. Returns `VALIDATION` for retained data. Requires `ledger/admin` under enforcement |
| `account_list`, `account_show` | `company`; list accepts `account_type` and `name`. Set `with_balances: true` for balances, with optional `from`, `to`, and `currency`; show accepts `code` |
| `account_create` | `company`, `code`, `name`, `account_type`, optional `default_tax_category` |
| `txn_post` | `company`, `description`, `debits`, `credits`; optional `date`, `currency`, `reference`, `on_conflict`, `metadata`, `meta`, `tax`, `correlate` |
| `txn_propose` | Same posting arguments as `txn_post`; requires a verified proposer, reserves the reference and waits for approval without posting |
| `txn_proposals` | `company`, optional `include_rejected` (includes all resolved statuses); visibility follows identity and grants |
| `txn_approve`, `txn_reject` | `company`, proposal `id`; reject accepts optional `reason`; approval posts atomically, rejection also serves proposer withdrawal |
| `txn_meta_set`, `txn_meta_unset` | `company`, exactly one of `id` or `reference`; set takes `values: ["key=value", ...]`, unset takes `keys: ["key", ...]`; values are strings; reserved relationship keys are rejected |
| `txn_reverse` | `company`, original transaction `id`; optional `date`, `description`, `reference` |
| `txn_list`, `txn_show` | `company`; list accepts CLI filters in snake_case, including `limit`, `offset`, `count`, and repeated `meta` filters; show accepts `id` |
| `financial_summary` | Read-only advisor facts and comparisons; `company`, optional dates, currency and account selector arrays; returns the full `bk summary` JSON envelope |
| `ledger_setup_status` | Read ledger readiness, including company, account, and transaction counts and whether the approval key is configured. Takes no arguments. |
| `ledger_setup_book_show` | `company`; returns the current profile and chart of accounts |
| `report` | `company`, `kind`, and that report's CLI options in snake_case; reconciliation accepts `mode` (`bank` default or `settlement`), `statement`, and settlement `link_key` (default `applies_to`) |
| `budget_list`, `budget_set`, `budget_delete` | `company` and CLI budget options; deletion is confirmed by the call |
| `txn_clear` | `company`, `transaction_id` + `entry`, or `reference` + `account`, or `batch` + `account`; optional `amount`, `date`, `statement`, `status` |
| `txn_reconcile` | No arguments; checks orphaned intercompany links without changing data; requires `ledger/reconcile` under enforcement and is omitted by `--read-only` |
| `verify` | No arguments; verifies ledger integrity |
| `security_create` | `company`, `symbol`, `name`, optional `currency` (default USD) |
| `security_list` | `company`; list registered securities |
| `security_price` | `company`, `symbol`, `date`, decimal major-unit `price` per whole share |
| `investment_buy` | `company`, `symbol`, decimal-string `quantity` and `cost`, `date`, `asset`, `cash`, optional `reference` |
| `investment_sell` | `company`, `symbol`, decimal-string `quantity` and `proceeds`, `date`, `asset`, `cash`, `gain`; optional `method` (`fifo` default, `lifo`, `specific`), `lot`, `reference` |
| `investment_lots` | `company`, `symbol`; inspect original/remaining quantity and cost basis |
| `investment_holdings` | `company`, optional `as_of` (default today); value positions at the latest price on or before the cutoff without posting |
| `audit_log` | Optional `company`, `entity`, `action` (including `create`), `limit`, `offset`; rows include `prev_hash` and `row_hash` |

`txn_post.debits` and `.credits` contain `{account, amount, memo?}` objects.
`amount` is a decimal string in major units. `meta` is a string-to-string map;
`tax` is an array of `account=category` strings. Use `reference` with
`on_conflict: "skip"` for safe retries returning the existing transaction ID.
`error` preserves duplicate rejection. Only `error` and `skip` are accepted.

`txn_reverse` creates a linked mirror posting with the CLI reversal rules. Its
`date` defaults to today and must be on or after the original date. Description
defaults to `Reversal of #ID: ORIGINAL DESCRIPTION`; reference defaults to
`reversal-of-ID`. Safe retries require the same **explicit** `reference`, returning
`{created: false, skipped: true, existing_id}`. A first reversal returns
`{created: true, skipped: false, id}`. Reconciled originals cannot be reversed;
existing clearings stay on the original and reversal entries start uncleared.
Use `audit_log` with `action: "reverse"` to inspect the audit record.

MCP monetary results include the same `_display` companions as CLI JSON.
`txn_list` and `audit_log` expose pagination in the tool result’s `_meta.pagination`
while retaining their data arrays.

`txn_clear.batch` contains `{reference, amount?}` rows, where `amount` is an optional decimal
string in major units (omit it to clear the remaining amount); the batch commits atomically. File and stdin inputs are
not exposed to tools.

### Setup sessions

Use `bk setup status` or `ledger_setup_status` to read readiness. The owner
authenticates with `ledger/admin` and calls `ledger_setup_begin` with `name`, the agent's
base64 Ed25519 `public_key`, and an optional `ttl`. The default lifetime is 15
minutes and the maximum is one hour. The equivalent CLI command is
`bk --json setup begin NAME --public-key KEY --ttl 15m`.

The response contains the public `session` and the one-time `credential` with
its bearer token. Store only the setup credential with the agent. It carries
`ledger/setup` and can read setup readiness. Setup writes require the active
`session.generation` in the same encrypted ledger.

The owner ends the session with `bk setup finish GENERATION` or
`bk setup revoke GENERATION`. The matching MCP calls take a positive integer
`generation`. Repeating the same terminal action returns the existing session.
Its credential is revoked when the terminal state commits.

### Books, profiles, and charts

Use `bk --json setup book-show COMPANY` or `ledger_setup_book_show` with
`company` to read the book name, saved profile, and current accounts. The
profile includes the current revision, preceding revision, purpose, base
currency, and fiscal-year start. Existing books without a saved profile
return `profile: null`. Both a read credential and an active setup credential
can call the tool.

A personal book and a business book use separate company slugs. Supply
`target` with `company`, `generation`, and a stable `reference` of up to 128
bytes on each MCP request. Reuse the same reference with the same inputs to
recover the original response. Changing the inputs under that reference
returns `VALIDATION`.

| Tool | Additional inputs |
|------|-------------------|
| `ledger_setup_book_create` | `name` and `settings` containing `purpose` (`personal` or `business`), `base_currency` and `fiscal_year_start` (`MM-DD`) |
| `ledger_setup_book_initialize` | `settings` for an existing company without a profile |
| `ledger_setup_book_update` | `supersedes` with the current profile revision and the new `settings` |
| `ledger_setup_chart_apply` | `template`, either `personal-v1` or `business-v1`, matching the book's purpose |
| `ledger_setup_account_add` | `code`, `name`, and `account_type` |

Supply the active setup credential through the usual agent token option to
create a calendar-year personal book in USD.

```bash
bk --json setup book-create home "Home" --generation 1 --reference home-book \
  --purpose personal --base-currency USD --fiscal-year-start 01-01
bk --json setup chart-apply home personal-v1 --generation 1 --reference home-chart
bk --json setup account-add home 1010 "Chase Checking" --type asset \
  --generation 1 --reference home-checking
```

Use `bk setup book-initialize COMPANY` with the same setting flags and
a stable reference when the company already exists. It adds the first profile
while preserving the company name, its accounts, and its journal.

Use `bk setup book-update COMPANY --supersedes REVISION` with the new
profile flags, active generation and a fresh reference to record a correction.
The chart operation commits all its accounts together. Choose a separate
account code for each real account.

### Opening journal preview

Call `ledger_setup_opening_preview` with the book's `company`, the active
`generation`, an explicit `date` and `currency`, and nonempty `debits` and
`credits`. Each line takes an `account` code, an `amount` string in major
units, and an optional `memo`. The description defaults to Opening balances.

```bash
bk --json setup opening-preview home --generation 1 --date 2026-01-01 \
  --currency USD --debit 1010:125.50 --credit 3000:125.50
```

The preview returns the profile revision, resolved account names, directions,
memos, tax categories, balanced totals, and a `fingerprint`. Every amount contains an exact
`amount_minor` string and an `amount_display` string in major units.

Call `ledger_setup_opening_reserve` with the same preview input under
`opening`, the returned `fingerprint`, and a stable `reference`. The
equivalent CLI command is `bk setup opening-reserve` with the preview flags
plus `--reference` and `--fingerprint`. The result includes the reference,
proposal ID, original setup generation, proposal creation time, and preview.
An exact retry returns the same result. A changed request under the same
reference returns `VALIDATION`.

The owner reviews the held proposal and uses `bk txn approve PROPOSAL_ID`
or `txn_approve` with a live `ledger/approve` credential for a different
identity. This approval requirement applies under either ledger policy and
after the setup session ends.

### Statements during setup

The setup agent calls `ledger_setup_statement_preview` with `company`,
`generation`, a stable `reference`, `bank`, `suspense`, `currency`,
`format`, and UTF-8 `content`. Use `format: "csv"` for CSV and
`format: "ofx"` for OFX or QFX. CSV accepts an optional `csv` mapping.
The content limit is 1 MiB and the row limit is 5,000. The ledger stores the
normalized preview and SHA-256 of the source without storing the source file.
Repeating the same request within the generation returns the frozen preview.
Changing the request under the same reference returns `VALIDATION`.

Review the entries, then pass the returned `batch_id` and `preview_sha256`
with `company` to `ledger_setup_statement_commit`. The first commit requires
the active setup generation and a different identity with a live
`ledger/approve` grant. It posts all rows in one transaction. An approver
repeating the exact commit can read the original receipt after setup ends.

Call `ledger_setup_statement_receipt` with `company` and `batch_id` to recover
the reference, format, source and preview digests, approver attribution, and
two arrays of `{reference, transaction_id}` records. `created` identifies the
transactions posted by this batch. `preexisting` identifies previously
posted matches. Use the normal audited reversal command for any created
transaction the owner chooses to correct.

```bash
bk --json setup statement-preview COMPANY statement.csv --generation GENERATION \
  --reference STATEMENT-REF --bank BANK --suspense SUSPENSE --currency USD --statement-format csv
bk --json setup statement-commit COMPANY BATCH-ID --preview-sha256 REVIEWED-DIGEST
bk --json setup statement-receipt COMPANY BATCH-ID
```

Issue the second command with the owner's approval credential. Use a local
file or `-` for stdin in the CLI. Supply bounded content directly to MCP.

### Report kinds

`report.kind` accepts `trial-balance`, `balance`,
`income-statement`, `balance-sheet`, `tax-summary`, `schedule-c`, `1099`, `ledger`, `budget-variance`,
`cash-flow`, `aging`, `reconciliation`, or `trend`, including their period/comparison and
metadata options. These are the 13 canonical CLI report kinds; `1099-nec` is an
alias for `1099`. Trial balance, account balance, balance sheet, and
reconciliation take `as_of` as an alias for `to` (CLI: `--as-of` for `--to`).
Aging takes `as_of`. Use `from` and `to` for other date-range reports.

### Read-only tools

Read-only mode removes `company_create`, `company_delete`, `account_create`, `txn_post`, `txn_propose`, `txn_approve`,
`txn_reject`, `txn_reverse`, `txn_meta_set`, `txn_meta_unset`, `txn_clear`,
`txn_reconcile`, `security_create`, `security_price`, `investment_buy`,
`investment_sell`, `budget_set`, `budget_delete`, `period_close`, `period_reopen`,
`ledger_setup_begin`, `ledger_setup_finish`, `ledger_setup_revoke`,
`ledger_setup_book_create`, `ledger_setup_book_initialize`, `ledger_setup_book_update`, `ledger_setup_chart_apply`,
`ledger_setup_account_add`, `ledger_setup_opening_preview`, `ledger_setup_opening_reserve`,
`ledger_setup_statement_preview`, `ledger_setup_statement_commit`,
`rule_create`, `rule_amend`, `rule_delete`, `bookkeeping_policy_set` and
`inform_preference_set` from the router.
It retains `txn_proposals`, `security_list`, `investment_lots`,
`investment_holdings` and the other tools requiring `ledger/read`.
Although `txn_reconcile` only checks links, its `ledger/reconcile` requirement
excludes it from read-only discovery and calls. Capability enforcement may
further restrict discovery: stdio filters at startup and HTTP per request.
`audit_log` accepts `actor` for the exact recorded label and `agent` for the
verified agent URI. Supply both to match both fields.

MCP tool discovery uses the same operation permissions as execution. Each
advertised tool includes the schema for its input and a `readOnlyHint`.
When `txn post` or `txn_post` omits currency, the posting uses
`BEANKEEPER_CURRENCY` if set and USD otherwise.

### Intentional CLI-only mutations

| CLI command | Reason |
|---|---|
| `bk init` | Ledger initialization chooses a local path, encryption and a new trust boundary. A trusted Desktop host can use `create_demo_ledger` to seed a separate file. |
| `bk backup`, `bk restore` | Deal with local full-ledger snapshot paths. The owner chooses the files and confirms replacement. |
| `bk key init`, `bk agent issue`, `bk agent revoke` | Owner-managed trust root and credential lifecycle. Setup sessions have their own scoped owner tools. |
| `bk agent limit set`, `bk agent limit clear`, `bk policy set` | Owner-managed authorization policy and limits. |
| `bk account delete` | Permanent catalog deletion uses local confirmation. |
| `bk txn import`, `bk txn attach` | The CLI works with local files. The bounded, reviewed statement import path has separate setup MCP tools. |
| `bk schedule create`, `bk schedule cancel`, `bk schedule run` | Recurring template management and execution are deferred until the MCP scheduling contract is available. |
| `bk loan pay` | The dedicated loan-terms input and payment calculation are deferred. Agents can already post a balanced payment through `txn_post`. |

Results match the CLI JSON `data` payload. JSON text content is always included;
`structuredContent` is also supplied when its shape is supported by the negotiated
protocol. Tool errors set `isError: true` with `{code, message}`. See
[README client configurations](../../../README.md#connect-an-ai-assistant-through-mcp)
for Claude Desktop and Codex examples without plaintext secrets.

### Intentional CLI-only reads

| CLI command | Reason |
|---|---|
| `bk export` | The CLI supports writing a full ledger export to a local file. |
| `bk agent list`, `bk agent limit show`, `bk policy show` | Owner-managed credential and authorization settings use the local CLI. |
| `bk schedule list`, `bk loan schedule` | These operations predate a corresponding MCP input contract. |

## Period close and locking

```bash
bk --company acme period close --through 2026-01-31
bk --company acme period list
bk --company acme --json period list
bk --company acme period reopen --reason "Correct a missing January receipt"
```

Each company closes independently. A close locks **all transaction dates on or
before** the inclusive cutoff, across currencies. An active close can only advance
the cutoff; an equal or earlier date is a validation error. Reopen clears the
company's entire cutoff and requires a nonempty reason. Both operations require
`ledger/admin` when capability enforcement is on; listing requires `ledger/read`.
No token is needed with enforcement off unless the caller supplies one.

The shared posting path rejects locked dates for journal posts, reversals, OFX,
CSV and JSON imports, scheduled occurrences, loan payments, investment trades,
and proposal approvals. A rejected approval remains pending, with its reference
reservation intact. Idempotent skips that create no posting remain allowed.
Reopening permits backdated postings, each with its normal creation audit record.
Clearance and metadata operations keep their existing behavior.

`period list` returns an array with the company's current state, or an empty array
if it has never closed. After reopening, the retained row has `closed_through: null`.
JSON/CSV fields are `company_slug`, `closed_through`, `changed_at`, `actor`, `reason`;
table output displays an open cutoff as `open`. Full close/reopen history is in
`audit log --entity period`, with `period_close` and `period_reopen` actions.

MCP tools `period_close`, `period_reopen`, and `period_list` share the same arguments
and behavior. Read-only MCP retains listing and removes close/reopen.

Back up before the automatic schema v19 to v20 migration. Historical audit hashes,
policy, and proposals are preserved; new records require audit hash version 4,
so older binaries cannot append. `verify`, backup, and restore validate lock state
against audit snapshots, including detection of removed lock rows.

## Companies

### `bk company create SLUG NAME`

```bash
bk company create acme "Acme Corp"
bk company create acme "Acme Corp" --description "Main business entity"
```

Slug: lowercase alphanumeric and hyphens, 1-64 characters.

### `bk company list`

### `bk company show SLUG`

### `bk company delete SLUG`

```bash
bk company delete old-company          # Prompts for confirmation
bk company delete old-company --force  # Confirms without prompting
```

The MCP `company_delete` call confirms deletion the same way `--force` does.
Both require `ledger/admin` under enforcement and record the previous company
in the audit log. A `VALIDATION` error for retained data explains how to keep
the company for its history and reverse an incorrect posting.

## Accounts

All account commands require `--company`.

### `bk account create CODE NAME --type TYPE`

```bash
bk --company acme account create 1000 "Cash" --type asset
bk --company acme account create 4000 "Revenue" --type revenue --default-tax-category income
```

Types: `asset`, `liability`, `equity`, `revenue`, `expense`.

### `bk account list`

```bash
bk --company acme account list
bk --company acme account list --type expense
bk --company acme account list --name cash
bk --company acme account list --with-balances
bk --company acme account list --with-balances --from 2026-01-01 --to 2026-03-31
```

### `bk account show CODE`

### `bk account delete CODE`

```bash
bk --company acme account delete 9999 --force
```

## Transactions

All transaction commands (except `reconcile`) require `--company`.

### `bk txn post`

Record a balanced journal entry. Debits must equal credits. The default
`direct` grant posts immediately. A credential issued with
`--write-mode propose-only` reserves a pending proposal with the same inputs.
MCP `txn_post` follows the same rule.

```bash
bk --company acme txn post -d "Office rent" \
  --debit 5000:2500 --credit 1000:2500
```

**Flags:**
- `-d, --description TEXT` (required): Transaction description
- `--debit ACCOUNT:AMOUNT` (required, repeatable): Debit entry
- `--credit ACCOUNT:AMOUNT` (required, repeatable): Credit entry
- `--date YYYY-MM-DD`: Transaction date (defaults to today)
- `--currency CODE`: Currency (default: USD)
- `-r, --reference KEY`: Idempotency reference
- `--on-conflict {error|skip}`: Duplicate handling (default: error)
- `--correlate TXN_ID`: Link to intercompany transaction
- `-m, --metadata TEXT`: Free-text memo or a JSON object (normalized to a JSON object, see below)
- `--meta KEY=VALUE` (repeatable): Set a top-level metadata string key; merges with `-m`; duplicate keys are an error
- `--tax ACCOUNT=CATEGORY` (repeatable): Tax category for entries

**Amount format:** `account_code:amount` where amount is in major units (dollars).

```bash
# Multi-line with tax categories
bk --company acme txn post -d "Payroll" \
  --debit 5300:5000 \
  --credit 2600:600 --credit 2800:382.50 --credit 1000:4017.50 \
  --tax 5300=payroll --tax 2600=payroll-tax --tax 2800=payroll-tax

# Idempotent post (safe to retry)
bk --company acme txn post -d "Monthly rent" \
  --debit 5000:2500 --credit 1000:2500 \
  -r "RENT-2026-03" --on-conflict skip

# Foreign currency
bk --company acme txn post -d "MXN payment" \
  --debit 5200:8500 --credit 1000:8500 --currency MXN

# Intercompany link
bk --company acme-products txn post -d "Payment to Consulting" \
  --debit 5400:5000 --credit 2500:5000 --correlate 15

# Structured metadata -> {"memo":"Q1 cloud","vendor":"AWS","project":"apollo"}
bk --company acme txn post -d "AWS bill" \
  --debit 5000:120 --credit 1000:120 \
  -m "Q1 cloud" --meta vendor=AWS --meta project=apollo
```

### Transaction metadata

`metadata` is always `null` or a **JSON object** (stored and returned as a JSON
string). Whatever you pass to `-m/--metadata` is normalized on write:

| `-m` input | Stored metadata |
|---|---|
| free text, e.g. `open:1000` | `{"memo":"open:1000"}` |
| a JSON object, e.g. `{"ref":"INV-1"}` | unchanged |
| a JSON string, e.g. `"abc"` | `{"memo":"abc"}` |
| a JSON array, e.g. `[1,2]` | `{"memo":[1,2]}` |
| a bare number / `true` / `false` / `null`, e.g. `00123` | `{"memo":"00123"}` (kept as the text you typed, so reference numbers keep leading zeros and precision) |
| empty or whitespace | no metadata (`null`) |

`--meta key=value` (repeatable) sets top-level **string** keys and merges with
`-m`: free text goes under `memo`, a JSON object gets the extra keys. A key
given twice (or already present in the `-m` object, or `memo` alongside free
text) is a `VALIDATION` error. `correlate` is reserved for `--correlate`.

`--correlate ID` adds `"correlate": ID` to the new transaction's metadata and
to the partner's (via the audited `correlate` action). A transaction counts as
correlated only if its metadata is a JSON object with a non-null `correlate`
key; plain text that mentions "correlate" never does.

### `bk txn propose`, `proposals`, `approve`, `reject`

```sh
bk --agent-token "$POSTER_TOKEN" --company acme txn propose -d "Equipment" \
  --debit 5000:750 --credit 1000:750 --date 2026-11-20 --reference equipment-001
bk --agent-token "$REVIEWER_TOKEN" --company acme txn proposals [--include-rejected]
bk --agent-token "$REVIEWER_TOKEN" --company acme txn approve PROPOSAL_ID
bk --agent-token "$REVIEWER_TOKEN" --company acme txn reject PROPOSAL_ID --reason "Requote first"
bk --agent-token "$POSTER_TOKEN" --company acme txn withdraw PROPOSAL_ID
```

`propose` accepts all `txn post` flags and always needs a verified agent token.
It validates and freezes the request without a transaction or hash-chain entry.
Pending references are unique and reserved against direct posts. A posted
reference follows `--on-conflict error|skip`; pending conflicts return validation.
`proposals` lists pending requests; `--include-rejected` includes approved,
rejected and withdrawn records too. Under enforcement read/propose grants list
owned requests; approve/admin grants list all. Only an approve grant approves
or rejects another agent's request. Propose permits withdrawing owned pending
requests with `reject` or its `withdraw` alias.

Approval revalidates and posts atomically, attributed to the approver, linked
by reserved metadata `proposal_id`. Its audit snapshot contains the frozen
request and proposer. Approval may override limits and consumes the proposer's
bucket. Rejection/withdrawal releases the reference. Each resolution is audited
and cannot be repeated. Proposal commands print reviewable JSON text in table
mode; `--json` returns the usual envelope. `data` is a proposal object, or an
array for listing; objects include `id`, `status`, `payload`, `debit_total`,
`currency`, proposer URI/JTI, timestamps, resolver URI/JTI, `transaction_id` and
`reason`. A skipped existing posted reference returns `{status:"skipped",
transaction_id:ID}`. Amounts retain minor units with `_display` companions.
MCP equivalents: `txn_propose`, `txn_proposals`, `txn_approve`, `txn_reject`.

### `bk txn list`

Search and filter transactions. Alias: `bk txn search`.

```bash
bk --company acme txn list
bk --company acme txn list --from 2026-01-01 --to 2026-03-31
bk --company acme txn list --account 1000
bk --company acme txn list -d "rent"
bk --company acme txn list --amount-gt 10000
bk --company acme txn list --amount-eq 2500
bk --company acme txn list --currency MXN
bk --company acme txn list --tax-category payroll
bk --company acme txn list --meta vendor=AWS --meta project=apollo
bk --company acme txn list --count   # Count only, no rows
```

**Flags:**
- `--account CODE`: Filter by account
- `--from DATE`, `--to DATE`: Date range (inclusive)
- `-d, --description TEXT`: Substring search (case-insensitive)
- `--amount-gt`, `--amount-lt`, `--amount-eq`: Amount filters (major units)
- `--currency CODE`: Filter by currency. Required with amount filters if the company uses multiple transaction currencies. Otherwise the sole transaction currency is inferred, with USD used for an empty company. MCP `txn_list` applies the same rule to `currency`.
- `--reference KEY`: Filter by idempotency key
- `--metadata TEXT`: Substring search over the raw metadata JSON text
- `--filter-metadata KEY=VALUE` (alias `--meta`; repeatable, AND-ed): Exact match on a top-level metadata key. JSON strings match exactly (case-sensitive; `KEY=` matches an empty string); numbers and booleans match by their JSON text as stored (`qty=3` matches `{"qty":3}` but not `{"qty":3.0}`; `paid=true` matches `{"paid":true}`); `null`, arrays, objects, missing keys and non-JSON metadata never match. Free text from `-m` is matched with `--filter-metadata memo=TEXT`. This is the same rule as on `report ledger`, `report reconciliation` and `report aging`.
- `--tax-category CAT`: Filter by tax category
- `--direction {debit|credit}`: Filter entry direction
- `--limit N` (default: 50), `--offset N` (default: 0): Pagination. JSON includes `meta.pagination` with filtered totals and `has_more`; table output signals truncation on stderr unless quiet.
- `--count`: Return count only

### `bk txn meta set/unset`

```sh
bk --company acme txn meta set 42 status=disputed dispute_id=D-42
bk --company acme txn meta set --reference CHARGE-42 status=resolved
bk --company acme txn meta unset 42 status dispute_id
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

### `bk txn show ID`

Display a single transaction with all its entries. Under each entry, the table
shows the entry id, its cleared and remaining amounts, and one line per
clearing (id, amount, status, cleared date, reconciled date, statement). JSON
adds `id`, `cleared_amount`, `remaining_amount` and `clearings` to each entry
(see `json-api.md`). CSV output is the transaction row only (no entries).

### `bk txn reverse ID`

Alias: `bk txn void ID`. Creates a linked opposite posting in the selected
company without changing the original entries, amounts, dates or clearings.

```bash
bk --company acme txn reverse 42 --date 2026-11-19 --reference VOID-42
bk --company acme txn reverse 42 --reference VOID-42  # successful retry (skip)
bk --company acme txn void 43 --description "Duplicate expense"
```

- `--date YYYY-MM-DD`: Defaults to local today, on or after the original date.
- `-d, --description TEXT`: Defaults to `Reversal of #ID: ORIGINAL DESCRIPTION`.
- `-r, --reference REF`: Defaults to `reversal-of-ID`. Trimmed and nonempty.
  A retry with an explicit reference matching the existing linked reversal
  succeeds with a skip; other attempts to reverse the same original fail.
  A reference belonging to any other transaction in the company is rejected.

Every entry is mirrored, retaining account, amount, memo and tax category;
the currency is unchanged. New entries start uncleared. Any reconciled
clearing on the original, including a partial reconciled clearing, blocks
reversal. Other clearings stay on the original. Reconciliation shows its
existing cleared/remaining amounts alongside the outstanding opposite
posting, effective from the reversal date. Neither side is automatically
cleared or reconciled.

Metadata links both ways (`reversed_by` on the original, `reverses` on the
reversal, numeric IDs). The original's other metadata is preserved. The new
transaction carries only `reverses`; transaction metadata and attachments
are not copied. Table views of `txn show` and `txn list` display the links;
JSON exposes them in `metadata`. The posting, original link and `reverse`
audit record commit atomically, recording `--actor` / `BK_ACTOR` as usual.

The undo uses reversal authority directly. JSON returns the direct posting
result shape with `meta.command = "txn.reverse"`.
Missing/foreign IDs exit 5; invalid dates, duplicate references, reconciled
originals and repeated reversals exit 3. Reversing a reversal is allowed,
but each transaction can be reversed only once.

### `bk txn import`

Import transactions from OFX, CSV, or JSON files.

```bash
bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000

bk --company acme txn import \
  --file statement.ofx --account 1000 --suspense 9000 --dry-run

cat data.ofx | bk --company acme txn import \
  --file - --input-format ofx --account 1000 --suspense 9000
```

**Flags:**
- `--file PATH`: Input file (use `-` for stdin)
- `--input-format {ofx|csv|json}`: Auto-detected from extension if omitted; `--format` remains a hidden compatibility alias
- `--account CODE`: Bank/asset account (required for OFX and CSV)
- `--suspense CODE`: Contra account for unclassified entries (required for OFX and CSV; CSV requires a different account from `--account`)
- `--dry-run`: Validate without persisting
- `--on-conflict {error|skip}` (default: skip)
- `--currency CODE`: CSV/JSON default currency (`BEANKEEPER_CURRENCY`, then USD); JSON transactions may override it
- `--date-col NAME`, `--description-col NAME`, `--amount-col NAME`: CSV headers (defaults: `date`, `description`, `amount`)
- `--reference-col NAME`: CSV reference header (default: use `reference` if present); an explicit header name must exist
- `--date-format FORMAT`: CSV strftime date format (default: `%Y-%m-%d`)
- `--positive-amount {inflow|outflow}`: CSV sign convention (default: inflow)

CSV example (`--account 1000 --suspense 9000`):

```csv
date,description,amount,reference
2026-03-15,"Coffee, shop",-12.34,bank-1001
2026-03-16,Deposit,100.00,bank-1002
```

Positive inflows debit the bank and credit suspense; negative amounts reverse
the pair. `--positive-amount outflow` reverses the sign convention. CSV amounts
are exact major units without currency symbols or thousands separators.
Descriptions support CSV quoting. Invalid dates, zero amounts, excess
currency precision, missing accounts, and missing/duplicate headers fail validation.

JSON input is a bulk journal array, independent of the full-database export:

```json
[
  {
    "date": "2026-03-15",
    "description": "Office supplies",
    "reference": "receipt-1001",
    "currency": "USD",
    "metadata": {"vendor": "Stationer"},
    "entries": [
      {"account_code": "5000", "direction": "debit", "amount": "12.34", "memo": "Paper", "tax_category": "supplies"},
      {"account_code": "1000", "direction": "credit", "amount": "12.34"}
    ]
  }
]
```

Transaction `date` (`YYYY-MM-DD`), `description`, and `entries` are required.
`currency`, `reference`, and object `metadata` are optional. Entries require
`account_code`, `direction` (`debit`/`credit`), and positive `amount`; `memo` and
`tax_category` are optional. Account tax defaults apply when a category is
omitted. All entries share the transaction currency and must balance through
the same journal validation as `txn post`. JSON decimal amounts must be
strings; integer numbers are accepted as whole **major units**. Fractional
JSON numbers and unknown fields are rejected. JSON needs no bank/suspense flags.

CSV/JSON imports are atomic, including `--on-conflict error`: one bad row or
posting rolls back the entire batch. Errors return a validation envelope and
exit 3, with no success summary. `--dry-run` uses the same posting path and
rolls back, so duplicate checks include rows earlier in the batch. Only `skip` and `error` are
accepted (other values are usage errors, exit 2).

Explicit references are trimmed, stored verbatim, and scoped to the company.
CSV blank/missing references and JSON missing/null references get a
`csv:sha256:<hash>:<n>` or `json:sha256:<hash>:<n>` reference. The hash covers
normalized posting data, including accounts, currency, date, description,
entries, and metadata. The ordinal `n` starts at 1 for each distinct content
among unreferenced rows in the batch. Identical unreferenced rows are all posted.
Re-importing the same or an overlapping file skips matching occurrences by
default; a later file with one more identical row imports only that additional
occurrence. File names and unrelated rows do not affect these references.
Duplicate explicit references within a batch still skip or error according to
`--on-conflict`. Explicit blank JSON references are rejected.

### `bk txn clear`

Clear all or part of an entry against a statement, for reconciliation. Each
call records a **clearing**: an amount, a cleared date, an optional
statement id and, once reconciled, a reconciled date. An entry can be cleared in several parts, on different dates,
by different statements. Every change is recorded in the audit trail
(`status_change` on the entry), so "when was this item cleared, and by which
statement?" is answerable via `bk audit log`.

```bash
bk --company acme txn clear 42 --entry 5                     # Clear in full, today
bk --company acme txn clear 42 --entry 5 --status reconciled # Mark as reconciled

# Partial clearing by transaction reference; the account picks the entry
bk --company acme txn clear --reference POL-123 --account 1200 \
  --amount 40 --date 2026-03-31 --statement STMT-2026-03

# A whole statement as a batch (stdin or a file), all or nothing
printf 'POL-123,40\nPOL-124\n' | bk --json --company acme txn clear \
  --from-file - --account 1200 --date 2026-03-31 --statement STMT-2026-03
```

**Picking entries** (exactly one of):
- `TXN_ID --entry ENTRY_ID`: one entry by id
- `--reference REF --account CODE`: the entry on `CODE` of the transaction whose reference is `REF` (references are unique per company). Errors: no such reference or no entry on that account → `NOT_FOUND`; several entries on that account → `VALIDATION` (use `TXN_ID --entry`)
- `--from-file PATH|- --account CODE`: a batch. One `reference[,amount]` CSV record per line (quote references that contain commas); blank lines and `#` comments are ignored, as is a leading `reference,amount` header. A reference may appear only once per batch. Every line is resolved and validated first, then all are applied in one transaction: if any line fails, nothing changes and the error names the line

**Flags:**
- `--amount AMT`: Amount to clear in major units (default: the full remaining amount). Not with `--from-file` (put amounts in the file)
- `--date YYYY-MM-DD`: Cleared date (default: today); with `--status reconciled`, the reconciled date of every clearing created or promoted. May not be before the transaction date (`VALIDATION`). `report reconciliation --to` counts clearings cleared on or before `--to`, and as reconciled only those also reconciled on or before it
- `--statement ID`: Statement id stored on the clearing
- `--status {cleared|reconciled|uncleared}` (default: `cleared`)

**Rules:**
- *Over-clearing* is a `VALIDATION` error: an explicit amount may not exceed the remaining (uncleared) amount; amounts must be positive.
- *Idempotency:* the key is **(entry, statement)**. An entry has at most one clearing per statement, so re-running the same statement (single or batch) skips entries it already cleared (`action: "skipped"`, `skip_reason: "already_cleared_by_statement"`). Re-running with a *different* explicit amount for the same entry and statement is a `VALIDATION` error. Without `--statement` there is no key: each call adds a clearing, except that clearing the full remaining amount of a fully cleared entry is a skip (`fully_cleared`).
- *Dates:* `--date` may not be before the transaction's date. In a batch the error names the line and nothing is applied.
- `--status reconciled` records a reconciled clearing. With no `--amount`/`--statement` it reconciles the entry in full (promotes its existing clearings and clears any remainder). For an entry the statement already cleared, it promotes that clearing, so re-running a statement batch with `--status reconciled` locks it in. Every clearing it creates or promotes gets `reconciled_date = --date`; a new reconciled clearing also has `cleared_date = --date`. Promoting a clearing whose cleared date is after `--date` is a `VALIDATION` error.
- *Reconciled clearings are locked.* `--status cleared` never downgrades them: for an entry whose clearing for `--statement` is reconciled, or (without `--statement`) a fully reconciled entry, it is a `VALIDATION` error (in a batch: the batch fails, naming the line). To redo it, remove the clearing with `--status uncleared` (with the same `--statement`), then clear it again. Re-running a statement whose clearings are still only `cleared` is unchanged: a skip.
- `--status uncleared` removes the entry's clearings (only that statement's with `--statement`); `--amount` is not allowed.

**Entry status** (`entries.status`, shown by `txn show`) is derived from the clearings:
`reconciled` when they cover the full amount and all are reconciled; `cleared`
when they cover the full amount; `uncleared` otherwise, including partially
cleared entries (the reconciliation report shows their cleared and remaining
amounts).

**Output:** with `--json`, a single entry returns `transaction_id`, `entry_id`,
`reference`, `status` (requested), `action` (`cleared`, `reconciled`,
`uncleared`, `skipped`), `clearing_id`, `amount` (cleared or removed by this
call), `entry_amount`, `cleared_amount`, `remaining_amount`, `previous_status`,
`entry_status`, `skip_reason`, `cleared_date`, `reconciled_date` (`--date`
with `--status reconciled`, else null), `statement`. A batch returns
`account`, `status`, `cleared_date`, `reconciled_date`, `statement`, `count`, `cleared_count`,
`skipped_count`, `cleared_total`, and `cleared`/`skipped` arrays of those items
(each with its input `line`). Amounts retain integer minor units and add decimal
`_display` companions. Each item includes `currency` and `partially_cleared`,
while existing status values stay unchanged. Human messages use currency units
and describe partial clears as `partially cleared`. A batch spanning currencies
has null `currency` and `cleared_total_display` for its aggregate. With `--format csv`,
one row per entry: `line,reference,transaction_id,entry_id,action,amount,cleared_amount,remaining_amount,entry_status,cleared_date,statement,skip_reason`.
The table format prints one `[ok]`/`[skip]` line per entry.

### `bk txn attach TXN_ID FILE --type TYPE`

Attach a document to a transaction.

```bash
bk --company acme txn attach 42 receipt.pdf --type receipt
bk --company acme txn attach 42 invoice.pdf --type invoice --entry 5
```

Types: `receipt`, `invoice`, `statement`, `contract`, `other`.

### `bk txn reconcile`

Find orphaned intercompany correlations. Does not require `--company` (scans all).

## Loans

### `bk loan schedule`

```bash
bk loan schedule --principal 200000 --rate 6 --months 360 --currency USD --json
```

| Flag | Meaning |
|------|---------|
| `--principal AMOUNT` | Required original principal, positive major currency units |
| `--rate PERCENT` | Required nominal annual percentage, 0 through 100, up to four decimal places |
| `--months N` | Required number of monthly payments, 1 through 1,200 |
| `--currency CODE` | Currency; defaults to `BEANKEEPER_CURRENCY`, then USD |

No company or database is needed unless an agent token is supplied, which
triggers normal database-backed token verification. Table mode shows the
regular payment, total interest, total paid and every payment split. JSON uses
the standard envelope with command `loan.schedule` and minor-unit integers plus
decimal display companions. CSV columns are
`number,payment,principal,interest,balance`, with amounts in minor units.

The monthly rate is the nominal annual rate divided by twelve. The exact annuity
payment and each month's interest round half up to the currency's minor unit.
The final payment absorbs rounding so principal ends at exactly zero. The
regular payment is at least one minor unit and payments cap at the remaining
principal plus interest, so very small loans can finish early. Zero-rate loans
are supported. All CLI monetary values, including totals, must fit `i64`.

### `bk loan pay`

Accepts all schedule flags plus the required flags below, and requires a
company through `--company` or `BEANKEEPER_COMPANY`.

| Flag | Meaning |
|------|---------|
| `--payment-number N` | One-based payment number present in the calculated schedule |
| `--liability CODE` | Existing liability account, debited for principal |
| `--interest-account CODE` | Existing expense account, debited for interest |
| `--paying-account CODE` | Existing asset account, credited for the payment total |
| `--date YYYY-MM-DD` | Transaction date |
| `--reference KEY` | Nonempty unique reference for this loan and payment |
| `--on-conflict error\|skip` | Duplicate reference behavior, default `error` |

```bash
bk --company acme loan pay --principal 200000 --rate 6 --months 360 \
  --payment-number 1 --liability 2000 --interest-account 5100 \
  --paying-account 1000 --date 2026-11-20 --reference mortgage-2026-11 --json
```

Posts through the existing transaction validation and atomic audited path;
zero-value lines are omitted and account tax defaults apply. JSON command
`loan.pay` returns the same `id`/`created`/`skipped` or
`existing_id`/`created`/`skipped` payload as `txn post`. Table output reports the
transaction ID and status. CSV has `id,status` columns. Use `txn show` to inspect
the posted entries. Retry with the same reference; `skip` reports the existing
transaction and does not promise its terms match the supplied calculation.

No loan records or schema migrations are introduced. Both commands are
stateless: original terms and payment number are explicit, payment order is
not enforced, and liability balances are not used to recalculate payments.
Persistent loan tracking, extra payments, fees, daily interest and variable
rates are outside scope. These commands do not add MCP tools.

## Budgets

All budget commands require `--company`.

### `bk budget set ACCOUNT`

Create or update a budget. Upsert semantics -- re-running overwrites.

```bash
# Single month
bk --company acme budget set 5000 --year 2026 --month 3 --amount 2500

# Annual (distributed evenly across 12 months)
bk --company acme budget set 5000 --year 2026 --annual 30000

# With currency and notes
bk --company acme budget set 5000 --year 2026 --annual 30000 \
  --currency MXN --notes "Office lease"
```

**Flags:**
- `--year YEAR` (required)
- `--month N` + `--amount AMT`: Single month (mutually exclusive with `--annual`)
- `--annual AMT`: Distribute evenly across 12 months (mutually exclusive with `--month`)
- `--currency CODE` (default: USD)
- `--notes TEXT`: Optional note

Amounts are in major units (dollars). Annual distribution is exact: each month gets `annual / 12`, with the first `annual % 12` months receiving an extra minor unit.

### `bk budget list`

```bash
bk --company acme budget list --year 2026
bk --company acme budget list --year 2026 --account 5000
bk --company acme budget list --year 2026 --month 3
```

### `bk budget delete ACCOUNT`

```bash
bk --company acme budget delete 5000 --year 2026           # All months
bk --company acme budget delete 5000 --year 2026 --month 3 # Single month
bk --company acme budget delete 5000 --year 2026 --force   # Skip confirmation
```

## Reports

### Report date defaults

All date-based reports use inclusive `YYYY-MM-DD` dates and local today as the
omitted end date. Point-in-time reports (`balance`, `balance-sheet`,
`trial-balance`, `ledger`, `reconciliation`, and `aging`) include history through
today and exclude future transactions unless an explicit later end date is
provided. Aging uses `--as-of`; the others use `--to`. An omitted `--from` on
`income-statement`, `cash-flow`, `tax-summary`, or a `--period`/`--compare` report
means January 1 of the effective end date's year. Thus a historical `--to` alone
selects that year's activity. Explicit bounds override these defaults; invalid
or reversed ranges fail validation. Table headings show the effective window.
Budget variance continues to use its required `--year` and optional month range.

All report commands require `--company`.

**Report currency.** Every report is computed in a single currency; amounts in
different currencies are never added together. All report subcommands accept
`--currency CODE`, which limits the report to transactions in that currency.
When it is omitted the currency is inferred from the company's transactions:

- one currency in use: that currency is used;
- no transactions yet: USD;
- several currencies in use: the command fails with a `VALIDATION` error
  (exit code 3) that lists the currencies found; rerun with `--currency`.

### `bk report trial-balance`

```bash
bk --company acme report trial-balance
bk --company acme report trial-balance --to 2026-03-31
bk --company acme report trial-balance --from 2026-01-01 --to 2026-03-31
bk --company acme report trial-balance --type expense
bk --company acme report trial-balance --currency MXN
```

### `bk report balance --account CODE`

```bash
bk --company acme report balance --account 1000
bk --company acme report balance --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report balance --account 1000 --currency MXN
```

### `bk report income-statement`

Revenue and expense summary for a period. Output has a **Revenue** section and an
**Expenses** section, each listing accounts with one signed amount on the
account's normal side (credit for revenue, debit for expenses) and a subtotal,
followed by **Net Income** (or **Net Loss** when expenses exceed revenue).

```bash
bk --company acme report income-statement --from 2026-01-01 --to 2026-03-31
```

### `bk report balance-sheet`

Assets, liabilities, and equity as of a date. Output has **Assets**,
**Liabilities** and **Equity** sections with subtotals, then **Total
Liabilities + Equity** and a balance check (`[ok] BALANCED` /
`[!!] UNBALANCED`).

Beankeeper does not post closing entries, so the Equity section always
includes a computed line, **Retained Earnings (computed)**, equal to cumulative
revenue minus expenses up to `--to`. This keeps Assets = Liabilities + Equity
for any valid ledger. If you post your own closing entries into a retained
earnings account, the revenue/expense balances drop to zero and the computed
line becomes zero accordingly.

```bash
bk --company acme report balance-sheet
bk --company acme report balance-sheet --to 2026-03-31
```

### `bk report trend`

Historical trends with one column per calendar period.

```bash
bk --company acme report trend --metric spending --from 2026-01-01 --to 2026-12-31
bk --company acme report trend --metric net-worth --period quarter --to 2026-12-31
```

- `--metric spending|net-worth` is required. Spending shows each expense
  account's debits minus credits in each period, including zero accounts and
  refunds, plus Total Spending. Categories are account codes and names.
- Net worth shows Total Assets, Total Liabilities and Net Worth at each period
  end, including history before `--from`. Assets are debits minus credits;
  liabilities are credits minus debits; net worth is assets minus liabilities.
  These are book values. No cross-period total is calculated.
- `--period month|quarter|year` defaults to month, with at most 120 periods.
- `--from DATE`, `--to DATE` are inclusive. End defaults to today; start defaults
  to January 1 of the end year. First and last periods are clipped; empty
  periods remain present. Invalid or reversed dates fail with `VALIDATION`.
- `--currency CODE` selects a single currency. Without it, infer the company's
  only transaction currency, use USD for an empty company, or fail if ambiguous.
- Table cells use major units. JSON command ID is `report.trend`; `data` includes
  `metric`, `period`, `from`, `to`, `currency`, `columns` (`label/from/to`), and
  `series` (`code`, `name`, `amounts`, `amounts_display`). Summary series have
  `code: null`; arrays align with columns. CSV uses `code,name,<period labels>`
  with integer minor-unit amounts, blank summary codes, and proper CSV quoting.
- MCP `report` accepts `kind: "trend"`, `metric`, and the same optional fields.

### `bk report tax-summary`

Entries grouped by tax category.

```bash
bk --company acme report tax-summary --from 2026-01-01 --to 2026-12-31
```

### `bk report schedule-c`

```sh
bk --company llc report schedule-c --from 2026-01-01 --to 2026-12-31 --json
bk --company llc report schedule-c --to 2026-12-31 --output-format csv
```

Options: `--from`, `--to`, `--currency`, `--output-format table|json|csv`.
Global `--format` and `--json` also work. Dates and currency follow `tax-summary`.
Maps case-insensitive `sched-c:<line>` tags to a static Schedule C line table.
JSON `data` includes `from`, `to`, `currency`, `lines`, `unmapped`, `untagged`.
Each activity row carries line/title, category, account fields, debit/credit
totals, and net. Income lines 1/6 are credit-positive; other lines debit-positive.
CSV has a `section` column and includes unmatched rows. JSON/CSV use integer
minor units; JSON adds decimal display strings. No deduction limits are applied.
See [the supported line table and full contract](../../tax-exports.md).

### `bk report 1099`

```sh
bk --company llc report 1099 --to 2026-12-31 --threshold 600 --json
bk --company llc report 1099 --to 2026-12-31 --threshold 0 --format csv
```

Alias: `1099-nec`. Options: `--from`, `--to`, `--currency`, `--threshold`,
`--output-format table|json|csv`. Dates/currency match `tax-summary`. Threshold
is a nonnegative major-unit decimal, default `600`; equality qualifies.
Only totals at or above the threshold appear; `0` lists every payee.
Annotate transactions with nonempty string `payee` and optional string `tin`,
`payee_address`. Grouping uses exact payee strings; conflicting nonempty details
fail validation. Each transaction contributes its full debit total, counted
once even with split debit legs. JSON includes `threshold` and `vendors` with
payee/details, payment count, total, and `meets_threshold`. CSV carries all vendor
fields, currency, period, and threshold. Monetary fields use minor units, with
JSON decimal displays. Trace using `txn list --filter-metadata 'payee=Name'`.
MCP `report` kinds `schedule-c` and `1099` return the same JSON payloads; vendor
`threshold` is an optional decimal string. These are book summaries, not tax
advice or filed forms. See [tax export conventions](../../tax-exports.md).

### `bk report ledger`

General ledger / account statement. For each account: an opening balance as of
the day before `--from` (zero without `--from`), one row per entry (ordered by
date, transaction id, entry id) with a running balance, and a closing balance.
Balances are on the account's normal side: `debits - credits` for asset and
expense accounts, `credits - debits` for liability, equity and revenue accounts.

```bash
bk --company acme report ledger
bk --company acme report ledger --account 1000 --from 2026-01-01 --to 2026-03-31
bk --company acme report ledger --account 1000 --account 4000 --currency EUR
bk --company acme report ledger --account 5000 --filter-metadata project=apollo
bk --company acme --format csv report ledger --account 1000
```

**Flags:**
- `--account CODE` (repeatable): Accounts to include. Default: every account
  with activity (entries in the period or a non-zero opening balance).
- `--from DATE`, `--to DATE`: Inclusive date range (`YYYY-MM-DD`).
- `--currency CODE`: Only entries of this currency. Default: the company's only
  transaction currency; if the company has several, the command fails with a
  validation error (exit 3) asking for `--currency`; with no transactions, USD.
- `--filter-metadata KEY=VALUE` (repeatable, AND-ed): only entries whose
  transaction metadata matches (same rule as `txn list`). The opening balance,
  running balances and closing balance count matching entries only, so the
  statement stays consistent; the table prints the active filter and JSON lists
  it in `metadata_filters`. Without `--account`, only accounts with matching
  activity are listed.

**CSV:** one flat row per statement line. Columns: `account_code`,
`account_name`, `account_type`, `currency`, `row_type`, `date`,
`transaction_id`, `entry_id`, `description`, `reference`, `memo`, `debit`,
`credit`, `balance`. `row_type` is `opening` (balance only, date = `--from`),
`entry`, or `closing` (date = `--to`, `debit`/`credit` = period totals,
`balance` = closing balance). Amounts are minor units.

### `bk report aging`

Receivables / payables aging for one or more asset or liability accounts.

```bash
bk --company acme report aging --account 1100
bk --company acme report aging --account 2000 --as-of 2026-03-31
bk --company acme report aging --account 1100 --buckets 15,45
bk --company acme report aging --account 1100 --group-by-metadata customer
bk --company acme report aging --account 1100 --group-by customer --detail
bk --company acme report aging --account 1100 --method specific
bk --company acme report aging --account 1100 --method specific --link-key invoice
bk --company acme report aging --account 1100 --currency EUR
bk --company acme report aging --account 1110 --account 1120
bk --company acme report aging --account 1100 --include-children --group-by customer
bk --company acme report aging --account 1100 --include-children --group-by customer --group-across-accounts
bk --company acme report aging --account 1100 --filter-metadata status=disputed
```

**Flags:**
- `--account CODE` (required, repeatable): an asset account (receivable: charges are debits, settlements are credits) or a liability account (payable: charges are credits, settlements are debits). Other account types, and a mix of asset and liability accounts, are a validation error (exit 3). Duplicates are ignored; accounts are reported in code order.
- `--include-children`: also age every account whose code starts with each `--account` code minus its trailing zeros (`1100` adds `1110`, `1120`, `11xx...`; `1110` adds `1110x...`). A code that is all zeros is rejected. There is no separate parent column: the rollup follows the chart-of-accounts code convention.
- `--as-of DATE`: include transactions dated on or before this date (default: today). Charges and settlements dated later are ignored, so rerunning for a past period end gives the same answer after later-dated payments are posted (back-dated postings on or before the date do change it).
- `--buckets N,N,...`: strictly ascending positive day bounds (default `30,60,90`, giving `Current`, `1-30`, `31-60`, `61-90`, `90+`)
- `--group-by-metadata KEY` (alias `--group-by KEY`): group by a key of the transaction metadata. Metadata is parsed leniently: if it is a JSON object holding a non-empty string or a number under `KEY`, that value is the counterparty; anything else (missing key, non-JSON text, JSON that is not an object, other value types) goes to `(unassigned)`.
- `--method fifo|specific` (default `fifo`): how settlements are matched to charges (see below).
- `--link-key KEY` (default `applies_to`): metadata key holding a settlement's link to its charge. Only valid with `--method specific` (exit 3 otherwise).
- `--detail`: list the open charges under each counterparty in table and CSV output (JSON always includes `open_items`).
- `--currency CODE`: currency to report. Defaults to the company's single transaction currency; if the company has several, `--currency` is required (exit 3); with no transactions, USD.
- `--group-across-accounts` (requires `--group-by`): with several accounts, show one row per counterparty combined across accounts (followed by account subtotals) instead of counterparty rows within each account. Matching is unchanged; JSON always has both views.
- `--filter-metadata KEY=VALUE` (repeatable, AND-ed; same rule as `txn list`): show only open charges whose transaction metadata matches. See "Metadata filter" below.

**Several accounts.** Each selected account is aged on its own, exactly as in a single-account run: FIFO and specific matching never cross accounts, so the combined totals equal the sum of the single-account runs. A specific link from a settlement on one selected account to a charge on another is not honoured and is reported with reason `other_account`. With `--group-by`, groups are formed within each account.

**Metadata filter.** Settlements are matched against every charge first; only then are the open items restricted to charges whose metadata matches. A filtered charge therefore keeps its original age and shows what is still open after the payments applied to it, whether they matched the filter or not (typically payments carry no `status`). This lets a disputed receivable stay in the AR account tagged `{"status":"disputed"}` instead of moving to another account, which would reset its age. Unapplied credit cannot be attributed to a charge, so it is not shown under a filter (`unapplied_credit` is 0), and the totals no longer tie to the account balance: the table shows the unfiltered balance separately, and JSON still has the real `account_balance`.

**How it works.** The ledger has no invoice or due-date model, so ages are measured from transaction dates. Entries on the account are netted per transaction; a net increase is a *charge* and a net decrease is a *settlement*. For an asset account (receivables) charges are debits and settlements credits; for a liability account (payables) charges are credits and settlements debits -- the same command serves AR and AP. Transactions are processed by date (charges before settlements on the same date, then by id), per counterparty when grouping. Settlements beyond the open charges are reported as an *unapplied credit*, which later charges consume first. Each charge's remaining open amount is bucketed by age in days (`as_of - transaction date`; age 0 is `Current`). The report always reconciles: total open - unapplied credit = account balance as of the date.

**Matching methods.**
- `fifo`: every settlement is applied to the oldest open charges.
- `specific`: a settlement whose metadata links to a charge is applied to that charge first (up to its open amount); any excess, and settlements without a link, fall back to FIFO. Links are ignored on charges.

**Links.** References are unique per company, so a payment cannot share its invoice's reference; instead the settlement's JSON metadata names the charge under the link key (default `applies_to`):
- a string is the charge's `reference` (as posted with `txn post -r`); if no transaction has that reference and the string is an integer, optionally `#`-prefixed (`"42"`, `"#42"`), it is used as a transaction id;
- a positive integer is the charge's transaction id;
- an array of these is applied in order (one payment covering several invoices).

```bash
bk --company acme txn post -d "Invoice 102" -r INV-102 --debit 1100:300 --credit 4000:300 \
  -m '{"customer":"Globex"}'
bk --company acme txn post -d "Globex payment" --debit 1000:100 --credit 1100:100 \
  -m '{"customer":"Globex","applies_to":"INV-102"}'
```

A link that cannot be honoured is ignored, the settlement is applied FIFO, a `[warn]` line is printed on stderr (unless `--quiet`), the table lists it under "Unmatched links", and JSON reports it in `unmatched_links` with one of these reasons: `not_found` (no transaction with that reference/id), `not_on_account` (the transaction has no entries on this account in this currency), `not_a_charge` (it is a settlement or nets to zero on this account), `charge_after_settlement` (the charge is dated after the settlement), `after_as_of` (the charge is dated after `--as-of`), `other_counterparty` (the charge belongs to another counterparty group), `other_account` (the charge is on another of the selected accounts), `invalid` (the value is not a non-empty string, a positive integer, or an array of those). A link to a charge that is already fully settled is not an error: the amount simply falls back to FIFO.

**Credits, refunds and reversals.** On a receivable, every credit-side net movement is a settlement: payments, credit notes, write-offs, and reversals of an invoice. Under `fifo` it reduces the oldest open charge; under `specific`, link it to the invoice (e.g. a credit note with `{"applies_to":"INV-102"}`) and it cancels that invoice. Every debit-side net movement is a charge: invoices, refunds paid out to the customer, and payment chargebacks (bounced payments). A charge first consumes any unapplied credit, so refunding an overpayment clears the credit; a chargeback that is not absorbed becomes a new open item aged from its own date. Payables mirror this with the sides swapped: bills and vendor refunds received are charges (credits), payments and vendor credit notes are settlements (debits).

**Net credit.** When settlements exceed the open charges the counterparty (or the account) is in *net credit*: the customer has prepaid or overpaid (for payables: we have prepaid or overpaid the vendor). Its open buckets are zero and the excess is shown as `Unapplied Credit`; `Net` (= total open - unapplied credit) is negative. The table marks it `CR` (e.g. `-150.00 CR`) and adds a legend line; CSV has a negative `net`; JSON has a negative `net` and `"net_credit": true`.

**Output.** Table output has columns for each bucket, `Total Open`, `Unapplied Credit` and `Net`, and these rows:
- without `--group-by`: one row per account (an `Account` column with code and name), then `Total`;
- with `--group-by` and one account: one row per counterparty, then `Total`;
- with `--group-by` and several accounts: per account, its counterparty rows and a `Subtotal` row (`Account` and `Counterparty` columns), then `Total`; with `--group-across-accounts`, the combined counterparty rows first, then one `Subtotal` per account, then `Total`.

With `--detail`, an "Open items" table follows for each counterparty (or each account when ungrouped): date, transaction id, reference, description, age (days), bucket, original amount and open amount, plus an `Account` column with several accounts.

CSV (amounts in minor units) is one flat table in the same row order. Columns: `row_type, account, counterparty`, then (with `--detail` only) `transaction_id, date, reference, description, age_days, bucket, original_amount, open_amount`, then one column per bucket, `total_open, unapplied_credit, net`. `row_type` is `counterparty` (`account` is empty for rows combined across accounts), `account` (an account's only row when ungrouped, otherwise its subtotal; subtotals appear only with several accounts), or `total` (`account` and `counterparty` empty). With `--detail`, `item` rows (one per open charge; its open amount appears in its bucket column and in `total_open`) precede the row that totals them.

### `bk report budget-variance`

Compare budgeted amounts to actual spending/revenue.

```bash
bk --company acme report budget-variance --year 2026
bk --company acme report budget-variance --year 2026 --month 3
bk --company acme report budget-variance --year 2026 --from 1 --to 6
bk --company acme report budget-variance --year 2026 --type expense
bk --company acme report budget-variance --year 2026 --include-unbudgeted
```

**Flags:**
- `--year YEAR` (required)
- `--month N`: Single month (mutually exclusive with `--from`/`--to`)
- `--from N`, `--to N`: Month range, 1-12 (defaults to full year)
- `--type TYPE`: Filter by account type
- `--currency CODE`: Currency of the budgets and actuals (env: `BEANKEEPER_CURRENCY`;
  inferred from the company's transactions when omitted, see above)
- `--include-unbudgeted`: Show accounts with actuals but no budget

**Variance logic:**
- Expense accounts: variance = budget - actual (positive = favorable, underspent)
- Revenue accounts: variance = actual - budget (positive = favorable, exceeded target)
- Status: `FAV`, `UNFAV`, or `ON BUDGET`

### Multi-column reports: `--period` and `--compare`

`report income-statement` and `report trial-balance` accept two options that
turn the single-column report into a multi-column one. Missing `--to` defaults
to today, and missing `--from` defaults to January 1 of the effective end year.
The options cannot be combined (usage error, exit code 2). Without either option
the output is unchanged.

```bash
# One column per month, plus a Total column and a Net Income row per month
bk --company acme report income-statement --from 2026-01-01 --to 2026-06-30 --period month

# Quarterly trial-balance activity, expense accounts only
bk --company acme report trial-balance --from 2026-01-01 --to 2026-12-31 --period quarter --type expense

# This quarter vs. the same quarter last year, with change and change %
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-year

# This quarter vs. the previous quarter
bk --company acme report income-statement --from 2026-04-01 --to 2026-06-30 --compare prior-period
```

**Flags:**
- `--period month|quarter|year`: One column per calendar period between
  `--from` and `--to`, plus a `Total` column. The first and last periods are
  clipped to the range. Columns are labelled `2026-01`, `2026-Q1`, `2026`.
  At most 120 columns.
- `--compare prior-year|prior-period`: Adds a column for an earlier range,
  plus `Change` (current - prior) and `Change %` (relative to the absolute
  prior amount; `n/a` when the prior amount is 0).
  - `prior-year`: the same dates one year earlier (Feb 29 becomes Feb 28).
  - `prior-period`: the range of equal length ending the day before `--from`.
    When the range is whole calendar months it shifts back by that many
    months (Q2 compares with Q1, March with February).

**Amounts:** every cell is the account's net activity in that window on its
normal side (debit - credit for assets and expenses, credit - debit for
liabilities, equity and revenue), so a positive number is a "normal" balance.
Trial-balance columns show activity within each window, not cumulative
balances. The income statement adds `Total Revenue`, `Total Expenses` and
`Net Income` rows; the trial balance adds `Total Debits` and `Total Credits`
rows (raw debit/credit totals per window).

### `bk report reconciliation --account CODE`

Reconcile one account against a bank, card or carrier statement using entry
clearings (recorded with `bk txn clear`). Not to be confused with
`bk txn reconcile`, which finds orphaned intercompany correlations.

```bash
bk --company acme report reconciliation --account 1000
bk --company acme report reconciliation --account 1000 --to 2026-03-31
bk --company acme report reconciliation --account 1000 --to 2026-03-31 \
  --statement-balance 1234.56
bk --company acme report reconciliation --account 2100 --currency USD   # credit card

# Check one statement's cleared set against its total
bk --company acme report reconciliation --account 1200 \
  --statement STMT-2026-03 --statement-balance 1500

# Outstanding receivables for one carrier, grouped by policy
bk --company acme report reconciliation --account 1200 \
  --filter-metadata carrier=Acme --group-by-metadata policy
```

**Flags:**
- `--account CODE` (required): Account to reconcile
- `--to YYYY-MM-DD`: As-of date, inclusive (default: today). Point in time: only entries dated on or before it count; a clearing counts toward the cleared balance when `cleared_date <= --to`, and toward the reconciled balance only when it is reconciled and `reconciled_date <= --to`. So an item cleared later shows as uncleared as of an earlier date, and a clearing reconciled later shows as cleared but not reconciled (also with `--statement`)
- `--statement-balance AMT`: Statement ending balance in major units (e.g. `1234.56`, may be negative), on the account's normal side
- `--statement ID`: The cleared and reconciled balances count only this statement's clearings, so `--statement-balance` checks that statement alone. Outstanding items still reflect every statement's clearings
- `--filter-metadata KEY=VALUE` (repeatable, AND-ed): Only entries whose transaction metadata has top-level `KEY` equal to `VALUE` (same rule as `txn list`: strings exactly, numbers and booleans by their JSON text; null, arrays, objects and non-JSON metadata never match). Applies to every balance and the item list
- `--group-by-metadata KEY` (alias `--group-by`): Group outstanding items by top-level `KEY`, with per-group counts and subtotals; items without the key (or with null, blank or nested values) go to `(unassigned)`, listed last
- `--currency CODE`: Only entries of transactions in this currency. If omitted, the company's single transaction currency is used (USD if it has no transactions); if the company has several currencies, the command fails with `VALIDATION` and `--currency` is required

**Output:**
- Book balance (all entries), cleared balance (sum of clearings; only `--statement`'s when given), total cleared balance (all statements), and reconciled balance (reconciled clearings), all on the account's normal side (debit-normal for asset/expense, credit-normal for liability/equity/revenue)
- The outstanding entries, i.e. not fully cleared: entry id, transaction id, date, **reference**, description, **metadata**, amount, **cleared amount**, **remaining amount** (JSON also has `reconciled_amount` and the item's `clearings` as of `--to`). Partially cleared items are counted (`partially_cleared_count`) and flagged. Invariant: `book - total cleared = uncleared total` (the sum of the items' signed remaining amounts)
- With `--statement-balance`: the statement balance, `difference = statement - cleared`, and `reconciled: yes/no` (yes when the difference is zero)

**CSV:** one header row
`row_type,entry_id,transaction_id,date,reference,description,amount,cleared_amount,remaining_amount,currency,group,metadata`,
then one `entry` row per outstanding entry (`group` filled when grouping;
`metadata` as compact JSON or the raw text), one `group` row per group
(`description` and `group` = the value, `remaining_amount` = subtotal), then
`summary` rows whose `description` names the metric and whose `amount` holds
it (`book_balance`, `cleared_balance`, `total_cleared_balance`,
`reconciled_balance`, `uncleared_total`, plus `statement_balance`,
`difference`, and `reconciled` (`1`/`0`) when a statement balance is given).
Amounts are signed minor units.

#### AR/AP statement settlement mode

Use `--mode settlement` for carrier commissions or vendor settlement statements.
The default `--mode bank` keeps the bank behavior above. Settlement mode requires
`--statement` and an asset (AR) or liability (AP) account. `--statement-balance`
is the statement's **net payments**, rather than a bank ending balance.
`--link-key KEY` changes the payment link key from `applies_to` (settlement only).

```sh
bk --company agency report reconciliation --mode settlement --account 1210 \
  --statement S1 --statement-balance 1620 --to 2026-10-31
```

Entries are netted per transaction on the selected account and currency, as in
aging. Debits are charges for AR, credits for AP. `cleared_balance` sums only
charge clearings on this statement, plus signed charge-reversal clearings.
Clearing the payment entry cannot cancel that total.
`difference = statement_balance - cleared_balance`; `reconciled` means this
comparison is zero, even when short pays remain open. Overpayments and duplicate
lines above the charge applications leave a difference when included in the
statement net.

Payment links use aging's specific-identification syntax and resolution rules:
reference strings first, then transaction ids (JSON integers or numeric strings,
optionally prefixed with `#`), or arrays in link order. Links must name a charge
on the same account/currency, dated no later than the payment and `--to`.
Settlement mode does not apply unmatched payments or excess FIFO to unrelated
charges. Missing/invalid links and unusable targets are listed with reasons in
`unmatched_settlements`; partially usable arrays retain their reasons in
`applied_settlements`. Unmatched credits are never open charges.

For each charge, `expected` is the original net charge minus `txn reverse`
charge reversals through `--to`. `applied_this_statement` is its charge clearing
on the selected statement. `applied_total` is the greater of all charge clearings
(net of charge-reversal clearings) and linked payment allocations through `--to`,
so posting a payment and clearing its charge do not count twice. `remaining` is
`max(expected - applied_total, 0)`. `short_pay` is that remainder for a charge
with application on this statement; `overpay = max(applied_total - expected, 0)`.
For arrays, payments fill linked charges in order; excess stays on the last valid
charge and is shown explicitly. Separate duplicate postings remain visible;
posting retries with the same reference still use normal idempotency.

Open charges keep their original reference, date, metadata and `age_days`.
Charge reversals appear in `negative_charges`, dated and statement-scoped to
the reversal itself. Reversing a payment creates a new positive charge, as in
aging. Matching runs before metadata filtering. Filters select charges by their
metadata, and linked payments follow them even if payment metadata lacks the
filter key. Groups subtotal open charges using original metadata. Unmatched
credits use their own metadata for filtering. Charge and payment explanations
cover the account through `--to`, including other statements; only
`applied_this_statement` and the comparison are statement-scoped.

The JSON envelope, common fields and 12 CSV columns remain. `uncleared_entries`
contains only open charges; `uncleared_total` sums their remaining amounts. The
bank-only `book_balance - total_cleared_balance` invariant does not apply.
`book_balance` remains the filtered ledger balance; clearing balances contain
charge-side clearings. JSON adds `mode`, `link_key`, `charges`, `negative_charges`,
`applied_settlements`, and `unmatched_settlements` only in settlement mode.
CSV adds `charge`, `negative_charge`, `applied_settlement`, and
`unmatched_settlement` rows alongside `entry`, `group` and `summary` rows.
For charge rows, `amount` is expected, `cleared_amount` is applied this statement,
and `remaining_amount` is remaining. For settlement rows, `amount` is the
positive credit and `remaining_amount` its excess; reasons are appended to
`description`. Table output includes charges, applied settlements and unmatched
settlements sections.

### `bk report cash-flow`

Cash flow statement for a period using the direct method.

```bash
bk --company acme report cash-flow --from 2026-01-01 --to 2026-12-31
bk --company acme report cash-flow --from 2026-01-01 --to 2026-03-31 \
  --cash-account 1000 --cash-account 1010
bk --company acme report cash-flow --from 2026-01-01 --to 2026-12-31 --currency EUR
```

**Flags:**
- `--from DATE`, `--to DATE` (inclusive, `YYYY-MM-DD`): the end defaults to today; the start defaults to January 1 of the effective end year
- `--cash-account CODE` (repeatable): accounts to treat as cash; each must exist and be an asset
- `--currency CODE`: report currency. Defaults to the company's only transaction currency; if the company has several, `--currency` is required (validation error); with no transactions, USD

**Default cash accounts:** Without `--cash-account`, every **asset** account whose name contains `cash`, `bank`, `checking` or `savings` (case-insensitive) is treated as cash -- this matches the bundled templates (`Operating Cash`, `Checking Account`, `Savings Account`). A code range is deliberately not used: `1100` is `Savings Account` in the personal template but `Accounts Receivable` in the business ones. If no account qualifies, the command exits with a validation error (exit code 3) asking for `--cash-account`.

**Method:**
- Only transactions dated in the range, in the report currency, whose cash entries have a non-zero net movement are included. Transfers between two cash accounts net to zero and are excluded.
- Each non-cash entry of such a transaction is attributed directly to its account: a credit to the counter-account is an inflow, a debit an outflow. Per transaction these sum exactly to the net cash movement, so split transactions need no allocation.
- Sections by counter-account type: revenue/expense = **Operating**; non-cash asset (receivables, inventory, equipment, ...) = **Investing**; liability/equity = **Financing**. The schema has no current/non-current distinction, so working-capital assets land in Investing.
- Opening cash = cash balance before `--from`; closing cash = cash balance through `--to`, computed independently. `opening + net change = closing` is checked and a mismatch is flagged (`[!!] MISMATCH` in the table, `reconciled: false` in JSON).
- CSV columns: `row_type,section,code,name,account_type,amount` with `row_type` `line`, `subtotal` or `summary` (amounts in minor units).

## Categorization rules

`bk rule` manages company-scoped mappings applied automatically during CSV and
OFX/QFX bank imports. Supply `--company` or `BEANKEEPER_COMPANY`.

```bash
bk --company acme rule create --contains Starbucks --account 5000 --priority 10
bk --company acme rule list --json
bk --company acme rule list --format csv
bk --company acme rule match --description 'Starbucks 102' --json
bk --company acme rule amend "$RULE_ID" --contains Starbucks --account 5100 --priority 10
bk --company acme rule delete "$RULE_ID" --force
```

- `create --contains TEXT --account CODE [--priority INTEGER]`: stores a rule
  with a `rule_...` TypeID. The pattern is trimmed, must be nonempty, and is
  normalized using Unicode lowercase. The destination account must exist in
  this company. Priority defaults to `0`; negative values are supported.
- `list`: returns rules in evaluation order, highest priority first, then
  ascending rule ID. JSON `data` is an array of `{id, company_slug, contains,
  account_code, priority}` objects. `create` and `amend` return a one-element
  array. Amended rules also carry `supersedes` with the prior rule ID.
- `amend ID --contains TEXT --account CODE [--priority INTEGER]` supersedes an
  active rule with the full replacement terms, including the default priority
  when omitted. Use the returned ID for subsequent changes.
- `match --description TEXT` checks the same rule order without posting. JSON
  returns `{"outcome":"match","rule":{...}}` or `{"outcome":"no_match"}`.
- `delete ID [--force]`: removes only this company's rule. Prompts on interactive
  stdin unless forced. JSON `data` is `{deleted: ID}`. Unknown or inactive IDs
  return `NOT_FOUND` (exit 5).

Matching lowercases the entire imported description and finds a literal
substring; `%`, `_`, and regex characters are ordinary text. CSV uses its mapped
description column. OFX combines `NAME` and `MEMO`. The first match selects the
contra account, preserving the bank account, direction, amount, currency, and
source reference. Both inflows and outflows are eligible. Unmatched rows use the
required `--suspense` account. A rule selecting the bank itself causes an import
validation error (CSV rolls back the batch; OFX records a per-row error).
Account tax defaults are inherited by the actual selected entries.

Rules apply to dry runs. JSON journal batches remain explicit and ignore rules.
Generated CSV references are computed before categorization, so adding or
removing rules does not duplicate existing transactions. Previously posted
transactions remain unchanged. Updates are atomic with audit snapshots
(`--entity rule`, actions `create`, `rule-supersede` and `rule-delete`).
An account cannot be deleted while an active rule references it.

| MCP tool | Arguments |
|---|---|
| `rule_list` | `company` |
| `rule_create` | `company`, `contains`, `account`, optional integer `priority` |
| `rule_amend` | `company`, `id`, `contains`, `account`, optional integer `priority` |
| `rule_match` | `company`, `description` |
| `rule_delete` | `company`, `id` |

## Standing bookkeeping and inform preferences

Use `bk --company COMPANY bookkeeping-policy show` to read both instructions.
Use `bk --company COMPANY bookkeeping-policy set sales-tax=VALUE` or
`bk --company COMPANY bookkeeping-policy set processor-fees=VALUE` to set one.

| Policy kind | Values |
|---|---|
| `sales-tax` | `unset`, `track-liability`, `marketplace-remitted`, `not-applicable` |
| `processor-fees` | `unset`, `separate-expense`, `net-from-revenue` |

Use `bk --company COMPANY inform-preference show` or
`bk --company COMPANY inform-preference set VALUE`. Values are `per-change`
(the default), `daily-digest` and `weekly-digest`.

| MCP tool | Arguments | Result |
|---|---|---|
| `bookkeeping_policy_show` | `company` | `{sales_tax, processor_fees}` |
| `bookkeeping_policy_set` | `company`, `policy` with `{kind, value}` | `{sales_tax, processor_fees}` |
| `inform_preference_show` | `company` | `{preference}` |
| `inform_preference_set` | `company`, `preference` | `{preference}` |

Setting changes take effect immediately and audit both values under
`bookkeeping_policy` or `inform_preference`. Restore an earlier value with the
same set operation. Repeating a configured value leaves the audit history
unchanged. These settings record the owner's instructions for the agent and
leave posted transactions and capability enforcement intact.

## Audit

The ledger is append-only: posted transactions are never edited or deleted
(corrections go through reversing entries). The few operations that *do* change
existing rows in place each append an immutable record to the audit trail,
capturing the actor (`--actor` / `BK_ACTOR`), a timestamp, and JSON before/after
snapshots. The recorded operations are:

| Action | Recorded when |
|--------|---------------|
| `status_change` | `bk txn clear` adds, promotes or removes an entry's clearings. `before`/`after` hold the derived entry `status` and `cleared_amount`; `after` adds `op` (`clear`, `reconcile`, `unclear`) and `clearings` (id, amount, cleared_date, reconciled_date, statement, status of the affected rows). Skips record nothing |
| `metadata_set`, `metadata_unset` | `bk txn meta set/unset` changes annotations; whole-object snapshots, one row per changed batch, no row for no-ops |
| `reverse` | `bk txn reverse` posts and links a reversal |
| `correlate` | `bk txn post --correlate` links a partner transaction |
| `proposal_approve`, `proposal_reject` | Approval, rejection or withdrawal snapshots, filter with `--entity proposal` |
| `limit_set` | Spend limit set/clear snapshots, filter with `--entity spend_limit` |
| `budget_set` | `bk budget set` creates or revises a budget (prior amount captured) |
| `budget_delete` | `bk budget delete` removes a budget |
| `account_delete` | `bk account delete` removes an account |
| `company_delete` | `bk company delete` removes a company |

### `bk audit log`

List recorded changes, most recent first. Does not require `--company` (scans
all by default).

```bash
bk audit log                                      # 50 most recent changes
bk audit log --company acme                        # one company
bk audit log --by-actor month-end                  # one recorded actor label
bk audit log --entity entry                        # clearance/reconciliation history
bk audit log --action company-delete --limit 100   # last 100 company deletions
bk audit log --json                                # full before/after snapshots
```

**Flags:**
- `--company SLUG`: Restrict to one company
- `--by-actor NAME` and `--agent URI`: Match the recorded label and verified URI respectively. Both filters apply when supplied together.
- `--entity KIND`: One of `entry`, `transaction`, `budget`, `account`, `company`
- `--action KIND`: One of `metadata_set`, `metadata_unset`, `status-change`, `correlate`, `reverse`, `budget-set`, `budget-delete`, `account-delete`, `company-delete`
- `--limit N` (default: 50), `--offset N` (default: 0)

JSON keeps the row array and adds `meta.pagination` (`limit`, `offset`, `returned`,
`total`, `has_more`). Table mode signals more rows on stderr unless quiet.
Continue with `--offset`; the total uses the same agent/company/entity/action filters.
Audit JSON also includes parsed `before_display` and `after_display` snapshots
with decimal monetary companions; original snapshot strings and hashes remain
unchanged. See [amount conventions](json-api.md#amount-conventions).

## Agent spend limits

```sh
bk agent limit set URI --company acme --currency USD --max-post 500 --daily-limit 2000
bk agent limit show URI [--company acme]
bk agent limit clear URI [--company acme]
```

`set` requires a company, currency and at least one nonnegative major-unit cap.
It replaces that currency's row; omitted caps become unlimited. `show` returns
rows, optionally scoped by company. `clear` removes all currency rows in scope,
or all companies when no company flag/environment default is set. Set/clear
requires `ledger/admin` under enforcement and records `limit_set`; show requires
`ledger/read`. JSON rows contain `agent_uri`, `company_slug`, `currency`,
`max_post`, `daily_limit` and monetary display strings; clear returns `{cleared:N}`.

All live tokens for one URI share company/currency limits. `max-post` measures
total debits including splits. `daily-limit` sums posted debits on the transaction
`date`, including approved requests charged to the proposer. Pending requests
consume nothing; backdated/future-dated posts use their specified date's bucket.
No rows means no amount cap. With any row for an agent/company, postings in an
unconfigured currency fail closed with `FORBIDDEN` (exit 7). Imports, loan,
investment and schedule posts use the same atomic check. Configured limits apply
to verified agents even with enforcement off; named actors without tokens have
no URI bucket. See [identity documentation](../../agent-identity.md#spend-limits-and-proposals)
for bootstrap, ownership and migration details.

## Agent identity

- `bk key init`: create one local root in an **encrypted** existing ledger.
  Returns public `kid`, `trust_root`, `public_key`; refuses reinitialization.
- `bk agent issue --name PREFIX --public-key BASE64 [--caps PATH,...] [--ttl 8h]`:
  issue an attestation for the supplied Ed25519 public key. `--generate-key`
  replaces `--public-key` and prints the agent private seed once as hex.
  Names are lowercase TypeID prefixes. The URI path is `ledger`, allowing sibling
  grants. Bare `ledger` is rejected. Capabilities default to `ledger/read,ledger/post` and must be under
  `ledger`. TTL accepts positive `s`, `m`, `h`, `d` values, maximum `30d`.
- `bk agent list`: list public claims and nullable `revoked_at`, without tokens
  or private keys.
- `bk agent revoke URI_OR_JTI`: revoke one token or all current tokens for a URI.
  Repeating is a no-op. Unknown targets return `NOT_FOUND`.
- `bk audit log --agent URI`: select audit rows attributed to that verified URI.

All support `--json`. Issue data includes `agent_uri`, `token_jti`, `agent_key`,
`capabilities`, `issued_at`, `expires_at`, `token`, plus `private_key` only when
requested. JSON envelopes add `meta.agent` when a valid token is supplied.
`txn show`/`list` and audit rows expose nullable `agent_uri`/`token_jti`;
audit rows also expose `hash_version` (1 for historical rows, 2 for preserved identity rows, 3 for new rows).
Audit filters also support entities `key`, `agent` and actions `create`,
`key-init`, `token-issue`, `token-revoke`, `attach` (JSON uses underscores).

`bk mcp serve` accepts the global token flag/environment variable per process.
Stdio verifies it at startup and on every call, including live revocations. HTTP
uses it only when the request has no Authorization header; every HTTP request
requires a verified token even with enforcement off. Tool arguments do not accept
tokens. While enforcement is off, CLI and stdio still accept tokenless named actors.

Tokens remain bearer credentials without proof of agent private-key possession.
Capability checks apply when this ledger enables enforcement. `init` rejects
supplied tokens before modifying files; initialize the new
ledger and root without one. Root keys travel with full encrypted DB backups,
not accounting exports. There is no recovery or rotation command in this phase.
See [identity setup and limits](../../agent-identity.md).

## Investment lots

`security create SYMBOL --name NAME [--currency USD]`, `security list`, and
`security price SYMBOL --date DATE --price AMOUNT` maintain company-scoped
securities and append-only prices. `investment buy SYMBOL --quantity Q --cost C
--date DATE --asset ACCOUNT --cash ACCOUNT [--reference KEY]` creates a purchase
lot. `investment sell SYMBOL --quantity Q --proceeds P --date DATE --asset ACCOUNT
--cash ACCOUNT --gain REVENUE_ACCOUNT [--method fifo|lifo|specific] [--lot LOT_ID]
[--reference KEY]` relieves lots and records gain/loss in one transaction.
`investment lots SYMBOL` includes original and remaining basis/quantities;
`investment holdings [--as-of DATE]` reports valuations without postings.

All commands require a company and support table/JSON/CSV output. MCP mirrors
them as `security_create`, `security_list`, `security_price`, `investment_buy`,
`investment_sell`, `investment_lots`, and `investment_holdings`. The three reads
remain available with `mcp serve --read-only`. See the [full investment guide](../../investments.md)
for account setup, exact quantities, rounding, date ordering, and phase-one limits.


## Capability policy

```sh
bk policy show
bk policy set --enforce on
bk --agent-token "$ADMIN_TOKEN" policy set --enforce off
```

Enforcement defaults off, including for upgraded ledgers. Before enabling, issue
and save a token with `ledger/read,ledger/admin`. A named actor can enable while
off; afterward all ledger commands require tokens and policy changes require
`ledger/admin`. `policy show` requires `ledger/read`. JSON data is
`{"enforce": true, "updated_at": "<UTC timestamp>"}`. Changes append atomic
`policy_change` before/after audit snapshots, filterable with
`audit log --entity policy --action policy_change`. Repeating a state is a no-op.

| Required capability | Commands |
| --- | --- |
| `ledger/read` | List/show, report, export, verify, audit log, agent list, policy show, loan schedule |
| `ledger/post` | txn post/import/attach/meta set/unset, investment buy/sell, security create/price, loan pay, schedule create/run/cancel, rule create/amend/delete, bookkeeping-policy set, inform-preference set |
| `ledger/propose` | txn propose, list/withdraw owned proposals |
| `ledger/approve` | List all proposals, txn approve/reject |
| `ledger/reverse` | txn reverse |
| `ledger/reconcile` | txn clear/reconcile |
| `ledger/budget` | budget set/delete |
| `ledger/admin` | company/account create/delete, key init, agent issue/revoke, agent limit set/clear, policy set, backup/restore |

Matching uses leaf requirements and path segments. Neither `ledger/admin` nor
`ledger/post` grants sibling capabilities; issue their explicit union when
needed. Bare `ledger` grants are rejected at issuance and acceptance. A missing
or rejected token produces `UNAUTHORIZED` (exit 6). A verified token lacking the
required grant produces `FORBIDDEN` (exit 7), naming that grant without secrets.

New-ledger `init` rejects supplied tokens. To overwrite an enforced ledger,
first disable enforcement with an admin token; `init --force` checks the actual
`--path`/`--db` destination with the existing passphrase. Unreadable ledgers are
not overwritten. `loan schedule` without an existing database remains a
standalone calculation; otherwise it follows the read policy.

Stdio MCP filters tools by token grants at startup; HTTP filters discovery by
the current request's token and policy. Both recheck policy and grants on every call. `--read-only` removes all writes, including `txn_reconcile`, even
with broader grants. Restart stdio MCP to refresh discovery after a policy change.
Back up before the schema v18 migration: v1/v2 audit hashes are preserved, but
new v3 audit rows block older binaries from appending, even with enforcement off.
See [identity setup and limits](../../agent-identity.md).

### What changed: comparisons and explanations

`income-statement`, `trial-balance`, `trend`, `cash-flow`, `balance` and
`balance-sheet` accept `--compare previous|year-ago|YYYY-MM-DD..YYYY-MM-DD`.
`prior-period` and `prior-year` remain aliases. `--explain-change ACCOUNT_CODE`
requires comparison and returns ranked transactions and payees for that line.
The MCP `report` tool accepts strings `compare` and `explain_change` with the
same semantics. For spending by category select `trend --metric spending`.

[Full comparison contract](../../comparisons.md) covers date defaults, activity
versus snapshots, currency handling, zero bases, movers, CSV rows and Desktop
links. `--period` remains exclusive with `--compare` on income/trial reports;
trend comparison compares the full selected range rather than calendar buckets.
