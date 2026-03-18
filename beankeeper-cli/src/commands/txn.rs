use std::collections::HashMap;
use std::path::Path;

use beankeeper::core::JournalEntry;
use beankeeper::prelude::IdempotencyKey;
use beankeeper::types::{Currency, DocumentType, Money};

use crate::cli::{Cli, OutputFormat, TxnCommand, resolve_format};
use crate::db::connection::Db;
use crate::db::{self, accounts, attachments, transactions};
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run a `bk txn` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &TxnCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db_handle = Db::open(&cli.db, pp.as_ref())?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        TxnCommand::Post {
            description,
            debit,
            credit,
            metadata,
            currency,
            date,
            correlate,
            reference,
        } => run_post(
            cli,
            &db_handle,
            company,
            description,
            debit,
            credit,
            metadata.as_deref(),
            currency,
            date.as_deref(),
            *correlate,
            reference.as_deref(),
        ),
        TxnCommand::List { account, from, to, limit, offset } => {
            let lp = transactions::ListTransactionParams {
                company_slug: company,
                account_filter: account.as_deref(),
                from_date: from.as_deref(),
                to_date: to.as_deref(),
                limit: *limit,
                offset: *offset,
            };
            run_list(cli, &db_handle, &lp, format, use_color)
        }
        TxnCommand::Show { id } => run_show(cli, &db_handle, company, *id, format, use_color),
        TxnCommand::Import {
            file: _,
            format: _,
            dry_run: _,
        } => Err(CliError::General(
            "import command not yet implemented".into(),
        )),
        TxnCommand::Attach {
            transaction_id,
            file_path,
            document_type,
            entry,
        } => run_attach(
            cli,
            &db_handle,
            company,
            *transaction_id,
            file_path,
            document_type,
            *entry,
        ),
        TxnCommand::Reconcile => run_reconcile(cli, &db_handle, format, use_color),
    }
}

/// Parse an `account_code:amount[:memo]` string into `(code, minor_units, optional_memo)`.
///
/// Undecorated integers are treated as major units (e.g. `50` = 50 dollars).
/// Decimal amounts are also major units (e.g. `50.00` = 50 dollars).
/// The amount is converted to minor units based on the currency's decimal places.
/// An optional third segment after a second colon is treated as a memo.
fn parse_entry_arg(s: &str, currency: Currency) -> Result<(String, i64, Option<String>), CliError> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();

    if parts.len() < 2 {
        return Err(CliError::Usage(format!(
            "invalid entry format '{s}': expected 'account_code:amount' or 'account_code:amount:memo'"
        )));
    }

    let code = parts[0];
    let amount_str = parts[1];

    if code.is_empty() {
        return Err(CliError::Usage(format!(
            "invalid entry format '{s}': account code is empty"
        )));
    }

    let minor = parse_amount_to_minor(amount_str.trim(), currency)?;

    let memo = if parts.len() == 3 && !parts[2].is_empty() {
        Some(parts[2].to_string())
    } else {
        None
    };

    Ok((code.to_string(), minor, memo))
}

