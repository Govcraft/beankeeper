//! OFX bank statement import.
//!
//! Parses OFX/QFX files and posts each transaction as a balanced double-entry
//! journal entry against a user-specified bank account and suspense account.

use std::fmt;
use std::io::Read;

use beankeeper::types::Currency;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rusqlite::params;

use crate::cli::{Cli, ImportFormat};
use crate::db::connection::Db;
use crate::db::{accounts, transactions};
use crate::error::CliError;
use crate::output::json;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// OFX import-specific errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfxImportError {
    /// The OFX file could not be parsed.
    ParseFailed { message: String },
    /// The `--account` flag is required for OFX import but was not provided.
    MissingAccountFlag,
    /// The `--suspense` flag is required for OFX import but was not provided.
    MissingSuspenseFlag,
    /// The specified account code does not exist.
    AccountNotFound { code: String },
    /// The OFX statement currency is not recognized by beankeeper.
    UnsupportedCurrency { code: String },
    /// An OFX amount could not be converted to minor units.
    AmountConversion { fit_id: String, detail: String },
    /// Format auto-detection failed.
    CannotDetectFormat { path: String },
}

impl fmt::Display for OfxImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseFailed { message } => write!(f, "OFX parse failed: {message}"),
            Self::MissingAccountFlag => write!(
                f,
                "--account is required for OFX import; specify the bank account code"
            ),
            Self::MissingSuspenseFlag => write!(
                f,
                "--suspense is required for OFX import; specify the contra/clearing account code"
            ),
            Self::AccountNotFound { code } => write!(
                f,
                "account '{code}' not found; create it first with 'bk account create'"
            ),
            Self::UnsupportedCurrency { code } => write!(
                f,
                "OFX statement currency '{code}' is not supported by beankeeper"
            ),
            Self::AmountConversion { fit_id, detail } => write!(
                f,
                "amount conversion failed for transaction '{fit_id}': {detail}"
            ),
            Self::CannotDetectFormat { path } => write!(
                f,
                "cannot detect import format from file '{path}'; specify --format explicitly"
            ),
        }
    }
}

impl std::error::Error for OfxImportError {}

impl From<OfxImportError> for CliError {
    fn from(e: OfxImportError) -> Self {
        match &e {
            OfxImportError::MissingAccountFlag
            | OfxImportError::MissingSuspenseFlag
            | OfxImportError::CannotDetectFormat { .. } => CliError::Usage(e.to_string()),
            OfxImportError::AccountNotFound { .. } => CliError::NotFound(e.to_string()),
            _ => CliError::Validation(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Outcome of an OFX import operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub imported: Vec<ImportedTransaction>,
    pub skipped: Vec<SkippedTransaction>,
    pub errors: Vec<FailedTransaction>,
}

/// A successfully posted transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTransaction {
    pub id: i64,
    pub date: String,
    pub description: String,
    pub amount_minor: i64,
    pub is_inflow: bool,
}

/// A transaction skipped due to duplicate reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTransaction {
    pub date: String,
    pub description: String,
    pub amount_minor: i64,
    pub reference: String,
}

/// A transaction that failed to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTransaction {
    pub date: String,
    pub description: String,
    pub amount_minor: i64,
    pub error: String,
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

/// Assemble a transaction description from OFX name and memo fields.
#[must_use]
fn build_description(name: Option<&str>, memo: Option<&str>, fit_id: &str) -> String {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    let memo = memo.map(str::trim).filter(|s| !s.is_empty());
    match (name, memo) {
        (Some(n), Some(m)) => format!("{n} - {m}"),
        (Some(n), None) => n.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => format!("OFX transaction {fit_id}"),
    }
}

/// Convert an OFX `Decimal` amount to beankeeper minor units (`i64`).
///
/// Takes the absolute value. Returns an error if the scaled result has a
/// fractional component or exceeds `i64` range.
fn ofx_amount_to_minor(
    decimal: Decimal,
    minor_unit_exponent: u8,
    fit_id: &str,
) -> Result<i64, OfxImportError> {
    let abs = decimal.abs();
    let multiplier = Decimal::from(10i64.pow(u32::from(minor_unit_exponent)));
    let scaled = abs * multiplier;

    if scaled != scaled.trunc() {
        return Err(OfxImportError::AmountConversion {
            fit_id: fit_id.to_string(),
            detail: format!(
                "amount {decimal} has more decimal places than the currency allows ({minor_unit_exponent})"
            ),
        });
    }

    scaled.trunc().to_i64().ok_or_else(|| {
        OfxImportError::AmountConversion {
            fit_id: fit_id.to_string(),
            detail: format!("amount {decimal} exceeds i64 range"),
        }
    })
}

/// Format an `OfxDateTime` as a `YYYY-MM-DD` string.
#[must_use]
fn format_ofx_date(dt: &ofx_rs::types::OfxDateTime) -> String {
    let odt = dt.as_offset_date_time();
    format!(
        "{:04}-{:02}-{:02}",
        odt.year(),
        u8::from(odt.month()),
        odt.day()
    )
}

/// Build the deduplication reference string for an OFX transaction.
#[must_use]
fn build_ofx_reference(account_id: &str, fit_id: &str) -> String {
    format!("ofx:{account_id}:{fit_id}")
}

/// Detect import format from file extension.
///
/// # Errors
///
/// Returns [`OfxImportError::CannotDetectFormat`] if the extension is not recognized.
pub fn detect_format(path: &str) -> Result<ImportFormat, OfxImportError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    match ext.as_deref() {
        Some("ofx" | "qfx") => Ok(ImportFormat::Ofx),
        Some("csv") => Ok(ImportFormat::Csv),
        Some("json") => Ok(ImportFormat::Json),
        _ => Err(OfxImportError::CannotDetectFormat {
            path: path.to_string(),
        }),
    }
}

