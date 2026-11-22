//! OFX bank statement import.
//!
//! Parses OFX/QFX files and posts each transaction as a balanced double-entry
//! journal entry against a user-specified bank account and suspense account.

use crate::operation::Operation;
use std::fmt;
use std::path::Path;

use beankeeper::types::{Currency, Money};
use chrono::NaiveDate;
use ofx_rs::{
    OfxDocument,
    aggregates::TransactionList,
    document::{BankingMessageSet, CreditCardMessageSet, InvestmentMessageSet},
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::cli::{Cli, ImportFormat, InputSource};
use crate::db::connection::{Db, ReadConn};
use crate::db::{accounts, transactions};
use crate::error::CliError;
use crate::identity::{Granted, Post};
use crate::output::amounts::format_money_grouped;
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
                "OFX statement currency '{code}' is not supported by beankeeper; \
                 supported currencies: USD, EUR, GBP, JPY, CHF, CAD, AUD, BHD, KWD, MXN"
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
    pub id: Option<crate::db::ids::TransactionId>,
    pub date: String,
    pub description: String,
    pub amount: Money,
    pub is_inflow: bool,
}

/// A transaction skipped due to duplicate reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTransaction {
    pub date: String,
    pub description: String,
    pub amount: Money,
    pub reference: String,
}

/// A transaction that failed to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTransaction {
    pub date: String,
    pub description: String,
    pub amount: Money,
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
    let overflow = || OfxImportError::AmountConversion {
        fit_id: fit_id.to_string(),
        detail: format!("amount {decimal} exceeds i64 range"),
    };
    let scaled = abs.checked_mul(multiplier).ok_or_else(overflow)?;

    if scaled != scaled.trunc() {
        return Err(OfxImportError::AmountConversion {
            fit_id: fit_id.to_string(),
            detail: format!(
                "amount {decimal} has more decimal places than the currency allows ({minor_unit_exponent})"
            ),
        });
    }

    scaled.trunc().to_i64().ok_or_else(overflow)
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
///
/// Includes currency to prevent collisions when different banks/statements
/// share the same account ID and fit ID across currencies.
#[must_use]
fn build_ofx_reference(currency: &str, account_id: &str, fit_id: &str) -> String {
    format!("ofx:{currency}:{account_id}:{fit_id}")
}

