# JSON Response Envelope for bk CLI

## Problem

bk's JSON output works but is implicit. Each command returns a different raw shape — bare arrays or objects — with no consistent contract. For an agent (or any programmatic consumer) to use bk reliably, every command's JSON output should follow a consistent, documented envelope.

## Design

### Envelope Structure

All JSON responses are wrapped in a uniform envelope:

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

### Rust Types

Three new structs in `output/json.rs`:

```rust
#[derive(Serialize)]
pub struct Meta {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    pub timestamp: String, // ISO 8601 UTC
}

#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub ok: bool,
    pub meta: Meta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EnvelopeError>,
}

#[derive(Serialize)]
pub struct EnvelopeError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}
```

A helper constructs `Meta`:

```rust
pub fn meta(command: &str, company: Option<&str>) -> Meta {
    Meta {
        command: command.to_string(),
        company: company.map(|s| s.to_string()),
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    }
}
```

### Command Naming Convention

The `meta.command` field uses `resource.action` dot notation derived from the CLI hierarchy:

| CLI invocation | `meta.command` |
|---|---|
| `bk init` | `init` |
| `bk verify` | `verify` |
| `bk export` | `export` |
| `bk company create` | `company.create` |
| `bk company list` | `company.list` |
| `bk company show` | `company.show` |
| `bk company delete` | `company.delete` |
| `bk account create` | `account.create` |
| `bk account list` | `account.list` |
| `bk account show` | `account.show` |
| `bk account delete` | `account.delete` |
| `bk txn post` | `txn.post` |
| `bk txn list` | `txn.list` |
| `bk txn show` | `txn.show` |
| `bk txn import` | `txn.import` |
| `bk txn attach` | `txn.attach` |
| `bk txn reconcile` | `txn.reconcile` |
| `bk report trial-balance` | `report.trial-balance` |
| `bk report balance` | `report.balance` |
| `bk report income-statement` | `report.income-statement` |
| `bk report balance-sheet` | `report.balance-sheet` |
| `bk report tax-summary` | `report.tax-summary` |

Each command handler passes its command string as a `&str` literal.

### Meta Fields

- `command` (string, always present): dot-notation command name
- `company` (string, optional): company slug when the command operates on a company; omitted otherwise via `skip_serializing_if`
- `timestamp` (string, always present): ISO 8601 UTC timestamp of response generation

### Error Codes

String codes derived from `CliError` variant names:

| CliError variant | `error.code` |
|---|---|
| `Usage` | `"USAGE"` |
| `Validation`, `Bean` | `"VALIDATION"` |
| `Database`, `Sqlite` | `"DATABASE"` |
| `NotFound` | `"NOT_FOUND"` |
| `General`, `Io` | `"GENERAL"` |

Implemented as a method on `CliError`:

```rust
pub fn error_code(&self) -> &str {
    match self {
        CliError::Usage(_) => "USAGE",
        CliError::Validation(_) | CliError::Bean(_) => "VALIDATION",
        CliError::Database(_) | CliError::Sqlite(_) => "DATABASE",
        CliError::NotFound(_) => "NOT_FOUND",
        CliError::General(_) | CliError::Io(_) => "GENERAL",
    }
}
```

### Integration Pattern

1. **`render_*` functions** gain a `meta: Meta` parameter and wrap results in `Envelope { ok: true, meta, data: Some(...), error: None }`.

2. **Command handlers** construct meta and pass it:
   ```rust
   let meta = output::json::meta("company.list", None);
   let rendered = output::json::render_companies(&rows, meta)?;
   println!("{rendered}");
   ```

3. **`CliError::report()`** gains a `meta: Option<Meta>` parameter. In JSON mode with meta available, it wraps the error in the envelope using `error_code()` for the code string.

4. **Table and CSV output are unchanged.** The envelope only applies when `OutputFormat::Json` is active.

### Data Field

The `data` field contains whatever the command naturally returns — arrays for list commands, objects for single-item commands. No normalization (e.g., no forced `{"items": [], "count": N}` wrapping).

## Scope

### In scope
- `Envelope<T>`, `Meta`, `EnvelopeError` structs in `output/json.rs`
- All `render_*` functions updated to accept `Meta` and return enveloped JSON
- `CliError::report()` updated to produce enveloped errors in JSON mode
- `error_code()` method on `CliError`
- `chrono` dependency for UTC timestamps (if not already present)
- All existing tests updated to expect envelope shape

### Not in scope
- Changes to table or CSV output
- Schema versioning
- Auto-generated skill definitions (future work)
- New commands or features
- Changes to the `beankeeper` library crate (purely CLI-layer)

### Backwards compatibility

Breaking change to JSON output. Since bk is not yet released, no compatibility shims are needed.