/// Build the metadata JSON string for an OFX transaction type.
#[must_use]
fn build_metadata_json(ofx_type: &str) -> String {
    // Simple enough to avoid pulling in serde_json for one field.
    format!(r#"{{"ofx_type":"{ofx_type}"}}"#)
}

// ---------------------------------------------------------------------------
// Database helper
// ---------------------------------------------------------------------------

/// Check if a reference already exists for a company.
fn reference_exists(
    conn: &rusqlite::Connection,
    company: &str,
    reference: &str,
) -> Result<bool, CliError> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM transactions WHERE company_slug = ?1 AND reference = ?2",
            params![company, reference],
            |row| row.get(0),
        )
        .map_err(CliError::Sqlite)?;
    Ok(exists)
}

// ---------------------------------------------------------------------------
// Single-transaction processing
// ---------------------------------------------------------------------------

/// Intermediate representation for one OFX transaction ready to post.
struct PreparedTransaction {
    date: String,
    description: String,
    amount_minor: i64,
    is_inflow: bool,
    reference: String,
    metadata: String,
    currency: String,
}

/// Prepare a single OFX transaction for posting.
fn prepare_transaction(
    txn: &ofx_rs::aggregates::StatementTransaction,
    account_id: &str,
    currency_code: &str,
    minor_units: u8,
) -> Result<Option<PreparedTransaction>, OfxImportError> {
    let fit_id = txn.fit_id().as_str();
    let amount = txn.amount().as_decimal();

    // Skip zero amounts — cannot form a valid double-entry.
    if amount.is_zero() {
        return Ok(None);
    }

    let amount_minor = ofx_amount_to_minor(amount, minor_units, fit_id)?;
    let is_inflow = !amount.is_sign_negative();
    let date = format_ofx_date(txn.date_posted());
    let description = build_description(txn.name(), txn.memo(), fit_id);
    let reference = build_ofx_reference(account_id, fit_id);
    let metadata = build_metadata_json(&txn.transaction_type().to_string());

    Ok(Some(PreparedTransaction {
        date,
        description,
        amount_minor,
        is_inflow,
        reference,
        metadata,
        currency: currency_code.to_string(),
    }))
}

