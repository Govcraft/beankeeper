# JSON Envelope Implementation Plan

## Summary

Wrap all JSON output from the `bk` CLI in a uniform `{ ok, meta, data/error }` envelope. This is a breaking change to JSON output. Table and CSV output are unaffected. `bk export` is excluded.

## Files to Modify (in dependency order)

### 1. `beankeeper-cli/src/output/json.rs` -- Envelope types and render functions

**Add new types:**

```
Meta { command: String, company: Option<String>, timestamp: String }
Envelope<T: Serialize> { ok: bool, meta: Meta, data: Option<T>, error: Option<EnvelopeError> }
EnvelopeError { code: String, message: String }
```

All three derive `Serialize`. `Meta.company` and `Envelope.data`/`Envelope.error` use `#[serde(skip_serializing_if = "Option::is_none")]`.

**Add helper constructors:**

- `pub fn meta(command: &str, company: Option<&str>) -> Meta` -- uses `chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)` for timestamp.
- `pub fn meta_with_timestamp(command: &str, company: Option<&str>, timestamp: String) -> Meta` -- for deterministic tests.

**Modify every `render_*` function signature** to accept `meta: Meta` as a new parameter. Each function wraps its current return payload in `Envelope { ok: true, meta, data: Some(payload), error: None }` and serialises the envelope instead of the raw payload.

Functions and their new signatures:

| Current signature | New signature |
|---|---|
| `render_companies(companies: &[CompanyRow]) -> Result<String, CliError>` | `render_companies(companies: &[CompanyRow], meta: Meta) -> Result<String, CliError>` |
| `render_accounts(accounts: &[AccountRow]) -> Result<String, CliError>` | `render_accounts(accounts: &[AccountRow], meta: Meta) -> Result<String, CliError>` |
| `render_accounts_with_balances(rows: &[AccountWithBalanceRow]) -> Result<String, CliError>` | `render_accounts_with_balances(rows: &[AccountWithBalanceRow], meta: Meta) -> Result<String, CliError>` |
| `render_transactions(txns, entries_map) -> Result<String, CliError>` | `render_transactions(txns, entries_map, meta: Meta) -> Result<String, CliError>` |
| `render_transactions_with_attachments(txns, entries_map, att_map) -> Result<String, CliError>` | `render_transactions_with_attachments(txns, entries_map, att_map, meta: Meta) -> Result<String, CliError>` |
| `render_trial_balance(balances: &[BalanceRow]) -> Result<String, CliError>` | `render_trial_balance(balances: &[BalanceRow], meta: Meta) -> Result<String, CliError>` |
| `render_account_balance(balance, currency) -> Result<String, CliError>` | `render_account_balance(balance, currency, meta: Meta) -> Result<String, CliError>` |
| `render_orphaned_correlations(orphans) -> Result<String, CliError>` | `render_orphaned_correlations(orphans, meta: Meta) -> Result<String, CliError>` |
| `render_tax_summary(rows) -> Result<String, CliError>` | `render_tax_summary(rows, meta: Meta) -> Result<String, CliError>` |

**Rename `error_code_string` to `error_code` and make it a method on `CliError`** (or keep it as a free function in json.rs -- see decision below). Update the error code strings to match the spec: `"USAGE"`, `"VALIDATION"`, `"DATABASE"`, `"NOT_FOUND"`, `"IO"`, `"GENERAL"` (dropping the `_ERROR` suffix from the current values).

**Remove `render_error`**, `ErrorJson`, and `ErrorDetailJson`. Their responsibility moves to `CliError::report()`.

### 2. `beankeeper-cli/src/error.rs` -- Enveloped error reporting

**Add `error_code()` method** to `CliError`:

```rust
pub fn error_code(&self) -> &'static str {
    match self {
        Self::Usage(_) => "USAGE",
        Self::Validation(_) | Self::Bean(_) => "VALIDATION",
        Self::Database(_) | Self::Sqlite(_) => "DATABASE",
        Self::NotFound(_) => "NOT_FOUND",
        Self::Io(_) => "IO",
        Self::General(_) => "GENERAL",
    }
}
```

**Update `report()` signature** from `pub fn report(&self, json_mode: bool)` to `pub fn report(&self, json_mode: bool, meta: Option<crate::output::json::Meta>)`.

When `json_mode` is true and `meta` is `Some`:
- Construct `Envelope { ok: false, meta, data: None::<()>, error: Some(EnvelopeError { code, message }) }`.
- Serialise with `serde_json::to_string_pretty` and `eprintln!`.
- Fallback: if serialisation fails, hand-craft minimal JSON.

When `json_mode` is true but `meta` is `None` (should not happen in practice but defensive): use the current inline `serde_json::json!` approach.

When `json_mode` is false: unchanged `eprintln!("error: {self}")`.