/// Parse an amount string to minor currency units.
///
/// The input is always interpreted as major units:
/// - `50` -> 50 dollars -> 5000 cents (for USD, 2 minor units)
/// - `50.00` -> 50 dollars -> 5000 cents
/// - `0.50` -> 0.50 dollars -> 50 cents
fn parse_amount_to_minor(s: &str, currency: Currency) -> Result<i64, CliError> {
    let minor_units = currency.minor_units();
    let multiplier = 10i64.pow(u32::from(minor_units));

    if let Some((whole_str, frac_str)) = s.split_once('.') {
        // Has decimal point
        let whole: i64 = whole_str.parse().map_err(|_| {
            CliError::Usage(format!("invalid amount '{s}': cannot parse whole part"))
        })?;

        // Validate fractional part length
        if frac_str.len() > usize::from(minor_units) {
            return Err(CliError::Validation(format!(
                "amount '{s}' has too many decimal places for {currency} \
                 (max {minor_units})"
            )));
        }

        // Pad fractional part to minor_units digits
        let padded = format!("{frac_str:0<width$}", width = usize::from(minor_units));
        let frac: i64 = padded.parse().map_err(|_| {
            CliError::Usage(format!(
                "invalid amount '{s}': cannot parse fractional part"
            ))
        })?;

        let total = whole
            .checked_mul(multiplier)
            .and_then(|w| w.checked_add(frac))
            .ok_or_else(|| {
                CliError::Validation(format!("amount '{s}' is too large"))
            })?;

        Ok(total)
    } else {
        // No decimal point: treat as whole major units
        let whole: i64 = s.parse().map_err(|_| {
            CliError::Usage(format!("invalid amount '{s}': not a valid number"))
        })?;

        whole.checked_mul(multiplier).ok_or_else(|| {
            CliError::Validation(format!("amount '{s}' is too large"))
        })
    }
}

/// Execute the `txn post` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_post(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    description: &str,
    debit_args: &[String],
    credit_args: &[String],
    metadata: Option<&str>,
    currency_code: &str,
    date: Option<&str>,
    correlate: Option<i64>,
    reference: Option<&str>,
) -> Result<(), CliError> {
    // 1. Parse currency
    let currency = Currency::from_code(currency_code).map_err(|e| {
        CliError::Validation(format!("invalid currency: {e}"))
    })?;

    // 2. Parse debit/credit args into (code, minor_units, memo) tuples
    let mut parsed_debits = Vec::new();
    for arg in debit_args {
        parsed_debits.push(parse_entry_arg(arg, currency)?);
    }

    let mut parsed_credits = Vec::new();
    for arg in credit_args {
        parsed_credits.push(parse_entry_arg(arg, currency)?);
    }

    // 3. Determine the transaction date
    let effective_date = match date {
        Some(d) => d.to_string(),
        None => chrono::Local::now().format("%Y-%m-%d").to_string(),
    };
    let txn_date = chrono::NaiveDate::parse_from_str(&effective_date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid date '{effective_date}': {e}")))?;

    // 4. Look up accounts and build library Account objects for validation
    let mut journal = JournalEntry::new(txn_date, description);
    if let Some(meta) = metadata {
        journal = journal.with_metadata(meta);
    }

    for (code, minor, memo) in &parsed_debits {
        let row = accounts::get_account(db_handle.conn(), company, code)?;
        let account = db::row_to_account(&row)?;
        let money = Money::from_minor(i128::from(*minor), currency);
        if let Some(m) = memo {
            journal = journal.debit_with_memo(&account, money, m)?;
        } else {
            journal = journal.debit(&account, money)?;
        }
    }

    for (code, minor, memo) in &parsed_credits {
        let row = accounts::get_account(db_handle.conn(), company, code)?;
        let account = db::row_to_account(&row)?;
        let money = Money::from_minor(i128::from(*minor), currency);
        if let Some(m) = memo {
            journal = journal.credit_with_memo(&account, money, m)?;
        } else {
            journal = journal.credit(&account, money)?;
        }
    }

    // 5. Validate via post() - enforces balance invariant
    let _transaction = journal.post()?;

    // 6. Build entries for DB persistence
    let mut db_entries: Vec<(String, String, i64, Option<String>)> = Vec::new();
    for (code, minor, memo) in &parsed_debits {
        db_entries.push((code.clone(), "debit".to_string(), *minor, memo.clone()));
    }
    for (code, minor, memo) in &parsed_credits {
        db_entries.push((code.clone(), "credit".to_string(), *minor, memo.clone()));
    }

    // Resolve idempotency key from the reference, if provided.
    let idempotency_key = reference
        .map(IdempotencyKey::from_reference)
        .transpose()
        .map_err(|e| CliError::Validation(format!("invalid reference: {e}")))?;

    let params = transactions::PostTransactionParams {
        company_slug: company,
        description,
        metadata,
        currency: currency_code,
        date: &effective_date,
        entries: &db_entries,
        correlate,
        reference: idempotency_key.as_ref().map(IdempotencyKey::as_str),
    };

    let txn_id = transactions::post_transaction(db_handle.conn(), &params)?;

    if !cli.verbosity.quiet {
        eprintln!("[ok] transaction #{txn_id} posted");
    }

    Ok(())
}

/// Execute the `txn list` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_list(
    cli: &Cli,
    db_handle: &Db,
    params: &transactions::ListTransactionParams<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let rows = transactions::list_transactions(db_handle.conn(), params)?;

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_transaction_list(&rows, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            // For JSON, include entries for each transaction
            let mut entries_map: HashMap<i64, Vec<db::EntryRow>> = HashMap::new();
            for txn in &rows {
                let entries =
                    transactions::get_entries_for_transaction(db_handle.conn(), txn.id)?;
                entries_map.insert(txn.id, entries);
            }
            let rendered = output::json::render_transactions(&rows, &entries_map)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_transactions(&rows)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        let count = rows.len();
        eprintln!(
            "{count} {noun}",
            noun = if count == 1 {
                "transaction"
            } else {
                "transactions"
            }
        );
    }

    Ok(())
}