/// Post one prepared transaction, returning the outcome.
fn post_one(
    conn: &rusqlite::Connection,
    company: &str,
    account_code: &str,
    suspense_code: &str,
    prepared: &PreparedTransaction,
    dry_run: bool,
) -> TransactionOutcome {
    // Pre-check for duplicates.
    match reference_exists(conn, company, &prepared.reference) {
        Ok(true) => {
            return TransactionOutcome::Skipped(SkippedTransaction {
                date: prepared.date.clone(),
                description: prepared.description.clone(),
                amount_minor: prepared.amount_minor,
                reference: prepared.reference.clone(),
            });
        }
        Ok(false) => {}
        Err(e) => {
            return TransactionOutcome::Failed(FailedTransaction {
                date: prepared.date.clone(),
                description: prepared.description.clone(),
                amount_minor: prepared.amount_minor,
                error: e.to_string(),
            });
        }
    }

    if dry_run {
        return TransactionOutcome::Imported(ImportedTransaction {
            id: 0,
            date: prepared.date.clone(),
            description: prepared.description.clone(),
            amount_minor: prepared.amount_minor,
            is_inflow: prepared.is_inflow,
        });
    }

    // Build balanced entries: inflow = debit bank, credit suspense;
    // outflow = debit suspense, credit bank.
    let (debit_code, credit_code) = if prepared.is_inflow {
        (account_code, suspense_code)
    } else {
        (suspense_code, account_code)
    };

    let entries = vec![
        transactions::PostEntryParams {
            account_code: debit_code.to_string(),
            direction: "debit".to_string(),
            amount: prepared.amount_minor,
            memo: None,
            tax_category: None,
        },
        transactions::PostEntryParams {
            account_code: credit_code.to_string(),
            direction: "credit".to_string(),
            amount: prepared.amount_minor,
            memo: None,
            tax_category: None,
        },
    ];

    let params = transactions::PostTransactionParams {
        company_slug: company,
        description: &prepared.description,
        metadata: Some(&prepared.metadata),
        currency: &prepared.currency,
        date: &prepared.date,
        entries: &entries,
        correlate: None,
        reference: Some(&prepared.reference),
    };

    match transactions::post_transaction(conn, &params) {
        Ok(id) => TransactionOutcome::Imported(ImportedTransaction {
            id,
            date: prepared.date.clone(),
            description: prepared.description.clone(),
            amount_minor: prepared.amount_minor,
            is_inflow: prepared.is_inflow,
        }),
        Err(e) => TransactionOutcome::Failed(FailedTransaction {
            date: prepared.date.clone(),
            description: prepared.description.clone(),
            amount_minor: prepared.amount_minor,
            error: e.to_string(),
        }),
    }
}

enum TransactionOutcome {
    Imported(ImportedTransaction),
    Skipped(SkippedTransaction),
    Failed(FailedTransaction),
}

// ---------------------------------------------------------------------------
// Statement processing
// ---------------------------------------------------------------------------

/// Context for processing a statement's transactions.
struct StatementContext<'a> {
    conn: &'a rusqlite::Connection,
    company: &'a str,
    account_code: &'a str,
    suspense_code: &'a str,
    ofx_account_id: &'a str,
    currency_code: &'a str,
    minor_units: u8,
    dry_run: bool,
    verbose: bool,
}

/// Process all transactions from a single statement's transaction list.
fn process_transactions(
    ctx: &StatementContext<'_>,
    txn_list: &ofx_rs::aggregates::TransactionList,
    result: &mut ImportResult,
) {
    for txn in txn_list.transactions() {
        let prepared = match prepare_transaction(
            txn,
            ctx.ofx_account_id,
            ctx.currency_code,
            ctx.minor_units,
        ) {
            Ok(Some(p)) => p,
            Ok(None) => {
                if ctx.verbose {
                    eprintln!(
                        "  [skipped] {} zero-amount transaction (fit_id: {})",
                        format_ofx_date(txn.date_posted()),
                        txn.fit_id().as_str()
                    );
                }
                continue;
            }
            Err(e) => {
                result.errors.push(FailedTransaction {
                    date: format_ofx_date(txn.date_posted()),
                    description: build_description(
                        txn.name(),
                        txn.memo(),
                        txn.fit_id().as_str(),
                    ),
                    amount_minor: 0,
                    error: e.to_string(),
                });
                continue;
            }
        };

        match post_one(
            ctx.conn,
            ctx.company,
            ctx.account_code,
            ctx.suspense_code,
            &prepared,
            ctx.dry_run,
        ) {
            TransactionOutcome::Imported(t) => result.imported.push(t),
            TransactionOutcome::Skipped(t) => result.skipped.push(t),
            TransactionOutcome::Failed(t) => result.errors.push(t),
        }
    }
}