### 3. `beankeeper-cli/src/commands/mod.rs` -- `command_name()` utility

**Add `command_name()` function** that maps `&Command` to `&'static str`:

```rust
pub fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Init { .. } => "init",
        Command::Verify => "verify",
        Command::Export { .. } => "export",
        Command::Company(sub) => match sub {
            CompanyCommand::Create { .. } => "company.create",
            CompanyCommand::List => "company.list",
            CompanyCommand::Show { .. } => "company.show",
            CompanyCommand::Delete { .. } => "company.delete",
        },
        Command::Account(sub) => match sub {
            AccountCommand::Create { .. } => "account.create",
            AccountCommand::List { .. } => "account.list",
            AccountCommand::Show { .. } => "account.show",
            AccountCommand::Delete { .. } => "account.delete",
        },
        Command::Txn(sub) => match sub.as_ref() {
            TxnCommand::Post { .. } => "txn.post",
            TxnCommand::List { .. } => "txn.list",
            TxnCommand::Show { .. } => "txn.show",
            TxnCommand::Import { .. } => "txn.import",
            TxnCommand::Attach { .. } => "txn.attach",
            TxnCommand::Reconcile => "txn.reconcile",
        },
        Command::Report(sub) => match sub {
            ReportCommand::TrialBalance { .. } => "report.trial-balance",
            ReportCommand::Balance { .. } => "report.balance",
            ReportCommand::IncomeStatement { .. } => "report.income-statement",
            ReportCommand::BalanceSheet { .. } => "report.balance-sheet",
            ReportCommand::TaxSummary { .. } => "report.tax-summary",
        },
    }
}
```

This requires importing `Command`, `CompanyCommand`, `AccountCommand`, `TxnCommand`, `ReportCommand` from `crate::cli`.

### 4. `beankeeper-cli/src/main.rs` -- Enveloped error path

Update the error branch to construct `Meta` and pass it to `report()`:

```rust
Err(e) => {
    let cmd_name = commands::command_name(&cli.command);
    let company = cli.company.as_deref();
    let meta = if json_mode {
        Some(beankeeper_cli::output::json::meta(cmd_name, company))
    } else {
        None
    };
    e.report(json_mode, meta);
    ExitCode::from(e.exit_code())
}
```

### 5. `beankeeper-cli/src/commands/company.rs` -- Pass meta to render functions

Update the local `render_companies` helper:
- Accept the command name (`&str`) and company slug (`Option<&str>`) or pre-built `Meta`.
- In the `OutputFormat::Json` arm, construct `meta` via `output::json::meta(cmd_name, company)` and pass it to `output::json::render_companies(rows, meta)`.
- Table and CSV arms are unchanged.

Call sites in `run()`:
- `CompanyCommand::Create` -> `"company.create"`, company = `None` (not scoped to a company).
- `CompanyCommand::List` -> `"company.list"`, company = `None`.
- `CompanyCommand::Show` -> `"company.show"`, company = `None`.
- `CompanyCommand::Delete` -> does not render JSON currently (only an `eprintln!`). Add a JSON envelope for the `{"deleted": "<slug>"}` confirmation object. This means adding a new code path in the Delete arm for JSON output.

### 6. `beankeeper-cli/src/commands/account.rs` -- Pass meta to render functions

Update the local `render_accounts` and `render_accounts_with_balances` helpers similarly. The company slug is available from the `company` parameter passed to `run()`.

Call sites:
- `AccountCommand::Create` -> `"account.create"`, company = `Some(company)`.
- `AccountCommand::List` -> `"account.list"`, company = `Some(company)`.
- `AccountCommand::Show` -> `"account.show"`, company = `Some(company)`.
- `AccountCommand::Delete` -> add JSON envelope for `{"deleted": "<code>"}`.

### 7. `beankeeper-cli/src/commands/txn.rs` -- Pass meta to render functions

Update `run_list`, `run_show`, `run_count`, `run_reconcile`:
- `run_list` / `run_count`: `"txn.list"`, company = `Some(company)`.
- `run_show`: `"txn.show"`, company = `Some(company)`.
- `run_reconcile`: `"txn.reconcile"`, company = `None` (scans all companies).
- `run_count`: the inline `println!("{{\"count\":{count}}}")` needs to be wrapped in an envelope. Add a small inline envelope construction or a `render_count` helper.
- `run_post`: does not produce JSON output currently (only `eprintln!`). Per the design spec, create commands return the created resource. The `run_post` path does not currently query back the posted transaction. Two options: (a) query it back and render, or (b) construct a minimal JSON response with `{"id": txn_id}`. The spec says "Create commands return the created resource as data", but `txn post` currently only prints `[ok] transaction #N posted` to stderr. For now, render `{"id": txn_id}` as data in the envelope for `txn.post`.
- `run_attach`: similar -- render `{"id": att_id, "transaction_id": transaction_id}` as data.