/// Detect import format from file extension.
///
/// # Errors
///
/// Returns [`OfxImportError::CannotDetectFormat`] if the extension is not recognized.
pub fn detect_format(path: impl AsRef<Path>) -> Result<ImportFormat, OfxImportError> {
    let path = path.as_ref();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    match ext.as_deref() {
        Some("ofx" | "qfx") => Ok(ImportFormat::Ofx),
        Some("csv") => Ok(ImportFormat::Csv),
        Some("json") => Ok(ImportFormat::Json),
        _ => Err(OfxImportError::CannotDetectFormat {
            path: path.display().to_string(),
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
// Single-transaction processing
// ---------------------------------------------------------------------------

/// Intermediate representation for one OFX transaction ready to post.
pub(crate) struct PreparedTransaction {
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) amount_minor: crate::db::amounts::PositiveMinorUnits,
    pub(crate) is_inflow: bool,
    pub(crate) reference: String,
    pub(crate) metadata: String,
    currency: Currency,
}

impl PreparedTransaction {
    fn amount(&self) -> Money {
        Money::from_minor(i128::from(self.amount_minor), self.currency)
    }
}

/// Prepare a single OFX transaction for posting.
pub(crate) fn prepare_transaction(
    txn: &ofx_rs::aggregates::StatementTransaction,
    account_id: &str,
    currency: Currency,
) -> Result<Option<PreparedTransaction>, OfxImportError> {
    let fit_id = txn.fit_id().as_str();
    let amount = txn.amount().as_decimal();

    // Skip zero amounts — cannot form a valid double-entry.
    if amount.is_zero() {
        return Ok(None);
    }

    let amount_minor = ofx_amount_to_minor(amount, currency.minor_units(), fit_id)?
        .try_into()
        .map_err(|error: CliError| OfxImportError::AmountConversion {
            fit_id: fit_id.to_string(),
            detail: error.to_string(),
        })?;
    let is_inflow = !amount.is_sign_negative();
    let posted = txn.date_posted().as_offset_date_time();
    let date = NaiveDate::from_ymd_opt(
        posted.year(),
        u32::from(u8::from(posted.month())),
        u32::from(posted.day()),
    )
    .ok_or_else(|| OfxImportError::ParseFailed {
        message: format!("invalid posting date for transaction '{fit_id}'"),
    })?;
    let description = build_description(txn.name(), txn.memo(), fit_id);
    let reference = build_ofx_reference(currency.code(), account_id, fit_id);
    let metadata = build_metadata_json(&txn.transaction_type().to_string());

    Ok(Some(PreparedTransaction {
        date,
        description,
        amount_minor,
        is_inflow,
        reference,
        metadata,
        currency,
    }))
}

/// Post one prepared transaction, returning the outcome.
fn post_one(ctx: &StatementContext<'_>, prepared: &PreparedTransaction) -> TransactionOutcome {
    // Pre-check for duplicates; an early outcome means we must not post.
    if let Some(outcome) = check_conflict(ctx.reader, ctx.company, prepared, ctx.conflict_strategy)
    {
        return outcome;
    }

    let entries = match build_entries(ctx, prepared) {
        Ok(entries) => entries,
        Err(error) => {
            return TransactionOutcome::Failed(FailedTransaction {
                date: prepared.date.to_string(),
                description: prepared.description.clone(),
                amount: prepared.amount(),
                error: error.to_string(),
            });
        }
    };

    if ctx.dry_run {
        return TransactionOutcome::Imported(ImportedTransaction {
            id: None,
            date: prepared.date.to_string(),
            description: prepared.description.clone(),
            amount: prepared.amount(),
            is_inflow: prepared.is_inflow,
        });
    }

    insert_transaction(ctx, prepared, &entries)
}

/// Resolve a pre-existing duplicate reference into an early [`TransactionOutcome`].
///
/// Returns `Some(outcome)` when the transaction must not be posted (duplicate
/// under the active strategy, or a lookup error) and `None` when posting may
/// proceed.
fn check_conflict(
    conn: ReadConn<'_>,
    company: &str,
    prepared: &PreparedTransaction,
    conflict_strategy: transactions::ConflictStrategy,
) -> Option<TransactionOutcome> {
    match transactions::reference_exists(conn, company, &prepared.reference) {
        Ok(true) => Some(match conflict_strategy {
            transactions::ConflictStrategy::Error => {
                TransactionOutcome::Failed(FailedTransaction {
                    date: prepared.date.to_string(),
                    description: prepared.description.clone(),
                    amount: prepared.amount(),
                    error: format!("duplicate reference: {}", prepared.reference),
                })
            }
            transactions::ConflictStrategy::Skip => {
                TransactionOutcome::Skipped(SkippedTransaction {
                    date: prepared.date.to_string(),
                    description: prepared.description.clone(),
                    amount: prepared.amount(),
                    reference: prepared.reference.clone(),
                })
            }
        }),
        Ok(false) => None,
        Err(e) => Some(TransactionOutcome::Failed(FailedTransaction {
            date: prepared.date.to_string(),
            description: prepared.description.clone(),
            amount: prepared.amount(),
            error: e.to_string(),
        })),
    }
}

/// Resolve the contra account and build the balanced double-entry pair.
fn build_entries(
    ctx: &StatementContext<'_>,
    prepared: &PreparedTransaction,
) -> Result<Vec<transactions::PostEntryParams>, CliError> {
    let contra = crate::db::rules::contra_account(
        ctx.rules,
        &prepared.description,
        ctx.account_code,
        ctx.suspense_code,
    )?;
    // Preserve the bank side and route the contra side through the selected account.
    let (debit_code, credit_code) = if prepared.is_inflow {
        (ctx.account_code, contra)
    } else {
        (contra, ctx.account_code)
    };

    let amount = prepared.amount_minor;
    Ok(vec![
        transactions::PostEntryParams {
            account_code: debit_code.to_string(),
            direction: beankeeper::types::DebitOrCredit::Debit,
            amount,
            memo: None,
            tax_category: accounts::get_account(ctx.reader, ctx.company, debit_code)?
                .default_tax_category,
        },
        transactions::PostEntryParams {
            account_code: credit_code.to_string(),
            direction: beankeeper::types::DebitOrCredit::Credit,
            amount,
            memo: None,
            tax_category: accounts::get_account(ctx.reader, ctx.company, credit_code)?
                .default_tax_category,
        },
    ])
}

/// Persist the validated bank/contra pair through the audited posting path.
fn insert_transaction(
    ctx: &StatementContext<'_>,
    prepared: &PreparedTransaction,
    entries: &[transactions::PostEntryParams],
) -> TransactionOutcome {
    let params = transactions::PostTransactionParams {
        company_slug: ctx.company,
        description: &prepared.description,
        metadata: Some(&prepared.metadata),
        currency: prepared.currency,
        date: prepared.date,
        entries,
        correlate: None,
        reference: Some(&prepared.reference),
        on_conflict: ctx.conflict_strategy,
        actor: ctx.actor,
    };

    match Granted::<Post>::with_db(ctx.db, ctx.actor, |grant| {
        transactions::post_transaction(grant, &params)
    }) {
        Ok(transactions::PostResult::Created(id)) => {
            TransactionOutcome::Imported(ImportedTransaction {
                id: Some(id),
                date: prepared.date.to_string(),
                description: prepared.description.clone(),
                amount: prepared.amount(),
                is_inflow: prepared.is_inflow,
            })
        }
        Ok(transactions::PostResult::Skipped(_)) => {
            // This should already be caught by our pre-check above, but for completeness:
            TransactionOutcome::Skipped(SkippedTransaction {
                date: prepared.date.to_string(),
                description: prepared.description.clone(),
                amount: prepared.amount(),
                reference: prepared.reference.clone(),
            })
        }
        Err(error @ (CliError::Forbidden(_) | CliError::Unauthorized(_))) => {
            TransactionOutcome::Denied(error)
        }
        Err(e) => TransactionOutcome::Failed(FailedTransaction {
            date: prepared.date.to_string(),
            description: prepared.description.clone(),
            amount: prepared.amount(),
            error: e.to_string(),
        }),
    }
}

enum TransactionOutcome {
    Denied(CliError),
    Imported(ImportedTransaction),
    Skipped(SkippedTransaction),
    Failed(FailedTransaction),
}

// ---------------------------------------------------------------------------
// Statement processing
// ---------------------------------------------------------------------------

/// Context for processing a statement's transactions.
struct StatementContext<'a> {
    reader: ReadConn<'a>,
    rules: &'a [crate::db::rules::Rule],
    actor: &'a crate::db::Actor,
    db: &'a Db,
    company: &'a str,
    account_code: &'a str,
    suspense_code: &'a str,
    ofx_account_id: &'a str,
    currency: Currency,
    dry_run: bool,
    conflict_strategy: transactions::ConflictStrategy,
    verbose: bool,
}

/// Process all transactions from a single statement's transaction list.
fn process_transactions(
    ctx: &StatementContext<'_>,
    txn_list: &ofx_rs::aggregates::TransactionList,
    result: &mut ImportResult,
) -> Result<(), CliError> {
    for txn in txn_list.transactions() {
        let prepared = match prepare_transaction(txn, ctx.ofx_account_id, ctx.currency) {
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
                    description: build_description(txn.name(), txn.memo(), txn.fit_id().as_str()),
                    amount: Money::from_minor(0, ctx.currency),
                    error: e.to_string(),
                });
                continue;
            }
        };

        match post_one(ctx, &prepared) {
            TransactionOutcome::Denied(error) => return Err(error),
            TransactionOutcome::Imported(t) => result.imported.push(t),
            TransactionOutcome::Skipped(t) => result.skipped.push(t),
            TransactionOutcome::Failed(t) => result.errors.push(t),
        }
    }
    Ok(())
}

