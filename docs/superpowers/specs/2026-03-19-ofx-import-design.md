# OFX Import Design

## Problem

The `bk txn import` command exists but is not implemented. Users need to import bank and credit card statements in OFX format into beankeeper's double-entry ledger. OFX (Open Financial Exchange) is the standard format exported by most US financial institutions.

## Decision Summary

- **Format**: OFX only (CSV/JSON import deferred to a later effort)
- **Double-entry**: Each OFX transaction maps to a balanced journal entry using a user-specified bank account and suspense/clearing account
- **Deduplication**: OFX `fit_id` maps to beankeeper's existing `reference` field for idempotent re-imports
- **Dependency**: `ofx-rs` crate from crates.io

## CLI Interface

```
bk txn import --file statement.ofx --format ofx --account 1000 --suspense 9000
```

### New/Modified Flags

| Flag | Required | Description |
|------|----------|-------------|
| `--file <PATH>` | No | Input file path. Use `-` for stdin. Defaults to stdin. |
| `--format <FMT>` | No | `ofx`, `csv`, or `json`. Auto-detected from file extension (`.ofx`, `.qfx` -> ofx). |
| `--account <CODE>` | For OFX | The beankeeper account code representing the bank account (e.g., `1000`). |
| `--suspense <CODE>` | For OFX | The contra/clearing account for the other leg of each entry (e.g., `9000`). |
| `--dry-run` | No | Validate and report what would be imported without posting. |

### CLI Struct Changes

The existing `TxnCommand::Import` variant in `cli.rs` must be extended with two new optional fields:
- `--account <CODE>` — required when format is OFX, ignored otherwise
- `--suspense <CODE>` — required when format is OFX, ignored otherwise

These are validated at runtime (not by clap) so CSV/JSON import can be added later without requiring them.

The `--company` global flag (or `BEANKEEPER_COMPANY` env var) is required as with all transactional commands.

### ImportFormat Enum Change

Add `Ofx` variant:

```rust
pub enum ImportFormat {
    Csv,
    Json,
    Ofx,
}
```

### Format Auto-Detection

When `--format` is omitted and `--file` is provided:
- `.ofx` or `.qfx` extension -> `Ofx`
- `.csv` extension -> `Csv`
- `.json` extension -> `Json`
- Otherwise -> error asking user to specify `--format`

## Mapping: OFX Transaction -> Beankeeper Journal Entry

Each `StatementTransaction` from the OFX file produces one balanced transaction:

| OFX Field | Beankeeper Field | Transformation |
|-----------|-----------------|----------------|
| `date_posted` | `date` | `OfxDateTime::as_offset_date_time()` -> format as `YYYY-MM-DD` via `time` crate |
| `name` + `memo` | `description` | Join with ` - ` if both present; use whichever is available |
| `amount` (positive, inflow) | debit `--account`, credit `--suspense` | Absolute value, converted to minor units |
| `amount` (negative, outflow) | debit `--suspense`, credit `--account` | Absolute value, converted to minor units |
| `fit_id` + account context | `reference` | Formatted as `ofx:<account_id>:<fit_id>` to prevent cross-bank collisions |
| `transaction_type` | `metadata` | JSON object: `{"ofx_type": "CHECK"}` |
| statement `currency_default` | `currency` | `CurrencyCode::as_str()`, validated against beankeeper's currency table |

### Amount Conversion

OFX amounts are `rust_decimal::Decimal` (via `OfxAmount::as_decimal()`). Beankeeper stores amounts in minor units (`i64`). Conversion:

1. Take absolute value of the OFX amount (sign determines debit/credit direction)
2. Zero amounts are skipped with a warning (cannot form a valid double-entry)
3. Multiply by 10^(currency minor units) — e.g., `* 100` for USD
4. Verify result has no fractional part (error if it does — indicates bad OFX data)
5. Convert to `i64` via `Decimal::to_i64()` (safe after step 4)

The currency's minor unit exponent comes from `beankeeper::types::Currency::from_code()`. If the OFX statement's `currency_default` is not recognized by beankeeper's currency table, abort with an error before importing.

### Description Assembly

