---
name: beankeeper
version: 0.28.0
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

## Install and set up Beankeeper on this machine

- **Desktop-managed books.** Follow the Desktop skill for this operating system to
  install the verified release, check the local service, and register the agent.
  Have the owner handle the recovery kit directly.
  [Beankeeper Desktop skill](https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/SKILL.md)
- **Standalone local books.** Install `bk` and confirm it with `bk --version`.
  For an existing ledger, use its configured path. For new books, choose a local
  database path and have the owner initialize it with `bk --db PATH init --encrypt`,
  entering the passphrase directly. Use `bk --db PATH --json verify` to check the
  ledger. To connect an MCP client, configure `bk --db PATH mcp serve` with the
  documented noninteractive passphrase source.
  [CLI installation and release verification](../../README.md#install) ·
  [MCP setup](../../README.md#connect-an-ai-assistant-through-mcp)

For binary installation, `install.sh` verifies the archive and checksum file's
Sigstore bundles when `cosign` is on `PATH`, before extraction or installation.
Missing bundles (HTTP 404) warn that the release predates signing and fall back
to SHA-256. Present bundles must verify; other download errors and verification
failures stop installation. Without cosign it warns and verifies SHA-256 only.
See the [release verification contract](../RELEASE.md#sigstore-verification-contract)
for artifact/bundle names, exact release-tag identity, issuer, and manual
`cosign verify-blob` / `gh attestation verify` commands. Sigstore signing does
not replace Apple notarization or Windows Authenticode.

## Set up books on the normal connection

Use `ledger_setup_status` and `ledger_setup_book_show` to see what already
exists. Ask the owner conversationally for the missing information. A
bookkeeper in either write mode can call `ledger_setup_book_create`, `ledger_setup_book_initialize`,
`ledger_setup_chart_apply`, and `ledger_setup_account_add` with a
`target` containing the company slug and a stable reference. Omit
`target.generation`.

Use `ledger_setup_opening_post` for opening balances and
`ledger_setup_statement_import` for CSV or OFX/QFX statement text. Reuse
the same reference and inputs on a retry. Report completed changes in plain
language, distinguish newly imported postings from preexisting matches, and
offer owner undo using the returned `receipt_id`. With an existing propose-only
credential, opening balances return a `proposal_id` and statement imports
return `proposals` for the new rows. Report those as pending until they are
approved through `txn_approve`. Use `txn_proposals` with
`include_rejected: true` to check current status after a retry. If Desktop is
available, use `desktop_path` to offer the
detailed receipt.
Owners can handle their recovery material directly while the agent completes
the bookkeeping.

[CLI and MCP onboarding commands](references/commands.md#direct-onboarding)

## Answering "How am I doing?"

Call `financial_summary` with `{"company":"SLUG"}` or run
`bk --company SLUG summary --json`. Use its brief `data.highlights` as the factual
starting point for your answer, then explain relevant attention counts and totals.
The default compares this month to date with the full last calendar month;
state the returned dates. Optional `from`, `to`, `compare_from`, `compare_to`,
`currency`, and account selector arrays use the same CLI/MCP calculation.

Draw charts from `standing`, `comparison.series`, `biggest_movers.series` and
`needs_attention.items`; monetary values are minor units with `_display`
companions. Supply your own advisor voice, and offer `desktop_view` only if a
visual would help. Do not send the owner to a dashboard to get the answer.
Overdue status is unavailable without invoice due dates. Older open items,
named suspense activity and scheduled posting volume carry explicit limits;
do not present them as overdue bills or unresolved transaction counts.
`needs_attention.items.stale_holds` lists card authorizations held open for
more than seven days as of the summary end date. Explain that these amounts can
still reduce available funds. Override the age with `stale_hold_age_days` or
`--stale-hold-age-days`.
The MCP tool returns the full `{ok, meta, data}` or `{ok, meta, error}` CLI
envelope and is available in read-only mode with `ledger/read`.
See [financial summary details](../financial-summary.md).

## Answering "Do my books check out?"

Call MCP `verify` with `{}` or run `bk --json verify`. Verification covers the
whole ledger. On success, say which checks passed in plain language. For
example, "I checked 42 transactions across two companies. Debits and credits
matched in each currency, and all 56 audit records matched their hashes."
Use `row_counts`, `companies[].currencies`, and `audit_chain` for the numbers.
Keep currencies separate. The debit and credit amounts are exact strings in
minor units. Use `minor_unit_digits` to convert them for display or charting.
Mention any check marked `skipped` separately.

Describe the result as local consistency and tamper evidence. A complete
rewrite or rollback can still produce a consistent ledger. If verification
fails, report the named check and record or company from the `DATABASE` error
instead of giving a clean bill of health. If Desktop is available, pass
`desktop_view.link` to `ui_get_link` and offer its URL for optional verification
status and recheck. See the [verify payload](references/json-api.md#verify).

## Core Concepts

- **Double-entry**: Postings are validated at runtime before saving. Unbalanced or mixed-currency entries are rejected; the library's `Transaction` type can only be constructed through `JournalEntry::post()` after validation.
- **Companies**: Multi-tenant -- each company has its own chart of accounts and ledger. Specified via `--company SLUG` or `BEANKEEPER_COMPANY` env var.
- **Accounts**: Five types: `asset`, `liability`, `equity`, `revenue`, `expense`. Each has a code (e.g. `1000`) and a normal balance direction (debit or credit).
- **Amounts**: Always specified in **major units** (dollars, not cents) on the CLI. Stored internally as minor units (cents). Example: `2500` means $2,500.00.
- **Append-only ledger**: Database triggers reject edits, deletion, and replacement of posted financial fields and entry details. Transaction metadata and entry clearance status remain mutable. Corrections use reversing entries.
- **Audit trail**: Posts, imports, and reversals store a poster (`posted_by`) and a `create` record. Supported mutations record an actor, timestamp, and before/after snapshots atomically. The CLI and MCP use `--actor`, then `BK_ACTOR`, then the verified agent URI for the label. `bk audit log --by-actor NAME` filters by exact label and `--agent URI` filters by verified identity. `bk audit log --action create` lists creations. JSON audit rows include `prev_hash` and `row_hash`.
- **Unverified runtime hints**: `bk --json audit log` and MCP `audit_log` include a nullable `client_hint`. When present, describe the runtime as unverified. Use `agent_uri` for verified identity. The hint stores only known environment marker names and carries no permissions.
- **Verification**: `bk verify` and MCP `verify` return the checks and evidence from one ledger snapshot. Legacy audit rows retain their IDs and timestamps after migration, and legacy posters stay unknown. See [audit integrity](../audit-integrity.md).
- **Archived accounts**. Receipt undo can archive an account. Its row and past entries remain available for reporting and reversals. Choose an active account for new postings and categorization. Use the `account_archive` audit action to inspect the recorded change.
- **Idempotency**: `--reference KEY` is trimmed, stored verbatim, and unique per company. Empty references are rejected. Use `--on-conflict skip` for safe retry of duplicate posts; Only `error` and `skip` are accepted; use `txn meta set/unset` to annotate existing postings.

For detailed accounting concepts and account types, see [`references/accounting.md`](references/accounting.md).

## Annotating posted transactions

Use `bk --company acme txn meta set 42 status=disputed dispute_id=D-42` or
`txn meta set --reference CHARGE-42 status=disputed`. Remove annotations with
`txn meta unset 42 status dispute_id` or select by `--reference`.
Values are strings, including numbers and booleans, as with `txn post --meta`.
Reserved keys are `correlate`, `reverses`, `reversed_by`, `proposal_id`,
`applies_to`, and `beankeeper_source`. Set custom settlement link keys at post
time and treat them as relationships too. Each key must be unique, nonempty,
and free of double quotes and NUL.
Each changed batch records a `receipt` with whole-object snapshots and returns
a `desktop_path` for optional detail. Tell the owner what changed and give them
the receipt for one-step undo with `bk audit undo --company ACME RECEIPT` or
`audit_undo` using your signed direct bookkeeper grant. Undo newer annotation edits first.
No-op retries have a null receipt. Removing the last key leaves `{}`. Dates and
financial fields stay unchanged, so tagging an original AR charge preserves
its age under `report aging --filter-metadata status=disputed`.

## Linking related transactions

Use `bk --company acme --json txn link 43 42 --kind refund` or call `txn_link`
with `{"company":"acme","source_id":43,"target_id":42,"kind":"refund"}`.
Point a refund at its original charge or a payment at its invoice. Other
kinds are `transfer`, `adjustment`, `recategorization`, and `related`.
Use the `links` array from either transaction in `txn_show` or `txn_list`.
Lists hide links to either side of a reversed pair unless you pass
`"include_reversed": true` (`--include-reversed` on the CLI). To inspect all
active links for a transaction by ID, use `txn_show`.
For transactions in different books, use `txn_post` with `correlate`.

Tell the owner which transactions you linked and why. Offer the result's
`desktop_path` for optional detail and use its `receipt` with `audit_undo`
or `bk audit undo --company acme RECEIPT` if asked to remove the link.
Undo uses a signed direct bookkeeper or owner grant and preserves the audit
history. Links do not change balances or postings.

## Record first and inform the owner

Use `bk txn post` or MCP `txn_post` for ordinary bookkeeping. The bookkeeper
grant posts directly by default. After a successful post, tell the owner what
you recorded in plain words. Include the amount and currency, the company and
transaction ID, and offer to undo the posting in one step. Offer a Desktop
detail link when one is available.
Use `--currency` in the CLI or `currency` in MCP with an active, three-letter
ISO 4217 currency code. Use the currency's decimal precision for major-unit
amounts, such as `45` for KRW or `1.234` for TND. Fund codes, test codes,
and codes without a defined minor unit are rejected. If you omit currency,
the posting uses `BEANKEEPER_CURRENCY` when set and USD otherwise.

Use `bk txn reverse ID` or `txn_reverse` in the selected company to reverse a
posting. Use `bk audit undo RECEIPT` or `audit_undo` when an eligible receipt
identifies the change. The direct bookkeeper grant applies corrections
immediately and preserves the audit link. Never create a hand-made offsetting
entry to undo a posting. Tell the owner what changed and offer the Desktop
detail link when available.

To correct the account on a posted line, use `bk txn recategorize` or
`txn_recategorize`. Supply the original transaction's ID or reference and
the destination account. If the tool cannot infer a single source line,
inspect the transaction and supply its entry ID or a source account with
one matching line. After the move, explain what changed using the returned
amount and account codes, offer the optional `desktop_path`, and offer
one-step undo with `audit_undo` and the returned `receipt_id`. For exact
retries, reuse `posting_reference` and set `on_conflict` to `skip`.

For a card authorization, use `txn_post` with `status: "pending"` or
`bk txn post --status pending`. Supply one debit and one credit on different
accounts and use a stable authorization reference. A CSV or OFX/QFX import can
settle an open hold when the statement line uniquely matches its bank account,
currency, direction, payee, and amount within seven days. Set the optional
`hold_amount_tolerance` in minor units or `hold_posting_window_days` on
`ledger_setup_statement_import`. The CLI accepts the same options as
`--hold-amount-tolerance` and `--hold-posting-window-days` on
`bk setup statement-import` and `bk txn import`.

Use the posted ID, `settlement.hold_id`, and `settlement.amount_delta` to tell
the owner what posted and how it differed from the hold. Offer the statement
receipt or register link when a visual would help. Retry with the same
reference. If the import finds multiple candidates and you can identify the
intended hold, call `txn_settle_hold` using the posted reference from the
import error. For explicit settlement, call `txn_settle_hold` with `company`, `hold: {"id":42}` or
`hold: {"reference":"hold-1"}`, the actual decimal `amount`, the posted
`date`, and the bank's posted `reference`. In the CLI, use
`bk txn settle-hold --id 42 --amount 43.88 --date 2026-03-04 --reference posted-1`.
Pass the same values on retry. Tell the owner the actual charge and difference
from the authorization. For undo, offer to reverse the settled transaction or
undo its receipt, which restores the hold. A propose-only response is a
proposal that awaits the ledger's usual approval.

For a bank balance question, use `account_list` with `with_balances: true`
or `bk account list --with-balances`. Report the posted balance and what is
available after holds. Use the numeric fields and currency for charts. Offer
the response's `desktop_view.url` on the matching Desktop listener when useful.

When the owner asks what changed, run `bk --company acme --json audit receipts`
or call `audit_receipts` with `company`. Pass the saved `cursor` for that ledger
and company, or omit it on the first read. Copy the returned `continuation`
into the next call until it is null, then save `through` as the next cursor.
Describe who changed what in plain words and use the typed amounts and
currencies for any chart. Offer the `desktop_path` on the matching Desktop
listener when its address is known.

When the owner asks to undo a receipt, run
`bk --company acme --json audit undo RECEIPT` or call `audit_undo` with
`company` and the numeric `receipt` ID. Use your signed bookkeeper credential
for a posting, annotation edit, or account rename. Report the result and its
`undo_receipt`. A repeat returns the same result. If the ledger returns
`UNDO_OWNER_ONLY`, explain the reason and ask the owner to have their signed
administration credential perform the correction. For `UNDO_UNSUPPORTED` or
`UNDO_INELIGIBLE`, explain the reason and the available next step in the
conversation. [Receipt commands and results](references/json-api.md#audit-receipts)

If the owner issued your credential with `--write-mode propose-only`, the same
post or correction returns `status: "pending"` and a proposal ID. Tell the owner
what is pending. A separate signed reviewer can apply it.
Use the explicit `txn propose` or `txn_propose` operation for ad hoc sign-off
when the owner requests it.

## Existing spend limits and proposal operations

`bk agent limit set URI --company acme --currency USD --max-post 500 --daily-limit 2000`
sets optional per-post and transaction-date limits, measured by total debits.
Use `agent limit show URI [--company acme]` to inspect and `agent limit clear URI
[--company acme]` to remove all currencies in scope. Set/clear requires
`ledger/admin` under enforcement; configured limits apply to verified identities
even with enforcement off. All tokens for one URI share usage. With any limit
for an agent/company, other currencies require matching rows; no limits means no
amount cap. Named actors without a token have no URI bucket.

If a configured limit denies a posting, report the named error to the owner.
A verified token is always required to propose. Proposals freeze accounts,
tax defaults and payload, reserve their
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
- `txn list` and `audit log` default to 50 rows. For transaction pages, advance `--offset` by the returned row count while `meta.pagination.has_more` is true. Audit pages accept up to 100 rows. Continue with `meta.pagination.next_offset` (MCP: `_meta.pagination.next_offset`) and the same filters. Use an audit limit of zero for just the matching count in pagination metadata.
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
Issue a replacement credential with `--write-mode propose-only` when the owner
opts into sign-off for that agent. Issuance and listing show the `write_mode`.

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
| `ledger/reverse` | Reverse postings and undo eligible audit receipts |
| `ledger/reconcile` | txn clear/reconcile |
| `ledger/budget` | budget set/delete |
| `ledger/chart` | Rename account labels on the bookkeeper connection. Direct applies immediately, and propose-only uses sign-off. |
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
Back up before upgrading to schema v18. The upgrade preserves old audit hashes,
and appending v3 audit rows requires a newer binary.

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
never include a passphrase in tool arguments. Stdio uses one ledger for the
life of the process.

Send nested tool arguments as JSON objects or arrays. Beankeeper also accepts
JSON text in those fields for clients that stringify them. If a tool rejects
its arguments, explain the error in plain words, correct the arguments, and
retry with the same reference where the tool supports one. Keep bookkeeping
writes in MCP or the CLI and offer Desktop views for optional detail.

For a shared Streamable HTTP endpoint, start `bk mcp serve --http`, optionally
with `--bind 127.0.0.1:PORT` (default `127.0.0.1:8750`). The endpoint is `/mcp`;
the actual bound address is printed to stderr. Every request requires a verified
`Authorization: Bearer <agent token>`, even when ledger enforcement is off.
`--agent-token` / `BK_AGENT_TOKEN` supplies a fallback for startup ledger requests
with no Authorization header.
Explicit invalid credentials never fall back and return HTTP 401 `UNAUTHORIZED`.
With enforcement on, discovery filters the current caller's grants; insufficient
grants return `FORBIDDEN`. Credentials and policy are rechecked per call, including
expiry and revocations. `--read-only` always removes writes. Non-loopback binds
require `--allow-remote`, which serves plaintext HTTP without TLS and is not a
hosted-tier implementation. Browser Origin headers are rejected. Prefer per-request
tokens for multiple agents; do not put credentials in tool arguments or URLs.

If the operator configured another ledger, send `Beankeeper-Ledger-Target: NAME`
and that ledger's bearer on each HTTP discovery or tool call. The same MCP
session can address multiple ledgers. Target selection is request-scoped and
defaults to `startup`. In the CLI or stdio, use `--ledger NAME=PATH` with
`--ledger-target NAME` to address the same ledger. A company slug selects a
company inside whichever ledger the request uses. See the
[named ledger command reference](references/commands.md#select-a-ledger).

For a host-owned ledger such as separate demo books, use the startup bearer
and the name supplied by the host on the existing HTTP session. Use the
`_meta["beankeeper/ledgerTarget"]` marker on MCP results or `meta.ledger_target`
in CLI JSON when telling the owner which books were read or changed. A detached
target returns `NOT_FOUND`. Check the intended books before retrying.

If an embedded host returns `LEDGER_WRITE_DENIED`, tell the owner which ledger
refused the write and why, using `details.ledger_target` and `details.reason`. Keep
read requests on the same ledger. A write to another ledger needs the owner's
intent to use those books.

Rust host applications can merge `beankeeper_cli::mcp::http_router(&cli, read_only,
config)?` beside their own axum routes. It already mounts `/mcp` and retains the
same authentication and capability rules. The host supplies
`StreamableHttpServerConfig`, controls its cancellation token, and owns the
listener, runtime, shutdown, and Host/Origin rules. See the API Rustdoc example.

Rust hosts that supply their own response metadata can convert a `CliError`
with `beankeeper_cli::output::json::EnvelopeError::from(&error)` to carry the
shared CLI and MCP error code, message, and any structured details.

For a host that also opens `Db::open_snapshot`, resolve the passphrase once and
use `beankeeper_cli::mcp::http_router_with_passphrase(&cli, passphrase.as_ref(),
read_only, config)?`. Pass the same borrowed secret to the snapshot, then drop it
after both connections open. `None` opens an unencrypted ledger. This factory
ignores CLI passphrase sources and `BEANKEEPER_PASSPHRASE_CMD`; it needs no file
descriptor bridge or plaintext secret file on any platform. MCP authentication
and capability checks remain the same.

Path-based Rust ledger opens return `CliError::SchemaMismatch { stored, expected }`
for a supported older ledger. Both fields are typed schema versions. Publish
a `VerifiedSnapshot`, give it to `Db::open_after_snapshot`, and pass the
resulting connection directly to the host dispatch API.

Embedded hosts can compose their own tools with `http_router_with_host_tools`
and `HostToolHandler`. Each host tool declares a required grant and read-only
effect; discovery and dispatch require the current bearer's grant even with
ledger enforcement off. `--caps ui/navigate` issues a separate `/ui/` identity.
Ledger grants do not authorize navigation, and mixed ledger/UI grant lists are
rejected. Navigation is a mutation and is removed by the read-only ceiling.
See [the host API contract and example](../mcp-host-tools.md).

An embedded host serving a single already-opened ledger can pass its `Db`
directly to `http_router_with_ledger_targets` for SSE resumption via
`Last-Event-ID`. The `LedgerTargets` registry supports switching ledgers and
returns HTTP 405 for replay requests. Use `Cli::ledger_bindings()` to read the
validated names and paths supplied through `--ledger`.

- Pass `company` explicitly for account, transaction, report, and budget tools.
- Use `company_list` / `company_show` and `account_list` / `account_show` to inspect the ledger before posting. `account_create` can add an account.
- When the owner asks to change an existing account's label, rename the account by its current code. Use `account_rename` with `{"company":"acme","code":"1000","name":"Operating Cash"}` or `bk --company acme account rename 1000 "Operating Cash"`. This keeps its identity, balance, and posting history together. Tell the owner the new label and share the returned undo receipt and optional Desktop link. With a propose-only grant, report the proposal ID for `txn_approve` instead.
- Use `company_create` with `slug`, `name`, and optional `description` to add a company. Use `ledger_setup_book_create` when the company also needs a setup profile. Both use the same audited company insert.
- Call `company_delete` with `slug` when the owner asks to delete that company. The call confirms deletion, records the eligible company's prior state in the audit log, and returns `{"deleted":"slug"}`. On `VALIDATION` for retained data, explain that the company keeps its history and use `txn_reverse` for posting corrections. Both company tools require `ledger/admin` under enforcement and are hidden in read-only mode.
- Call `txn_post` with `debits` and `credits` arrays of `{account, amount, memo?}`. Amounts are decimal strings in major units. Direct grants post immediately. Propose-only grants return a pending proposal. Supply a stable `reference` and `on_conflict: "skip"` for retries of posted references. Find a pending reference with `txn_proposals`. `meta` is a map of string keys to string values; `tax` is an array of `account=category` strings.
- Call `txn_meta_set` with `company`, exactly one of `id` or `reference`, and `values: {"status":"pending"}`. Legacy arrays such as `["status=pending"]` still work. `txn_meta_unset` takes the same selector and `keys: ["status"]`. Report the returned `receipt` to the owner. Pass it with `company` to `audit_undo` using your signed bookkeeper grant when they ask for a correction. Read-only mode omits both write tools.
- Call `txn_reverse` with `company` and the original `id` to create a linked mirror posting; optional fields are `date`, `description`, and `reference`. Supply the same explicit `reference` on retries to get `skipped: true` and `existing_id`. Reconciled transactions cannot be reversed. Existing clearings stay on the original; reversal entries start uncleared. This write tool is absent in read-only mode.
- Call `txn_list` or `txn_show` to inspect posted entries. Lists, counts, and pagination totals hide both sides of reversed pairs by default. Include them with MCP `"include_reversed": true` or CLI `bk txn list --include-reversed`. Use `report` with `kind` (for example `trial-balance`, `ledger`, or `cash-flow`) and the corresponding CLI options in snake_case.
- For `report` with `kind: "reconciliation"`, `mode` accepts `bank` (default) or `settlement`. Settlement mode requires `statement` and accepts `link_key` (default `applies_to`) for payment-to-charge metadata links.
- Use `security_create`, `security_list`, and `security_price` to register symbols, inspect them and record price observations. Trade with `investment_buy` / `investment_sell`, inspect basis with `investment_lots`, and value positions with `investment_holdings`. Quantities and input money are decimal strings; `company` is required. Read-only mode retains the three inspection tools and hides the four writes. See [securities and investment lots](#securities-and-investment-lots) for account setup and trade rules.
- Use `budget_list`, `budget_set`, `budget_delete`, and `txn_clear` for their CLI equivalents. Budget deletion is confirmed by calling the tool. `txn_clear.batch` accepts an in-memory array of `{reference, amount?}` rows with an `account`; it never reads a file or stdin.
- Pass `with_balances: true` to `account_list` (`bk account list --with-balances`) for debit, credit, and balance totals. Choose a currency when a company uses more than one.
- Use `as_of` or `to` for `report` kinds `trial-balance`, `balance`, `balance-sheet`, and `reconciliation` (CLI: `--as-of` or `--to`). Aging uses `as_of`. For date-range reports such as `income-statement`, use `from` and `to`.
- Use `txn_reverse` (`bk txn reverse`) to correct a mistaken posting. Use `txn_link` (`bk txn link`) to record a relationship within one book. Use `correlate` when creating an intercompany posting.
- `txn_reconcile` checks for orphaned intercompany links without changing data. `verify` checks accounting and audit integrity. `audit_log` accepts `actor` for the exact recorded label, `agent` for the verified URI, and `action: "create"` for postings. Audit rows include `prev_hash` and `row_hash`. MCP uses a sanitized `mcp:<client>` label when neither a configured label nor a verified agent is available, with `mcp:unknown` as the final fallback. The verified URI and token ID are recorded even when a configured label supplies the actor.

Successful results contain the CLI JSON **data payload**, without its envelope.
Read JSON text content, or `structuredContent` when present. The SDK uses JSON text
for array payloads on protocol versions that require structured content to be an
object. Monetary fields include decimal-string `_display` companions alongside
integer minor units. Read pagination from the tool result’s `_meta.pagination`
for `txn_list` and `audit_log`; both accept `limit` and `offset`. Errors set `isError: true`
and return `{code, message}` with the CLI's existing error codes. Audited mutations
record `mcp:<sanitized client name>`; current creation-auditing behavior is unchanged.

The [MCP reference](references/commands.md#intentional-cli-only-mutations) lists
mutations that require the CLI, including the final confirmation for company
deletion. See the [client configuration examples](../../README.md#connect-an-ai-assistant-through-mcp).

### Try it with demo books

For a CLI walkthrough, use `bk init --demo --path demo.db` at a fresh path and
target that file with `--db demo.db`. Add `--encrypt` to set a passphrase.
Generated postings and audit activity are credited to `demo`. Company creation
and a single `demo_seed` event retain the initializer's actor. Generated
postings carry the reserved `beankeeper_source` metadata key with value `demo`.
Use `bk audit log --action demo_seed` or MCP `audit_log` with action `demo_seed`
to see who initialized the sample books. [Demo report and audit notes](../reports.md)

To tailor the books, ask about the owner's country, broad work situation, book
types, account types, and currencies. Send only those choices as a JSON profile.
Use `bk init --demo --path tailored.db --profile -` with the profile on stdin,
or pass the same fields to `demo_ledger_create` when the MCP host exposes it.
Write to a fresh local database. Confirm creation in plain words, then use the
same reports on the demo target if the owner asks to explore it. The generator
uses fictional names and amounts and keeps the interview in memory.
[Profile example and defaults](../../README.md#tailor-the-demo-to-your-situation)

Desktop hosts can provision a separate demo ledger with
`beankeeper_cli::commands::init::create_demo_ledger_from_profile` or serve
`beankeeper_cli::mcp::DemoLedgerTool`. Configure the generated ledger's access and
policy before serving it. [Host configuration](../mcp-host-tools.md#tailored-demo-books)

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
| `bk setup begin/finish/revoke` | Manage an encrypted ledger's setup session with an owner `ledger/admin` credential |
| `bk setup book-create/book-initialize/chart-apply/account-add` | Configure books and accounts through the bookkeeper connection or an active setup session |
| `bk setup opening-post/statement-import` | Submit opening balances or statement text. Direct posts return an undo receipt. Propose-only returns proposal IDs. |
| `bk setup book-update` | Record a new profile revision in an active setup session |
| `bk period close/reopen/list` | Close company posting dates, explicitly reopen with a reason, and inspect cutoffs |
| `bk company create/list/show/delete` | Manage companies |
| `bk account create/list/show/rename/classify/delete` | Manage chart of accounts |
| `bk fx post/revalue` | Record native movements and audited carrying-value adjustments |
| `bk txn post` | Record a balanced journal entry |
| `bk txn meta set/unset` | Annotate posted transactions by ID or reference without changing financial fields |
| `bk txn propose` | Submit a balanced posting for approval without posting it |
| `bk txn proposals` | List visible pending requests, optionally including resolved proposals |
| `bk txn approve/reject` | Resolve a pending proposal; `withdraw` aliases rejection of an owned request |
| `bk txn list` | Search and filter transactions |
| `bk txn link SOURCE TARGET --kind KIND` | Record a same-book relationship and return a receipt for one-step undo |
| `bk txn settle-hold` | Replace a card authorization with the actual posted charge and retain its history. |
| `bk txn reverse ID` | Apply or propose a linked mirror transaction (alias `void`). Reconciled originals cannot be reversed. |
| `bk txn recategorize` | Move one posted line between accounts and return a receipt for one-step undo. |
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
| `bk rule amend` | Supersede an active rule and retain both versions in the audit trail |
| `bk rule match` | Preview which active rule matches a candidate description |
| `bk rule undo` | Deactivate a newly created rule and return its linked undo receipt |
| `bk bookkeeping-policy show/set` | Read or save sales-tax and processor-fee instructions |
| `bk inform-preference show/set` | Read or save how often the owner hears about completed changes |
| `bk rule delete` | Remove a mapping without changing existing postings |
| `bk txn clear` | Clear entries (in full or in part) against a statement, by id, by reference, or in an atomic batch; mark reconciled |
| `bk txn attach` | Link documents (receipts, invoices) to transactions |
| `bk txn reconcile` | Find orphaned intercompany correlations |
| `bk budget set` | Create or update monthly/annual budgets |
| `bk budget list` | List budget entries |
| `bk budget delete` | Remove budget entries |
| `bk summary` | Answer how finances are doing with facts, chartable comparisons and optional Desktop targets |
| `bk setup status` | Read ledger readiness before configuring a new ledger or continuing onboarding |
| `bk setup book-show` | Read a book's saved profile, currency, fiscal year, and accounts |
| `bk setup opening-preview` | Check the proposed opening journal and its totals in an active setup generation |
| `bk setup opening-reserve` | Hold the exact reviewed preview for owner approval and return its proposal ID |
| `bk setup statement-preview` | Freeze a bounded CSV, OFX, or QFX statement for owner review |
| `bk setup statement-commit` | Post the exact reviewed batch with the owner's approval credential |
| `bk setup statement-receipt` | Read provenance, created transaction IDs, and preexisting matches |
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
| `bk reconcile statement/list` | Match and lock monthly statements, get an undo receipt, and list account months that still need statements |
| `bk report reconciliation` | Bank balances by default; `--mode settlement --statement ID` compares AR/AP charge applications with statement net, listing short pays, overpays and linked/unmatched credits. Supports metadata filters/grouping |
| `bk report cash-flow` | Cash flow statement (direct method) for a period |
| `bk report trend` | Show revenue, spending or net worth across periods |
| `bk audit log` | Inspect the audit trail of ledger mutations/deletions |
| `bk audit receipts` | Read a company's grouped changes from a cursor |
| `bk audit undo RECEIPT` | Apply or propose undo of an eligible receipt with a signed credential |
| `bk verify` | Check ledger integrity |
| `bk export` | Export accounting data as JSON or CSV |
| `bk backup --output PATH` | Save and verify a full database snapshot at its existing schema version and encryption |
| `bk restore --db PATH --input BACKUP` | Restore the selected ledger's books and return an undo ID |
| `bk restore-undo UNDO_ID --db PATH` | Undo the latest restore before subsequent audited bookkeeping changes |
| `bk recover --db NEW_PATH --input BACKUP` | Recover a verified full snapshot into a new ledger file |

For the full command reference with all flags and examples, see [`references/commands.md`](references/commands.md).

## Essential Workflows

### Back up and restore a ledger

Standalone `bk` and MCP startup save a verified pre-upgrade copy of an older
ledger at `.bk-pre-migration-*/books.bk-backup` beside that ledger. Read
`meta.ledger_upgrade` in CLI JSON or `_meta["beankeeper/ledgerUpgrade"]` in
successful MCP ledger tool results. Use `from` and `to` to tell the owner which
schema changed and `backup` to tell them where the verified copy was saved.
MCP repeats the same notice for the upgraded ledger while the server is
running. Report it once.

Use `bk backup --output PATH` to save a verified full snapshot. For an existing
ledger, use `bk --db PATH restore --input BACKUP` or MCP `restore` with a server
local `input` path. Use a signed `ledger/admin` credential if the selected
live ledger enforces credentials. The command updates its bookkeeping rows, preserves live
credentials and revocations, and records the change in its audit history.

Tell the owner what was restored and offer one-step undo with the returned
`undo_id`. Use `bk --db PATH restore-undo UNDO_ID` or MCP `restore_undo` with
that ID before any later audited bookkeeping changes. Keep the undo snapshot
files beside the ledger. Use the response's verification data and Desktop review
link if the owner wants details.

Restore accepts standalone backups that match the ledger's audit history.
Older backups upgrade in a private copy. A newer backup or a backup with SQLite
sidecars is rejected. Use `bk recover` at a new path to recover a full
current-schema snapshot, including its identity and audit history. Preserve the
passphrase and the external `attachments/` directory separately.
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
`reverses` on the reversal. They appear in `txn show` and
`txn list --include-reversed`. Original
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
bk --company acme rule match --description 'Starbucks 102' --json
bk --company acme rule amend "$RULE_ID" --contains Starbucks --account 5100 --priority 10
bk --company acme rule undo "$CREATED_RULE_ID"
bk --company acme rule delete "$RULE_ID" --force
```

Rules automatically apply to future CSV and OFX/QFX imports, including dry runs.
Patterns are trimmed and matched as literal substrings after Unicode lowercase
conversion. CSV matches its description; OFX matches combined name/memo.
Higher priority wins (default `0`), then ascending rule ID. Rules are scoped to
one company, require an existing destination account, and replace only the
suspense side. They apply to both inflows and outflows; unmatched rows retain
`--suspense`. Account tax defaults apply. A rule targeting the import bank is
an error. JSON journal imports retain their explicit entries.

Use the current rule ID to amend it with complete replacement terms. An
amendment creates a new ID and saves both versions under `rule_supersede`.
To undo an amendment, amend the replacement using the earlier terms from the
audit trail. To undo a creation, pass the created rule ID to `rule undo` or
MCP `rule_undo` with a signed `ledger/admin` credential. Undo takes effect
immediately and appends a `rule_deactivate` event. Tell the owner that the rule
will no longer categorize new imports and give them the undo receipt ID.
When Desktop's `changes_since` returns that receipt and a `desktop_url`, offer
the optional detail link by appending `#receipt-<undo_receipt>` to the URL.
Existing transactions and generated import references stay intact. A retained
deactivated rule still references its original account.

Use `rule_create`, `rule_list`, `rule_amend`, `rule_undo`, `rule_match` and
`rule_delete` when calling MCP. Results match the CLI JSON data. Use
`rule_match` for a read-only `match` or `no_match` result before posting.

### Save standing instructions and the inform preference

Read current settings with `bookkeeping_policy_show` and `inform_preference_show`
or with the corresponding CLI `show` commands. Save the owner's stated
preferences directly. Use `bookkeeping_policy_set` with
`{"company":"acme","policy":{"kind":"sales-tax","value":"track-liability"}}`
or `inform_preference_set` with
`{"company":"acme","preference":"daily-digest"}`.

```bash
bk --company acme bookkeeping-policy set sales-tax=track-liability
bk --company acme bookkeeping-policy set processor-fees=separate-expense
bk --company acme inform-preference set daily-digest
```

Tell the owner what changed and offer to restore the previous setting with one
audited set. Use `unset` for an unconfigured bookkeeping instruction.
`per-change` is the default inform preference. The preference guides when to
report completed changes. Use the [accepted values and payloads](references/commands.md#standing-bookkeeping-and-inform-preferences)
when translating the owner's answer to a setting.

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

Reports total one currency. A configured book defaults to the functional currency
in its profile. Legacy books without a profile infer the currency from postings
and require `--currency CODE` when several currencies are present.

### Foreign-currency balances

- Use `account_create` with `native_currency` (`bk account create --native-currency CODE`) for a foreign-currency asset or liability in a book with a functional currency.
- Record movement with `fx_post` (`bk fx post`). Supply the foreign `account`, functional `offset_account`, `direction`, positive integer `native_minor`, decimal-string `rate`, `date`, `description`, and unique `reference`. The rate expresses functional major units per native major unit. A movement must round to a positive functional minor-unit magnitude.
- Use `fx_revalue` (`bk fx revalue`) with `as_of`, a revenue or expense `fx_gain_loss_account`, and optionally `account` and `rate`. Omit `account` for all active foreign accounts. Omit `rate` to reuse the latest rate recorded for each pair as of that date. A supplied rate applies to a single currency pair.
- Tell the owner what changed in plain words. Include the functional gain or loss, original native balance, and returned Desktop link when helpful. Read the account's `fx` object in `account_list`, or `fx_balances` in a balance sheet or `financial_summary`, for exact amounts and the last active revaluation.
- Use the returned `undo_receipt` with `audit_undo` to reverse a posted adjustment in one step. A no-change revaluation returns no receipt. A closed period or reconciled entry can block a reversal. Use a new rate when `FX_RATE_MISSING` is returned.

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

After marking the statement's entries as cleared, use
`bk reconcile statement --account CODE --period-end YYYY-MM-DD --statement-ending-balance AMOUNT`
or the `reconcile_statement` tool. The ending balance is in decimal major
units and `period_end` is the last calendar day of the month. For MCP, pass
`company`, `account`, `period_end` and `statement_ending_balance`. Specify
`currency` for a company with multiple currencies. Optional `statement_ref`
limits the clearings to that reference. Use `entry_ids` or `references` to
limit the selection further. The CLI equivalents are repeatable `--entry-id`
or `--reference`.

The workflow adds the selected signed clearings dated in the statement month
to the prior completed statement balance. The first run uses net reconciled
clearings dated before the month as the opening. If a match succeeds, tell the
owner the ending balance and receipt. Offer the Desktop link if they want to
inspect the activity. Use `audit_undo` or `bk audit undo RECEIPT` to unlock a
completed statement. The `close_period` option also locks posting for the
company and requires `ledger/admin` both to set and to restore its cutoff.

On `STATEMENT_DISCREPANCY`, use the expected and actual ending balances and
the exact delta in `details` to help locate unmatched activity. Beankeeper
preserves the original clearings. Use `reconcile_list` or `bk reconcile list`
for completed receipts and outstanding months. Summary exposes those gaps
as `needs_attention.items.unreconciled_months`.

Use the balance report to inspect clearings before completing the statement.

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

For statement amounts, use `-95.40` for a withdrawal (credit) and `+95.40`
for a deposit (debit) on the selected account. An unsigned amount clears
either side. The same signs work with `--amount`, batch CSV, and MCP.

Clearing output uses currency units and describes partial clears as `partially cleared`.
JSON retains existing status values and adds `partially_cleared` and each item’s
`currency`. Mixed-currency batch totals have a null currency and display companion.

`--to` is point-in-time: clearings count as cleared when cleared on or before it,
and as reconciled only when also reconciled on or before it (`--status reconciled
--date D` records reconciled date `D`). Over-clearing, a `--date` before the
transaction date, and a reconciled date before the cleared date are `VALIDATION`
errors. Undo a completed monthly statement by receipt before editing its
clearings. Individual clearings outside completed statements can still be
reset with `--status uncleared`. Outstanding items carry `reference`,
`metadata`, `cleared_amount`, `reconciled_amount`, `remaining_amount` and
`clearings`; `bk txn show` lists every entry's clearings. See the statement-reconciliation recipe
in [`references/workflows.md`](references/workflows.md).

`summary` and `report cash-flow` use persisted `cash` / `cash_equivalent` classifications, independent of names. Classify an asset with `account classify CODE cash` or `account_classify` (`company`, `code`, `cash_class`); new custom accounts default to `non_cash`. Inform the owner of the completed change and offer its audit receipt for one-step undo with `audit undo RECEIPT` / `audit_undo`. Existing books preserve their previous cash selection once during migration. `--cash-account CODE` (repeatable) replaces the default selection; see [`references/commands.md`](references/commands.md#bk-report-cash-flow).

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

### Draw a chart in the conversation

Request `--json --chart` on a supported summary/report, or pass `chart: true` to
MCP `financial_summary` / `report`. Use `data.chart` from CLI and summary MCP
responses, or `chart` from MCP report results. Answer in plain words first;
draw the chart from the returned decimal strings when useful. Offer Desktop
as an optional detail view using `chart.desktop_view` with `ui_get_link`.

Supported outputs: summary comparisons, spending trends and categories
(`report income-statement`), net-worth history (`report trend --metric net-worth`),
cash flow, and period/comparison income statements and trial balances.
`--chart-all` / `chart_all: true` requires a chart request and returns all
categories/series; the default is eight plus a signed Other aggregate.

Version 1 contains `schema_version: 1`, `kind` (`line` or `bar`), `title`, ordered
`x` labels, `x_accounts`, and named `series` with `values`, `unit: "major"`,
`currency`, a semantic `role`, and optional `account`. Every values array aligns with x. Values
are exact signed decimal strings at the currency's scale, including zeros and
refunds. Preserve account codes when display labels repeat. `truncated` and
`omitted_count` describe Other; the full source report remains available.

Use `role` rather than labels or signs to distinguish `asset`, `liability`,
`net_worth`, `equity`, `income`, `expense`, `leftover`, `cash_flow`, and `mixed`.
Assets can include non-cash holdings; cash flow is signed movement, not a balance.
Negative net worth remains net worth and must never be shown as positive cash.
Other retains a shared role or uses `mixed` when its contributors differ. Choose
your own palette and render unknown future roles neutrally. This is an additive
version 1 field; older consumers can ignore it.

Worked example: request
`{"company":"acme","kind":"trend","metric":"spending","from":"2026-01-01","to":"2026-02-28","chart":true}`.
For a chart with `x: ["2026-01","2026-02"]` and
`series: [{"name":"Dining","role":"expense","account":"5000","values":["120.50","-15.00"],"unit":"major","currency":"USD"}]`,
draw a line labeled Dining with those two USD values. Explain that February's
refunds exceeded expenses by USD 15.00. Do not sum Total Spending together with
its categories or sum net-worth snapshots across dates. Do not replace an
account code with its label or put that label in Desktop's tax-tag `category`.
See the [full schema and link example](../chart-data.md).

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
| `BK_ACTOR` | Poster and audit actor label, overridden by `--actor` | Verified agent URI, else the CLI or MCP fallback |
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

Supply real `YYYY-MM-DD` dates for report, summary and posting inputs in CLI
and MCP. When the date defaults to today, Beankeeper uses the local date. An
invalid date or a blank audit actor filter returns `VALIDATION`.

Validate before posting: ensure account codes exist (`bk account list`), amounts are positive, and total debits equal total credits. The CLI enforces balance at post time and will reject unbalanced entries.

## Reference Files

For detailed information beyond this overview:

- **[`references/accounting.md`](references/accounting.md)** -- Account types, normal balances, the accounting equation, and double-entry rules
- **[`references/commands.md`](references/commands.md)** -- Complete command reference with all flags, arguments, and examples
- **[`references/json-api.md`](references/json-api.md)** -- JSON envelope format, field types, and programmatic usage patterns
- **[`references/workflows.md`](references/workflows.md)** -- Multi-step workflow recipes for common bookkeeping tasks

## Answering what changed

For “why is spending up?” use `report trend --metric spending --compare previous`.
For “how does this quarter compare to last year?” supply the quarter's inclusive
`--from` and `--to` with `--compare year-ago` on `report income-statement`.
For “what changed in rent?” add `--explain-change <expense-account-code>`.
The MCP `report` tool accepts the same `compare` and `explain_change` fields for
income-statement, trial-balance, trend, cash-flow, balance and balance-sheet.

Lead with `summary`, then use `movers`, `accounts` and `totals` for follow-ups or
charts. Each line includes current/prior/change minor-unit integers, decimal
displays, and a nullable `change_percent`. A zero prior amount means no meaningful
percent; never describe null as zero or infinity. Explain transaction and payee
contributions as recorded differences, not proof of a real-world cause.

Offer the optional `desktop_view.url` and `comparison_url` register links after
the answer. Resolve these origin-relative URLs against the owner's configured
Desktop origin; do not invent a hostname or require Desktop to answer. The
`view_id`, `state` and `comparison_state` also fit Desktop navigation tools.
Links select the explained account, otherwise the balance account or largest
mover. [Comparison semantics and examples](../comparisons.md) document date
windows, cash attribution, currency isolation and CSV rows.
