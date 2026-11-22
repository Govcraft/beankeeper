//! Atomic CSV bank imports and JSON bulk journal posting.
//!
//! Parsing and normalization are independent of storage. Every posting is
//! validated and persisted by the shared transaction writer.
//! Dry runs exercise that same path inside a transaction that is rolled back.

use crate::operation::Operation;
use std::collections::HashMap;

use beankeeper::types::{Currency, DebitOrCredit, Money, PositiveMoney};
use chrono::NaiveDate;
use clap::{Args, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::reference::TransactionReference;
use super::statement_source::SourceRow;
use crate::cli::{Cli, ImportFormat, OnConflictArg, OutputFormat, TxnImportArgs};
use crate::db::connection::Db;
use crate::db::{self, accounts, transactions};
use crate::error::CliError;
use crate::output::json;

/// Options for CSV mapping and the default CSV/JSON transaction currency.
#[derive(Args, Debug)]
pub struct ImportOptions {
    #[command(flatten)]
    mapping: CsvMapping,
    /// Default currency for CSV/JSON (JSON objects may override it).
    #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
    currency: crate::cli::CurrencyArg,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CsvMapping {
    /// CSV header containing the transaction date.
    #[arg(long, default_value = "date")]
    date_col: String,
    /// CSV header containing the description.
    #[arg(long, default_value = "description")]
    description_col: String,
    /// CSV header containing the signed amount in major currency units.
    #[arg(long, default_value = "amount")]
    amount_col: String,
    /// CSV header containing a reference (optional unless explicitly selected).
    #[arg(long)]
    reference_col: Option<String>,
    /// CSV date format using strftime syntax.
    #[arg(long, default_value = "%Y-%m-%d")]
    date_format: String,
    /// Meaning of positive CSV amounts. Negative amounts mean the opposite.
    #[arg(long, value_enum, default_value = "inflow")]
    positive_amount: PositiveAmount,
}

impl Default for CsvMapping {
    fn default() -> Self {
        Self {
            date_col: "date".into(),
            description_col: "description".into(),
            amount_col: "amount".into(),
            reference_col: None,
            date_format: "%Y-%m-%d".into(),
            positive_amount: PositiveAmount::Inflow,
        }
    }
}

/// The direction of positive bank statement amounts.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum PositiveAmount {
    Inflow,
    Outflow,
}

/// Exact major-unit input. JSON fractions must be strings to avoid floating
/// point rounding. Integer JSON numbers, like `txn post` integers, are major units.
#[derive(Deserialize)]
#[serde(untagged)]
enum InputAmount {
    Text(String),
    Integer(i64),
}

impl InputAmount {
    fn minor_units(&self, currency: Currency) -> Result<i64, CliError> {
        match self {
            Self::Text(text) => super::txn::parse_amount_to_minor(text.trim(), currency),
            Self::Integer(value) => super::txn::parse_amount_to_minor(&value.to_string(), currency),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputEntry {
    account_code: String,
    direction: DebitOrCredit,
    amount: InputAmount,
    memo: Option<String>,
    tax_category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputTransaction {
    date: String,
    description: String,
    entries: Vec<InputEntry>,
    currency: Option<String>,
    reference: Option<String>,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize)]
struct PreparedEntry {
    account_code: String,
    direction: DebitOrCredit,
    amount: db::amounts::PositiveMinorUnits,
    memo: Option<String>,
    tax_category: Option<String>,
}

#[derive(Serialize)]
struct PreparedTransaction {
    date: NaiveDate,
    description: String,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    entries: Vec<PreparedEntry>,
    metadata: Option<db::metadata::UserAnnotations>,
    #[serde(skip)]
    reference: Option<TransactionReference>,
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}

fn parse_currency(code: &str) -> Result<Currency, CliError> {
    Currency::from_code(code).map_err(|error| invalid(format!("invalid currency: {error}")))
}

fn parse_date(date: &str, format: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(date.trim(), format)
        .map_err(|error| invalid(format!("invalid date '{date}': {error}")))
}

fn prepare_json(
    input: InputTransaction,
    default_currency: &crate::cli::CurrencyArg,
) -> Result<PreparedTransaction, CliError> {
    let currency = input
        .currency
        .as_deref()
        .map_or_else(|| default_currency.resolve(), parse_currency)?;
    let entries = input
        .entries
        .into_iter()
        .map(|entry| {
            Ok(PreparedEntry {
                amount: entry.amount.minor_units(currency)?.try_into()?,
                account_code: entry.account_code,
                direction: entry.direction,
                memo: entry.memo,
                tax_category: entry.tax_category,
            })
        })
        .collect::<Result<_, CliError>>()?;
    Ok(PreparedTransaction {
        date: parse_date(&input.date, "%Y-%m-%d")?,
        description: input.description,
        currency,
        entries,
        metadata: input
            .metadata
            .map(db::metadata::UserAnnotations::from_fields)
            .transpose()?,
        reference: input.reference.as_deref().map(str::parse).transpose()?,
    })
}

struct CsvColumns {
    date: usize,
    description: usize,
    amount: usize,
    reference: Option<usize>,
}

impl CsvColumns {
    fn new(headers: &csv::StringRecord, options: &CsvMapping) -> Result<Self, CliError> {
        let required = |name: &str| {
            headers
                .iter()
                .position(|header| header == name)
                .ok_or_else(|| invalid(format!("CSV header '{name}' not found")))
        };
        let mut seen = std::collections::HashSet::new();
        for header in headers {
            if !seen.insert(header) {
                return Err(invalid(format!("duplicate CSV header '{header}'")));
            }
        }
        Ok(Self {
            date: required(&options.date_col)?,
            description: required(&options.description_col)?,
            amount: required(&options.amount_col)?,
            reference: match options.reference_col.as_deref() {
                Some(name) => Some(required(name)?),
                None => headers.iter().position(|header| header == "reference"),
            },
        })
    }
}

fn prepare_csv(
    record: &csv::StringRecord,
    columns: &CsvColumns,
    options: &ImportOptions,
    bank: &str,
    suspense: &str,
) -> Result<PreparedTransaction, CliError> {
    let currency = options.currency.resolve()?;
    let signed = super::txn::parse_amount_to_minor(record[columns.amount].trim(), currency)?;
    let inflow = (signed > 0) == (options.mapping.positive_amount == PositiveAmount::Inflow);
    let (debit, credit) = if inflow {
        (bank, suspense)
    } else {
        (suspense, bank)
    };
    let amount = signed
        .checked_abs()
        .ok_or_else(|| invalid("amount is too large"))?
        .try_into()?;
    let entries = [
        (debit, DebitOrCredit::Debit),
        (credit, DebitOrCredit::Credit),
    ]
    .into_iter()
    .map(|(account, direction)| PreparedEntry {
        account_code: account.to_string(),
        direction,
        amount,
        memo: None,
        tax_category: None,
    })
    .collect();
    Ok(PreparedTransaction {
        date: parse_date(&record[columns.date], &options.mapping.date_format)?,
        description: record[columns.description].to_string(),
        currency,
        entries,
        metadata: None,
        // Blank CSV cells mean the source row has no explicit reference.
        reference: columns
            .reference
            .and_then(|index| record[index].parse().ok()),
    })
}

fn read_transactions(
    content: &str,
    format: ImportFormat,
    options: &ImportOptions,
    bank: Option<&str>,
    suspense: Option<&str>,
) -> Result<Vec<PreparedTransaction>, CliError> {
    match format {
        ImportFormat::Json => {
            let inputs: Vec<InputTransaction> = serde_json::from_str(content)
                .map_err(|error| invalid(format!("JSON import: {error}")))?;
            inputs
                .into_iter()
                .enumerate()
                .map(|(index, input)| {
                    prepare_json(input, &options.currency).map_err(|error| {
                        invalid(format!("JSON transaction {}: {error}", index + 1))
                    })
                })
                .collect()
        }
        ImportFormat::Csv => {
            let bank =
                bank.ok_or_else(|| CliError::Usage("--account is required for CSV import".into()))?;
            let suspense = suspense
                .ok_or_else(|| CliError::Usage("--suspense is required for CSV import".into()))?;
            if bank == suspense {
                return Err(invalid("CSV --account and --suspense must be different"));
            }
            read_csv(content, options, bank, suspense, usize::MAX)
        }
        ImportFormat::Ofx => Err(CliError::Usage("expected CSV or JSON import".into())),
    }
}

fn read_csv(
    content: &str,
    options: &ImportOptions,
    bank: &str,
    suspense: &str,
    max_rows: usize,
) -> Result<Vec<PreparedTransaction>, CliError> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::Headers)
        .from_reader(content.as_bytes());
    let headers = reader
        .headers()
        .map_err(|error| invalid(format!("CSV headers: {error}")))?;
    let columns = CsvColumns::new(headers, &options.mapping)?;
    reader
        .records()
        .enumerate()
        .map(|(index, record)| {
            if index >= max_rows {
                return Err(invalid(format!("statement exceeds {max_rows} rows")));
            }
            let row = index + 2;
            let record = record.map_err(|error| invalid(format!("CSV row {row}: {error}")))?;
            prepare_csv(&record, &columns, options, bank, suspense)
                .map_err(|error| invalid(format!("CSV row {row}: {error}")))
        })
        .collect()
}

/// Normalize source identity before the configured categorization rules are applied.
pub(crate) fn read_csv_statement(
    content: &str,
    mapping: &CsvMapping,
    currency: Currency,
    bank: &str,
    suspense: &str,
    max_rows: usize,
) -> Result<Vec<SourceRow>, CliError> {
    let options = ImportOptions {
        mapping: mapping.clone(),
        currency: currency.into(),
    };
    let mut occurrences = HashMap::new();
    read_csv(content, &options, bank, suspense, max_rows)?
        .into_iter()
        .map(|mut row| {
            let reference = row_reference(&mut row, ImportFormat::Csv, &mut occurrences)?
                .parse::<TransactionReference>()?;
            let bank_entry = row
                .entries
                .iter()
                .find(|entry| entry.account_code == bank)
                .ok_or_else(|| invalid("CSV row has no bank entry"))?;
            let amount =
                PositiveMoney::new(Money::from_minor(i128::from(bank_entry.amount), currency))
                    .map_err(|error| invalid(error.to_string()))?;
            Ok(SourceRow {
                reference,
                date: row.date,
                description: row.description,
                amount,
                bank_direction: bank_entry.direction,
                metadata: None,
            })
        })
        .collect()
}

fn row_reference(
    prepared: &mut PreparedTransaction,
    format: ImportFormat,
    occurrences: &mut HashMap<String, usize>,
) -> Result<String, CliError> {
    if let Some(reference) = prepared.reference.take() {
        return Ok(reference.into_string());
    }
    let bytes = serde_json::to_vec(prepared).map_err(|error| invalid(error.to_string()))?;
    let prefix = if format == ImportFormat::Csv {
        "csv"
    } else {
        "json"
    };
    let hash = format!("{prefix}:sha256:{:x}", Sha256::digest(bytes));
    // Count only unreferenced rows with the same normalized content, so unrelated
    // rows and explicit references cannot shift ordinals on overlapping imports.
    let ordinal = occurrences.entry(hash.clone()).or_default();
    *ordinal += 1;
    Ok(format!("{hash}:{ordinal}"))
}

#[derive(Serialize)]
struct BatchResult {
    dry_run: bool,
    imported: usize,
    skipped: usize,
    errors: usize,
    transactions: Vec<ImportOutcome>,
}

#[derive(Serialize)]
struct ImportOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    date: String,
    description: String,
    reference: String,
    status: ImportStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ImportStatus {
    Imported,
    Skipped,
}

struct ImportContext<'a> {
    db: &'a Db,
    company: &'a str,
    actor: &'a db::Actor,
    format: ImportFormat,
    rules: &'a [db::rules::Rule],
    bank: Option<&'a str>,
    suspense: Option<&'a str>,
    strategy: transactions::ConflictStrategy,
    dry_run: bool,
}

fn post_import(
    ctx: &ImportContext<'_>,
    mut prepared: PreparedTransaction,
    occurrences: &mut HashMap<String, usize>,
) -> Result<ImportOutcome, CliError> {
    let reference = row_reference(&mut prepared, ctx.format, occurrences)?;
    // Existing CSV references are checked against the uncategorized source.
    // A changed rule cannot change the identity or validation of a replay.
    let skip_source = ctx.format == ImportFormat::Csv
        && ctx.strategy == transactions::ConflictStrategy::Skip
        && transactions::reference_exists(ctx.db.read(), ctx.company, &reference)?;
    if !skip_source && ctx.format == ImportFormat::Csv {
        categorize_csv(&mut prepared, ctx.rules, ctx.bank, ctx.suspense)?;
    }
    let entries = prepared
        .entries
        .iter()
        .map(|entry| transactions::PostEntryParams {
            account_code: entry.account_code.clone(),
            direction: entry.direction,
            amount: entry.amount,
            memo: entry.memo.clone(),
            tax_category: entry.tax_category.clone(),
        })
        .collect::<Vec<_>>();
    let params = transactions::PostTransactionParams {
        company_slug: ctx.company,
        description: &prepared.description,
        metadata: prepared
            .metadata
            .as_ref()
            .map(db::metadata::UserAnnotations::as_str),
        currency: prepared.currency,
        date: &prepared.date.to_string(),
        entries: &entries,
        correlate: None,
        reference: Some(&reference),
        on_conflict: ctx.strategy,
        actor: ctx.actor,
    };
    let source = transactions::PostingSource::Import;
    let (id, status) = if skip_source {
        transactions::build_journal(ctx.db.conn(), &params, source)?;
        (None, ImportStatus::Skipped)
    } else {
        match transactions::post_from_source(ctx.db.conn(), &params, source)? {
            transactions::PostResult::Created(id) => {
                ((!ctx.dry_run).then_some(id), ImportStatus::Imported)
            }
            transactions::PostResult::Skipped(_) => (None, ImportStatus::Skipped),
        }
    };
    Ok(ImportOutcome {
        id,
        date: prepared.date.to_string(),
        description: prepared.description,
        reference,
        status,
    })
}

fn load_csv_rules(
    db: &Db,
    company: &str,
    format: ImportFormat,
    bank: Option<&str>,
    suspense: Option<&str>,
) -> Result<Vec<db::rules::Rule>, CliError> {
    if format != ImportFormat::Csv {
        return Ok(Vec::new());
    }
    for code in [bank, suspense].into_iter().flatten() {
        accounts::get_account(db.read(), company, code)
            .map_err(|error| invalid(format!("CSV account validation: {error}")))?;
    }
    db::rules::list_rules(db.read(), company)
}

fn categorize_csv(
    prepared: &mut PreparedTransaction,
    rules: &[db::rules::Rule],
    bank: Option<&str>,
    suspense: Option<&str>,
) -> Result<(), CliError> {
    let bank = bank.ok_or_else(|| invalid("missing bank account"))?;
    let fallback = suspense.ok_or_else(|| invalid("missing suspense account"))?;
    let contra = db::rules::contra_account(rules, &prepared.description, bank, fallback)?;
    for entry in &mut prepared.entries {
        if entry.account_code == fallback {
            entry.account_code = contra.to_string();
        }
    }
    Ok(())
}

/// Run the CSV/JSON importer, committing the entire batch or rolling it back.
///
/// # Errors
///
/// Returns [`CliError`] for invalid input, posting/conflict errors, or I/O errors.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &TxnImportArgs,
    format: ImportFormat,
) -> Result<(), CliError> {
    let TxnImportArgs {
        file,
        dry_run,
        options,
        account,
        suspense,
        on_conflict,
        ..
    } = args;
    let strategy = match on_conflict {
        OnConflictArg::Error => transactions::ConflictStrategy::Error,
        OnConflictArg::Skip => transactions::ConflictStrategy::Skip,
    };
    let content = file.read_text()?;
    let prepared = read_transactions(
        &content,
        format,
        options,
        account.as_deref(),
        suspense.as_deref(),
    )?;
    let actor = cli.principal();
    let transaction = db.conn().unchecked_transaction()?;
    let rules = load_csv_rules(db, company, format, account.as_deref(), suspense.as_deref())?;
    let mut result = BatchResult {
        dry_run: *dry_run,
        imported: 0,
        skipped: 0,
        errors: 0,
        transactions: Vec::new(),
    };
    let mut occurrences = HashMap::new();
    let ctx = ImportContext {
        db,
        company,
        actor: &actor,
        format,
        rules: &rules,
        bank: account.as_deref(),
        suspense: suspense.as_deref(),
        strategy,
        dry_run: *dry_run,
    };
    for (index, prepared) in prepared.into_iter().enumerate() {
        let outcome =
            post_import(&ctx, prepared, &mut occurrences).map_err(|error| match error {
                CliError::Forbidden(message) => {
                    CliError::Forbidden(format!("import transaction {}: {message}", index + 1))
                }
                CliError::Unauthorized(message) => {
                    CliError::Unauthorized(format!("import transaction {}: {message}", index + 1))
                }
                other => invalid(format!("import transaction {}: {other}", index + 1)),
            })?;
        match outcome.status {
            ImportStatus::Imported => result.imported += 1,
            ImportStatus::Skipped => result.skipped += 1,
        }
        result.transactions.push(outcome);
    }
    // Render before committing, so a serialization failure cannot hide a commit.
    let rendered = render_result(cli, company, &result)?;
    if *dry_run {
        transaction.rollback()?;
    } else {
        transaction.commit()?;
    }
    if let Some(rendered) = rendered {
        println!("{rendered}");
    }
    Ok(())
}

fn render_result(
    cli: &Cli,
    company: &str,
    result: &BatchResult,
) -> Result<Option<String>, CliError> {
    if crate::cli::resolve_format(None, cli) == OutputFormat::Json {
        let envelope = json::Envelope {
            ok: true,
            meta: cli.meta(Operation::TxnImport, Some(company)),
            data: Some(result),
            error: None,
        };
        return serde_json::to_string_pretty(&envelope)
            .map(Some)
            .map_err(|error| invalid(format!("import output: {error}")));
    }
    if cli.verbosity.quiet {
        return Ok(None);
    }
    let action = if result.dry_run {
        "Would import"
    } else {
        "Imported"
    };
    Ok(Some(format!(
        "{action} {} transactions, skipped {} duplicates.",
        result.imported, result.skipped
    )))
}