```
name: "GROCERY STORE", memo: "Weekly groceries"  -> "GROCERY STORE - Weekly groceries"
name: "GROCERY STORE", memo: None                 -> "GROCERY STORE"
name: None,            memo: "Weekly groceries"   -> "Weekly groceries"
name: None,            memo: None                 -> "OFX transaction <fit_id>"
```

## Handling Multiple Statements

An OFX file can contain multiple statement responses (banking + credit card). The import processes all of them, using the same `--account` and `--suspense` for all transactions. If the user has a file with multiple accounts, they should split it or import once per account.

Both `banking.statement_responses()` and `credit_card.statement_responses()` are iterated. Each response is wrapped in `TransactionWrapper` — call `.response()` to get `Option<&StatementResponse>` and skip wrappers where the response is `None` (failed server responses).

## Duplicate Handling

- Each OFX `fit_id` is stored as `reference = "ofx:<account_id>:<fit_id>"` on the posted transaction (account_id from the OFX statement's bank/CC account, preventing collisions across different bank accounts)
- The raw reference string is passed directly to `PostTransactionParams.reference` (not through `IdempotencyKey`)
- Beankeeper's existing idempotency check rejects duplicate references per company
- On duplicate: skip the transaction, increment a `skipped` counter, continue processing
- This makes re-importing the same file safe and idempotent

## Dry-Run Mode

When `--dry-run` is set:
- Parse the OFX file
- Validate account codes exist
- Report each transaction that would be imported (date, description, amount, direction)
- Report duplicates that would be skipped
- Do not post anything

## Output

### Table Mode (default)

```
Imported 47 transactions, skipped 3 duplicates.
```

With `--verbose`:

```
  [imported] 2025-01-15  GROCERY STORE - Weekly groceries     -$50.00
  [imported] 2025-01-16  DIRECT DEPOSIT                    $3,200.00
  [skipped]  2025-01-17  GAS STATION                         -$45.00  (duplicate: ofx:20250117002)
  ...
Imported 47 transactions, skipped 3 duplicates.
```

### JSON Mode

```json
{
  "ok": true,
  "meta": { ... },
  "data": {
    "imported": 47,
    "skipped": 3,
    "transactions": [
      { "id": 101, "date": "2025-01-15", "description": "GROCERY STORE", "amount": -5000, "status": "imported" },
      { "id": null, "date": "2025-01-17", "description": "GAS STATION", "amount": -4500, "status": "skipped", "reason": "duplicate" }
    ]
  }
}
```

## Error Handling

| Error | Behavior |
|-------|----------|
| OFX parse failure | Abort with error message from `OfxError` |
| `--account` code doesn't exist | Abort before importing |
| `--suspense` code doesn't exist | Abort before importing |
| Individual transaction fails to post (non-duplicate) | Report error, continue with remaining transactions |
| All transactions are duplicates | Success with "0 imported, N skipped" |
| Empty OFX file (no transactions) | Success with "0 imported, 0 skipped" |

## Module Structure

New file: `beankeeper-cli/src/commands/import_ofx.rs`

Contains:
- `run_import_ofx()` — orchestrator function called from `txn.rs`
- `build_description()` — assembles description from name/memo
- `ofx_amount_to_minor()` — converts `Decimal` to `i64` minor units
- `format_ofx_date()` — converts `OfxDateTime` to `YYYY-MM-DD` string (via `time::OffsetDateTime` formatting)

The existing `txn.rs` dispatches to this module when format is OFX.

## Dependencies

- `ofx-rs` (from crates.io) — OFX parsing
- `rust_decimal` — already a transitive dep via ofx-rs, but add directly for amount conversion
- `time` — already a transitive dep via ofx-rs, needed for date formatting

## Verification

1. `cargo check -p beankeeper-cli` — compiles
2. `cargo nextest run -p beankeeper-cli` — all tests pass
3. Unit tests for:
   - `build_description()` with various name/memo combinations
   - `ofx_amount_to_minor()` with positive/negative/zero amounts and different currencies
   - `format_ofx_date()` with various OFX datetime formats
4. Integration test: import a sample OFX file with `--demo` database, verify transactions posted correctly
5. Manual test: `bk init --demo && bk txn import --file test.ofx --format ofx --account 1000 --suspense 9000 --company acme-consulting`