### 8. `beankeeper-cli/src/commands/report.rs` -- Pass meta to render functions

Update all report rendering call sites:
- `run_trial_balance`: `"report.trial-balance"`, company = `Some(company)`.
- `run_balance`: `"report.balance"`, company = `Some(company)`.
- `run_income_statement`: `"report.income-statement"`, company = `Some(company)`.
- `run_balance_sheet`: `"report.balance-sheet"`, company = `Some(company)`.
- `run_tax_summary`: `"report.tax-summary"`, company = `Some(company)`.

### 9. `beankeeper-cli/src/commands/init.rs` -- Envelope for init output

`init` currently only emits `eprintln!`. For JSON mode, output an envelope with `data: {"path": "<db_path>"}`.

### 10. `beankeeper-cli/src/commands/verify.rs` -- Envelope for verify output

`verify` currently only emits `eprintln!`. For JSON mode, output an envelope with `data: {"schema_version": N, "status": "healthy"}`.

### 11. `beankeeper-cli/src/commands/export.rs` -- No changes

The design spec explicitly excludes `bk export` from envelope wrapping.

## Implementation Order

The changes have the following dependency graph:

```
1. output/json.rs (new types + modified render functions)
   |
   +---> 2. error.rs (error_code method + updated report())
   |        |
   |        +---> 4. main.rs (updated error path)
   |
   +---> 3. commands/mod.rs (command_name utility)
   |        |
   |        +---> 4. main.rs (uses command_name)
   |
   +---> 5-10. All command handler files (pass meta to render calls)
```

**Recommended order:**

1. `output/json.rs` -- Add `Meta`, `Envelope`, `EnvelopeError` types and helper constructors. Add `meta_with_timestamp` for tests. Update all `render_*` signatures and implementations to accept `Meta` and wrap in envelope. Remove `render_error`, `ErrorJson`, `ErrorDetailJson`, and `error_code_string`.
2. `error.rs` -- Add `error_code()` method. Update `report()` to accept `Option<Meta>` and produce enveloped errors.
3. `commands/mod.rs` -- Add `command_name()` function.
4. `main.rs` -- Update error branch.
5. `commands/company.rs` -- Update call sites + add delete JSON output.
6. `commands/account.rs` -- Update call sites + add delete JSON output.
7. `commands/txn.rs` -- Update call sites + add post/attach/count JSON output.
8. `commands/report.rs` -- Update call sites.
9. `commands/init.rs` -- Add JSON output path.
10. `commands/verify.rs` -- Add JSON output path.

After each step, run `cargo check -p beankeeper-cli` to verify compilation. After step 10, run `cargo nextest run -p beankeeper-cli` and `cargo clippy -p beankeeper-cli -- -D warnings`.

## Test Updates

### Unit tests in `output/json.rs`

All existing tests call `render_*` functions without `Meta`. They need to be updated to pass a `Meta` constructed via `meta_with_timestamp` with a fixed timestamp string (e.g., `"2025-01-01T00:00:00Z"`).

Existing assertions check for raw field values like `"slug": "acme"`. These assertions remain valid because the `data` field still contains the same structure -- they just need to dig one level deeper (inside `"data"`). The tests should also assert on the envelope shape:

- `assert!(json.contains(r#""ok": true"#))`
- `assert!(json.contains(r#""command": "company.list""#))`
- `assert!(json.contains(r#""timestamp": "2025-01-01T00:00:00Z""#))`

Tests for `render_error_*` variants should be **removed** since `render_error` is being removed. New tests for `CliError::error_code()` should be added in `error.rs`.

**Specific test changes:**

| Test | Change |
|---|---|
| `render_companies_empty` | Pass `meta_with_timestamp("company.list", None, ...)`. Assert `"ok": true` and `"data": []`. |
| `render_companies_single` | Pass `meta_with_timestamp("company.show", None, ...)`. Existing `contains` assertions still work (data is nested but `contains` searches the whole string). |
| `render_accounts_normal_balance` | Pass `meta_with_timestamp("account.list", Some("acme"), ...)`. |
| `render_accounts_revenue_is_credit` | Same pattern. |
| `render_transactions_with_entries` | Pass `meta_with_timestamp("txn.list", Some("acme"), ...)`. |
| `render_transactions_missing_entries` | Same pattern. |
| `render_trial_balance_balanced` | Pass `meta_with_timestamp("report.trial-balance", Some("acme"), ...)`. |
| `render_trial_balance_unbalanced` | Same pattern. |
| `render_account_balance_json` | Pass `meta_with_timestamp("report.balance", Some("acme"), ...)`. |
| `render_error_*` (5 tests) | **Remove entirely.** |
| `normal_balance_for_all_types` | No change (tests a helper, not a render function). |