/// Validate a currency code from OFX against beankeeper's currency table.
pub(crate) fn validate_currency(code: &str) -> Result<Currency, OfxImportError> {
    Currency::from_code(code).map_err(|_| OfxImportError::UnsupportedCurrency {
        code: code.to_string(),
    })
}

pub(crate) fn parse_document(content: &str) -> Result<OfxDocument, OfxImportError> {
    ofx_rs::parse(content).map_err(|error| OfxImportError::ParseFailed {
        message: error.to_string(),
    })
}

pub(crate) struct OfxStatement<'a> {
    pub(crate) account_id: &'a str,
    pub(crate) currency: &'a str,
    pub(crate) transactions: Option<&'a TransactionList>,
}

pub(crate) fn statements(doc: &OfxDocument) -> impl Iterator<Item = OfxStatement<'_>> {
    let bank = doc
        .banking()
        .into_iter()
        .flat_map(BankingMessageSet::statement_responses)
        .filter_map(|wrapper| wrapper.response())
        .map(|stmt| OfxStatement {
            account_id: stmt.bank_account().account_id().as_str(),
            currency: stmt.currency_default().as_str(),
            transactions: stmt.transaction_list(),
        });
    let cards = doc
        .credit_card()
        .into_iter()
        .flat_map(CreditCardMessageSet::statement_responses)
        .filter_map(|wrapper| wrapper.response())
        .map(|stmt| OfxStatement {
            account_id: stmt.credit_card_account().account_id().as_str(),
            currency: stmt.currency_default().as_str(),
            transactions: stmt.transaction_list(),
        });
    let investments = doc
        .investment()
        .into_iter()
        .flat_map(InvestmentMessageSet::statement_responses)
        .filter_map(|wrapper| wrapper.response())
        .map(|stmt| OfxStatement {
            account_id: stmt.investment_account().account_id().as_str(),
            currency: stmt.currency_default().as_str(),
            transactions: stmt.transaction_list(),
        });
    bank.chain(cards).chain(investments)
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

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
    file: &InputSource,
    account_code: &str,
    suspense_code: &str,
    dry_run: bool,
    on_conflict: crate::cli::OnConflictArg,
) -> Result<(), CliError> {
    let verbose = cli.verbosity.verbose;
    let actor = cli.principal();
    let rules = crate::db::rules::list_rules(db_handle.read(), company)?;
    let format = crate::cli::resolve_format(None, cli);

    let conflict_strategy = match on_conflict {
        crate::cli::OnConflictArg::Error => transactions::ConflictStrategy::Error,
        crate::cli::OnConflictArg::Skip => transactions::ConflictStrategy::Skip,
    };

    validate_accounts(db_handle.read(), company, account_code, suspense_code)?;

    // Read and parse OFX.
    let content = file.read_text()?;
    let doc = parse_document(&content)?;

    let mut result = ImportResult {
        imported: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
    };

    for stmt in statements(&doc) {
        let currency = validate_currency(stmt.currency)?;
        let ctx = StatementContext {
            reader: db_handle.read(),
            rules: &rules,
            actor: &actor,
            db: db_handle,
            company,
            account_code,
            suspense_code,
            ofx_account_id: stmt.account_id,
            currency,
            dry_run,
            conflict_strategy,
            verbose,
        };
        if let Some(list) = stmt.transactions {
            process_transactions(&ctx, list, &mut result)?;
        }
    }

    // Render output.
    render_result(&result, cli, company, dry_run, format)?;
    Ok(())
}