/// Execute the `txn show` subcommand.
fn run_show(
    _cli: &Cli,
    db_handle: &Db,
    company: &str,
    id: i64,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let (txn, entries) = transactions::get_transaction(db_handle.conn(), company, id)?;
    let att_rows = attachments::list_attachments(db_handle.conn(), company, id)?;

    // Determine currency minor units for formatting
    let currency_minor_units = Currency::from_code(&txn.currency)
        .map(|c| c.minor_units())
        .unwrap_or(2);

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_transaction_detail(
                &txn,
                &entries,
                currency_minor_units,
                use_color,
            );
            println!("{rendered}");
            if !att_rows.is_empty() {
                let att_rendered = output::table::render_attachments(&att_rows, use_color);
                println!("{att_rendered}");
            }
        }
        OutputFormat::Json => {
            let mut entries_map: HashMap<i64, Vec<db::EntryRow>> = HashMap::new();
            entries_map.insert(txn.id, entries);
            let mut att_map: HashMap<i64, Vec<db::AttachmentRow>> = HashMap::new();
            att_map.insert(txn.id, att_rows);
            let rendered =
                output::json::render_transactions_with_attachments(&[txn], &entries_map, &att_map)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_transactions(&[txn])?;
            print!("{rendered}");
        }
    }

    Ok(())
}

/// Execute the `txn attach` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_attach(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    transaction_id: i64,
    file_path: &str,
    document_type_str: &str,
    entry_id: Option<i64>,
) -> Result<(), CliError> {
    // 1. Validate document type
    let doc_type: DocumentType = document_type_str.parse().map_err(|e| {
        CliError::Validation(format!("{e}"))
    })?;

    // 2. Validate the transaction exists for this company
    let _txn = transactions::get_transaction(db_handle.conn(), company, transaction_id)?;

    // 3. Validate source file exists
    let source = Path::new(file_path);
    if !source.exists() {
        return Err(CliError::NotFound(format!(
            "file not found: {file_path}"
        )));
    }

    // 4. Hash and store the file in content-addressed storage
    let (hash, stored_path) = attachments::hash_and_store_file(source, &cli.db)?;

    // 5. Derive the URI (relative to db parent) and original filename
    let uri = stored_path.file_name().map_or_else(
        || stored_path.to_string_lossy().to_string(),
        |n| format!("attachments/{}", n.to_string_lossy()),
    );

    let original_filename = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string());

    // 6. Insert attachment record
    let params = attachments::StoreAttachmentParams {
        transaction_id,
        entry_id,
        company_slug: company,
        uri: &uri,
        document_type: &doc_type.to_string(),
        hash: Some(hash.as_str()),
        original_filename: original_filename.as_deref(),
    };
    let att_id = attachments::store_attachment(db_handle.conn(), &params)?;

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] attachment #{att_id} added to transaction #{transaction_id} ({doc_type}, {hash_short})",
            hash_short = &hash[..hash.len().min(12)]
        );
    }

    Ok(())
}