**New tests to add:**

| Test | Location | Purpose |
|---|---|---|
| `error_code_returns_correct_strings` | `error.rs` | Verify all 6 variant mappings. |
| `envelope_structure_success` | `json.rs` | Parse rendered JSON and verify `ok`, `meta`, `data` fields structurally (using `serde_json::Value`). |
| `envelope_structure_company_field` | `json.rs` | Verify `meta.company` is present when provided and absent when `None`. |
| `meta_with_timestamp_is_deterministic` | `json.rs` | Verify the timestamp matches exactly. |

### Tests in `error.rs`

No existing tests are affected (they test `exit_code`, `Display`, and `From` impls). Add new `error_code` tests as described above.

### Tests in `cli.rs`, `commands/txn.rs`

No changes needed -- these test parsing and amount conversion, not JSON output.

## Risks and Gotchas

### 1. Circular dependency between `error.rs` and `output/json.rs`

`error.rs` currently does not import from `output/json.rs`. The updated `report()` method will need `Meta`, `Envelope`, `EnvelopeError` from `output/json.rs`. Meanwhile, `output/json.rs` already imports `CliError` from `error.rs`. This is fine in Rust (same crate, no actual circular dependency), but it creates a tighter coupling.

**Mitigation:** The `error_code()` method lives on `CliError` in `error.rs` and does not depend on json types. The `report()` method imports json types only for the JSON branch. This is acceptable.

### 2. The `error_code_string` function in `json.rs` vs `error_code` method on `CliError`

The spec says `error_code()` should be a method on `CliError`. The current `error_code_string` is a free function in `json.rs`. Moving it to `CliError` is cleaner and matches the spec. The json.rs function should be removed entirely.

### 3. Inline JSON in `txn.rs::run_count`

The `run_count` function currently prints `{"count": N}` inline. This needs to become an enveloped response. A small `#[derive(Serialize)] struct CountJson { count: i64 }` should be added to `json.rs` with a `render_count(count, meta)` function, or the envelope can be constructed inline in `run_count`.

**Decision:** Add `render_count` to `json.rs` for consistency.

### 4. Inline JSON in `txn.rs::run_reconcile` (empty case)

When no orphans are found, `run_reconcile` prints `[]` directly. This must become an enveloped `{ ok: true, meta, data: [] }`.

### 5. Commands that currently produce no JSON output

`init`, `verify`, `company delete`, `account delete`, `txn post`, `txn attach` do not currently emit JSON to stdout. The spec says:
- Create commands return the created resource.
- Delete commands return `{"deleted": "<slug>"}`.
- `init` and `verify` are not explicitly specified but should follow the pattern.

For these commands, JSON output should only be emitted when `--json` or `--format json` is active. The command handlers need to check `format == OutputFormat::Json` (or receive the format) and conditionally emit the envelope. However, `init` and `verify` do not currently resolve `OutputFormat` because they have no tabular output. They need to be updated to check `cli.is_json()`.

### 6. `txn post` does not query back the posted transaction

The posted transaction is not returned from `transactions::post_transaction` -- only the `txn_id: i64` is returned. To return the full resource as `data`, we would need an additional DB query. For the initial implementation, return `{"id": txn_id}` as a lightweight confirmation. This can be enhanced later to return the full transaction.

### 7. Timestamp non-determinism in tests

Using `Utc::now()` makes JSON output non-deterministic. The `meta_with_timestamp` constructor solves this for unit tests. For integration/CLI tests (if added later), the timestamp field should be ignored or regex-matched.

### 8. `chrono` is already a dependency

Confirmed: `chrono = { version = "0.4.44", features = ["serde"] }` is already in `beankeeper-cli/Cargo.toml`. The `Utc` type and `SecondsFormat` are available via `chrono::Utc` and `chrono::SecondsFormat`. No dependency changes needed.

### 9. Error code string change is a secondary breaking change

The current error codes use `_ERROR` suffix (e.g., `"USAGE_ERROR"`, `"DATABASE_ERROR"`). The spec drops the suffix (e.g., `"USAGE"`, `"DATABASE"`). Since the entire JSON output format is changing anyway, this is acceptable as part of the same breaking change.

### 10. `render_error` callers

`render_error` is only used within `output/json.rs` tests. It is not called from any command handler or from `main.rs`. The `CliError::report()` method in `error.rs` uses its own inline `serde_json::json!` construction. Removing `render_error` is safe.

## Semver Recommendation

**Minor bump: 0.1.2 -> 0.2.0**

Rationale: This is a breaking change to JSON output format. However, since `beankeeper-cli` is pre-1.0 (`0.1.2`), semver conventions allow breaking changes in minor version bumps. A minor bump from `0.1.2` to `0.2.0` signals the breaking nature of the change while staying within pre-1.0 conventions.