fn validate_accounts(
    conn: ReadConn<'_>,
    company: &str,
    bank: &str,
    suspense: &str,
) -> Result<(), CliError> {
    for code in [bank, suspense] {
        if !accounts::account_exists(conn, company, code)? {
            return Err(OfxImportError::AccountNotFound {
                code: code.to_string(),
            }
            .into());
        }
    }
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
        let meta = cli.meta(Operation::TxnImport, Some(company));
        let json_str = json::render_import_result(result, dry_run, meta)?;
        println!("{json_str}");
    } else {
        if cli.verbosity.verbose {
            for t in &result.imported {
                let sign = if t.is_inflow { "+" } else { "-" };
                let action = if dry_run { "would import" } else { "imported" };
                eprintln!(
                    "  [{action}] {}  {:<50} {sign}{}",
                    t.date,
                    t.description,
                    format_money_grouped(t.amount)
                );
            }
            for t in &result.skipped {
                eprintln!(
                    "  [skipped]  {}  {:<50} (duplicate: {})",
                    t.date, t.description, t.reference
                );
            }
            for t in &result.errors {
                eprintln!("  [error]    {}  {:<50} {}", t.date, t.description, t.error);
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
            build_ofx_reference("USD", "9876543210", "1001"),
            "ofx:USD:9876543210:1001"
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
        assert_eq!(build_metadata_json("CHECK"), r#"{"ofx_type":"CHECK"}"#);
    }

    #[test]
    fn metadata_json_debit() {
        assert_eq!(build_metadata_json("DEBIT"), r#"{"ofx_type":"DEBIT"}"#);
    }
}