fn run_reconcile(
    _cli: &Cli,
    db_handle: &Db,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let orphans = transactions::find_orphaned_correlations(db_handle.conn())?;

    if orphans.is_empty() {
        match format {
            OutputFormat::Json => println!("[]"),
            OutputFormat::Csv => println!("transaction_id,company,description,date,partner_id"),
            OutputFormat::Table => eprintln!("[ok] no orphaned correlations found"),
        }
        return Ok(());
    }

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_orphaned_correlations(&orphans, use_color);
            println!("{rendered}");
            eprintln!("[!!] {} orphaned correlation(s) found", orphans.len());
        }
        OutputFormat::Json => {
            let rendered = output::json::render_orphaned_correlations(&orphans)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_orphaned_correlations(&orphans)?;
            print!("{rendered}");
        }
    }

    // Exit code 3 (validation error) when orphans found — useful in pipelines.
    Err(CliError::Validation(format!(
        "{} orphaned correlation(s) found",
        orphans.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_amount_whole_usd() {
        // 50 dollars -> 5000 cents
        let result = parse_amount_to_minor("50", Currency::USD);
        assert_eq!(result.ok(), Some(5000));
    }

    #[test]
    fn parse_amount_decimal_usd() {
        // 50.00 dollars -> 5000 cents
        let result = parse_amount_to_minor("50.00", Currency::USD);
        assert_eq!(result.ok(), Some(5000));
    }

    #[test]
    fn parse_amount_partial_cents() {
        // 50.5 -> 50.50 -> 5050 cents
        let result = parse_amount_to_minor("50.5", Currency::USD);
        assert_eq!(result.ok(), Some(5050));
    }

    #[test]
    fn parse_amount_jpy() {
        // JPY has 0 minor units: 50 -> 50
        let result = parse_amount_to_minor("50", Currency::JPY);
        assert_eq!(result.ok(), Some(50));
    }

    #[test]
    fn parse_amount_bhd() {
        // BHD has 3 minor units: 50 -> 50000
        let result = parse_amount_to_minor("50", Currency::BHD);
        assert_eq!(result.ok(), Some(50000));
    }

    #[test]
    fn parse_amount_bhd_decimal() {
        // BHD: 50.125 -> 50125
        let result = parse_amount_to_minor("50.125", Currency::BHD);
        assert_eq!(result.ok(), Some(50125));
    }

    #[test]
    fn parse_amount_too_many_decimals() {
        // USD only allows 2 decimal places
        let result = parse_amount_to_minor("50.123", Currency::USD);
        assert!(result.is_err());
    }

    #[test]
    fn parse_entry_arg_valid() {
        let (code, amount, memo) = parse_entry_arg("1000:50", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code, "1000");
        assert_eq!(amount, 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_decimal() {
        let (code, amount, memo) = parse_entry_arg("1000:50.00", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code, "1000");
        assert_eq!(amount, 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_with_memo() {
        let (code, amount, memo) = parse_entry_arg("1000:50:Net pay", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code, "1000");
        assert_eq!(amount, 5000);
        assert_eq!(memo.as_deref(), Some("Net pay"));
    }

    #[test]
    fn parse_entry_arg_with_empty_memo() {
        let (code, amount, memo) = parse_entry_arg("1000:50:", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code, "1000");
        assert_eq!(amount, 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_invalid_format() {
        let result = parse_entry_arg("1000-50", Currency::USD);
        assert!(result.is_err());
    }

    #[test]
    fn parse_entry_arg_empty_code() {
        let result = parse_entry_arg(":50", Currency::USD);
        assert!(result.is_err());
    }
}