/// Validate a currency code from OFX against beankeeper's currency table.
fn validate_currency(code: &str) -> Result<(String, u8), OfxImportError> {
    let currency = Currency::from_code(code).map_err(|_| OfxImportError::UnsupportedCurrency {
        code: code.to_string(),
    })?;
    Ok((currency.code().to_string(), currency.minor_units()))
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Read input from file or stdin.
fn read_input(file: Option<&str>) -> Result<String, CliError> {
    match file {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(CliError::Io)?;
            Ok(buf)
        }
        Some(path) => std::fs::read_to_string(path).map_err(CliError::Io),
    }
}

/// Execute an OFX file import.
///
/// # Errors
///
/// Returns `CliError` on parse failure, missing accounts, or database errors.
#[allow(clippy::too_many_arguments)]
pub fn run_import_ofx(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    file: Option<&str>,
    account_code: &str,
    suspense_code: &str,
    dry_run: bool,
) -> Result<(), CliError> {
    let conn = db_handle.conn();
    let verbose = cli.verbosity.verbose;
    let format = crate::cli::resolve_format(None, cli);

    // Validate accounts exist.
    if !accounts::account_exists(conn, company, account_code)? {
        return Err(OfxImportError::AccountNotFound {
            code: account_code.to_string(),
        }
        .into());
    }
    if !accounts::account_exists(conn, company, suspense_code)? {
        return Err(OfxImportError::AccountNotFound {
            code: suspense_code.to_string(),
        }
        .into());
    }

    // Read and parse OFX.
    let content = read_input(file)?;
    let doc = ofx_rs::parse(&content).map_err(|e| OfxImportError::ParseFailed {
        message: e.to_string(),
    })?;

    let mut result = ImportResult {
        imported: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
    };

    // Process banking statement responses.
    if let Some(banking) = doc.banking() {
        for wrapper in banking.statement_responses() {
            if let Some(stmt) = wrapper.response() {
                let (currency_code, minor_units) =
                    validate_currency(stmt.currency_default().as_str())?;
                let ctx = StatementContext {
                    conn,
                    company,
                    account_code,
                    suspense_code,
                    ofx_account_id: stmt.bank_account().account_id().as_str(),
                    currency_code: &currency_code,
                    minor_units,
                    dry_run,
                    verbose,
                };
                if let Some(txn_list) = stmt.transaction_list() {
                    process_transactions(&ctx, txn_list, &mut result);
                }
            }
        }
    }

    // Process credit card statement responses.
    if let Some(cc) = doc.credit_card() {
        for wrapper in cc.statement_responses() {
            if let Some(stmt) = wrapper.response() {
                let (currency_code, minor_units) =
                    validate_currency(stmt.currency_default().as_str())?;
                let ctx = StatementContext {
                    conn,
                    company,
                    account_code,
                    suspense_code,
                    ofx_account_id: stmt.credit_card_account().account_id().as_str(),
                    currency_code: &currency_code,
                    minor_units,
                    dry_run,
                    verbose,
                };
                if let Some(txn_list) = stmt.transaction_list() {
                    process_transactions(&ctx, txn_list, &mut result);
                }
            }
        }
    }

    // Render output.
    render_result(&result, cli, company, dry_run, format)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Output rendering
// ---------------------------------------------------------------------------

fn render_result(
    result: &ImportResult,
    cli: &Cli,
    company: &str,
    dry_run: bool,
    format: crate::cli::OutputFormat,
) -> Result<(), CliError> {
    let imported_count = result.imported.len();
    let skipped_count = result.skipped.len();
    let error_count = result.errors.len();

    if format == crate::cli::OutputFormat::Json {
        let meta = json::meta("txn.import", Some(company));
        let json_str = json::render_import_result(result, dry_run, meta)?;
        println!("{json_str}");
    } else {
        if cli.verbosity.verbose {
            for t in &result.imported {
                let sign = if t.is_inflow { "+" } else { "-" };
                let action = if dry_run { "would import" } else { "imported" };
                eprintln!(
                    "  [{action}] {}  {:<50} {sign}{}",
                    t.date, t.description, t.amount_minor
                );
            }
            for t in &result.skipped {
                eprintln!(
                    "  [skipped]  {}  {:<50} (duplicate: {})",
                    t.date, t.description, t.reference
                );
            }
            for t in &result.errors {
                eprintln!(
                    "  [error]    {}  {:<50} {}",
                    t.date, t.description, t.error
                );
            }
        }

        let verb = if dry_run { "Would import" } else { "Imported" };
        let mut summary = format!("{verb} {imported_count} transaction");
        if imported_count != 1 {
            summary.push('s');
        }
        if skipped_count > 0 {
            use std::fmt::Write;
            let _ = write!(summary, ", skipped {skipped_count} duplicate");
            if skipped_count != 1 {
                summary.push('s');
            }
        }
        if error_count > 0 {
            use std::fmt::Write;
            let _ = write!(summary, ", {error_count} error");
            if error_count != 1 {
                summary.push('s');
            }
        }
        summary.push('.');

        if !cli.verbosity.quiet {
            println!("{summary}");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    // -- build_description -------------------------------------------------

    #[test]
    fn build_description_both_name_and_memo() {
        assert_eq!(
            build_description(Some("GROCERY STORE"), Some("Weekly groceries"), "12345"),
            "GROCERY STORE - Weekly groceries"
        );
    }

    #[test]
    fn build_description_name_only() {
        assert_eq!(
            build_description(Some("GROCERY STORE"), None, "12345"),
            "GROCERY STORE"
        );
    }

    #[test]
    fn build_description_memo_only() {
        assert_eq!(
            build_description(None, Some("Weekly groceries"), "12345"),
            "Weekly groceries"
        );
    }

    #[test]
    fn build_description_neither() {
        assert_eq!(
            build_description(None, None, "12345"),
            "OFX transaction 12345"
        );
    }

    #[test]
    fn build_description_whitespace_trimmed() {
        assert_eq!(
            build_description(Some("  STORE  "), Some("  memo  "), "1"),
            "STORE - memo"
        );
    }

    #[test]
    fn build_description_empty_strings_treated_as_none() {
        assert_eq!(
            build_description(Some(""), Some(""), "42"),
            "OFX transaction 42"
        );
    }

    // -- ofx_amount_to_minor -----------------------------------------------

    #[test]
    fn amount_positive_usd() {
        let d = Decimal::new(5000, 2); // 50.00
        assert_eq!(ofx_amount_to_minor(d, 2, "t1").unwrap(), 5000);
    }

    #[test]
    fn amount_negative_takes_absolute() {
        let d = Decimal::new(-5000, 2); // -50.00
        assert_eq!(ofx_amount_to_minor(d, 2, "t1").unwrap(), 5000);
    }

    #[test]
    fn amount_three_decimal_currency() {
        let d = Decimal::new(50125, 3); // 50.125
        assert_eq!(ofx_amount_to_minor(d, 3, "t1").unwrap(), 50125);
    }

    #[test]
    fn amount_jpy_zero_decimals() {
        let d = Decimal::new(5000, 0); // 5000
        assert_eq!(ofx_amount_to_minor(d, 0, "t1").unwrap(), 5000);
    }

    #[test]
    fn amount_fractional_remainder_errors() {
        let d = Decimal::new(50005, 3); // 50.005
        assert!(ofx_amount_to_minor(d, 2, "t1").is_err());
    }

    // -- build_ofx_reference -----------------------------------------------

    #[test]
    fn reference_format() {
        assert_eq!(
            build_ofx_reference("9876543210", "1001"),
            "ofx:9876543210:1001"
        );
    }

    // -- detect_format -----------------------------------------------------

    #[test]
    fn detect_ofx_extension() {
        assert_eq!(detect_format("statement.ofx").unwrap(), ImportFormat::Ofx);
    }

    #[test]
    fn detect_qfx_extension() {
        assert_eq!(detect_format("statement.QFX").unwrap(), ImportFormat::Ofx);
    }

    #[test]
    fn detect_csv_extension() {
        assert_eq!(detect_format("data.csv").unwrap(), ImportFormat::Csv);
    }

    #[test]
    fn detect_json_extension() {
        assert_eq!(detect_format("data.json").unwrap(), ImportFormat::Json);
    }

    #[test]
    fn detect_unknown_errors() {
        assert!(detect_format("data.txt").is_err());
    }

    // -- build_metadata_json -----------------------------------------------

    #[test]
    fn metadata_json_check() {
        assert_eq!(
            build_metadata_json("CHECK"),
            r#"{"ofx_type":"CHECK"}"#
        );
    }

    #[test]
    fn metadata_json_debit() {
        assert_eq!(
            build_metadata_json("DEBIT"),
            r#"{"ofx_type":"DEBIT"}"#
        );
    }
}
