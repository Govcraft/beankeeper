use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use beankeeper::core::{JournalEntry, TransactionError};
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
pub fn run(
    cli: &Cli,
    company: &str,
    sub: &TxnCommand,
    meta: Option<output::json::Meta>,
) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db_handle = Db::open_existing(&cli.db, pp.as_ref())?;
    if !matches!(sub, TxnCommand::Reconcile) {
        crate::db::companies::get_company(db_handle.conn(), company)?;
    }
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        TxnCommand::Post { .. } => {
            let actor = db::Actor::resolve(cli.actor.as_deref());
            let result = execute_post(&db_handle, company, sub, &actor)?;
            print_post_result(cli, company, result)
        }
        TxnCommand::List { .. } => run_list_or_count(
            cli,
            &db_handle,
            company,
            sub,
            (format, use_color),
            &mut std::io::stdout(),
        ),
        TxnCommand::Show { id } => run_show(
            &db_handle,
            company,
            *id,
            format,
            use_color,
            &mut std::io::stdout(),
        ),
        TxnCommand::Reverse { .. } => run_reverse(cli, &db_handle, company, sub),
        TxnCommand::Clear { .. } => super::txn_clear::run(cli, &db_handle, company, sub, meta),
        TxnCommand::Import { .. } => run_import(cli, &db_handle, company, sub),
        TxnCommand::Attach { .. } => run_attach(cli, &db_handle, company, sub),
        TxnCommand::Reconcile => {
            run_reconcile(&db_handle, format, use_color, &mut std::io::stdout())
        }
    }
}

/// Execute an atomic reversal and report its new or existing transaction ID.
fn run_reverse(cli: &Cli, db_handle: &Db, company: &str, sub: &TxnCommand) -> Result<(), CliError> {
    let TxnCommand::Reverse { id, .. } = sub else {
        return Err(CliError::General("expected txn reverse subcommand".into()));
    };
    let format = resolve_format(None, cli);
    let actor = db::Actor::resolve(cli.actor.as_deref());
    let result = execute_reverse(db_handle, company, sub, &actor)?;
    if format == OutputFormat::Json {
        println!(
            "{}",
            output::json::render_post_result(
                result,
                output::json::meta("txn.reverse", Some(company))
            )?
        );
    }
    if !cli.verbosity.quiet {
        match result {
            transactions::PostResult::Created(reversal_id) => {
                eprintln!("[ok] transaction #{id} reversed by #{reversal_id}");
            }
            transactions::PostResult::Skipped(reversal_id) => {
                eprintln!("[skipped] transaction #{id} already reversed by #{reversal_id}");
            }
        }
    }
    Ok(())
}

/// Execute an atomic reversal with an explicit audit actor.
fn execute_reverse(
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    actor: &db::Actor,
) -> Result<transactions::PostResult, CliError> {
    let TxnCommand::Reverse {
        id,
        date,
        description,
        reference,
    } = sub
    else {
        return Err(CliError::General("expected txn reverse subcommand".into()));
    };
    let date = date
        .clone()
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let reference = reference
        .as_deref()
        .map(normalize_reference)
        .transpose()
        .map_err(|e| CliError::Validation(e.to_string()))?;
    db::audit::reverse_transaction(
        db_handle.conn(),
        &db::audit::ReverseTransactionParams {
            company_slug: company,
            id: *id,
            date: &date,
            description: description.as_deref(),
            reference: reference.as_deref(),
            actor,
        },
    )
}

/// Execute a supported transaction operation without writing to stdout.
///
/// The returned value is exactly the data payload of the CLI JSON envelope.
/// Mutations use the explicitly supplied audit actor.
///
/// # Errors
///
/// Returns the same validation, lookup and database errors as the CLI. Import
/// and attachment operations are intentionally unavailable through this API.
pub fn execute_json(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    actor: &db::Actor,
) -> Result<serde_json::Value, CliError> {
    if !matches!(sub, TxnCommand::Reconcile) {
        db::get_company(db_handle.conn(), company)?;
    }
    let mut out = Vec::new();
    match sub {
        TxnCommand::Post { .. } => {
            let result = execute_post(db_handle, company, sub, actor)?;
            let meta = output::json::meta("txn.post", Some(company));
            out = output::json::render_post_result(result, meta)?.into_bytes();
        }
        TxnCommand::List { .. } => run_list_or_count(
            cli,
            db_handle,
            company,
            sub,
            (OutputFormat::Json, false),
            &mut out,
        )?,
        TxnCommand::Show { id } => {
            run_show(db_handle, company, *id, OutputFormat::Json, false, &mut out)?;
        }
        TxnCommand::Reconcile => run_reconcile(db_handle, OutputFormat::Json, false, &mut out)?,
        TxnCommand::Clear { .. } => {
            return super::txn_clear::execute_json(db_handle, company, sub, actor, None);
        }
        TxnCommand::Reverse { .. } => {
            let result = execute_reverse(db_handle, company, sub, actor)?;
            let meta = output::json::meta("txn.reverse", Some(company));
            out = output::json::render_post_result(result, meta)?.into_bytes();
        }
        TxnCommand::Import { .. } | TxnCommand::Attach { .. } => {
            return Err(CliError::Usage(
                "transaction operation is not available over MCP".into(),
            ));
        }
    }
    let envelope: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))?;
    Ok(envelope["data"].clone())
}

/// Execute the `txn import` subcommand.
fn run_import(cli: &Cli, db_handle: &Db, company: &str, sub: &TxnCommand) -> Result<(), CliError> {
    let TxnCommand::Import {
        file,
        format: import_format,
        dry_run,
        account,
        suspense,
        on_conflict,
        ..
    } = sub
    else {
        return Err(CliError::General("expected txn import subcommand".into()));
    };

    let effective_format = match import_format {
        Some(f) => *f,
        None => match file.as_deref() {
            Some(path) if path != "-" => crate::commands::import_ofx::detect_format(path)?,
            _ => {
                return Err(CliError::Usage(
                    "cannot detect format from stdin; specify --input-format".into(),
                ));
            }
        },
    };

    match effective_format {
        crate::cli::ImportFormat::Ofx => {
            let acct = account
                .as_deref()
                .ok_or(crate::commands::import_ofx::OfxImportError::MissingAccountFlag)?;
            let susp = suspense
                .as_deref()
                .ok_or(crate::commands::import_ofx::OfxImportError::MissingSuspenseFlag)?;
            crate::commands::import_ofx::run_import_ofx(
                cli,
                db_handle,
                company,
                file.as_deref(),
                acct,
                susp,
                *dry_run,
                *on_conflict,
            )
        }
        crate::cli::ImportFormat::Csv | crate::cli::ImportFormat::Json => {
            super::txn_import::run(cli, db_handle, company, sub, effective_format)
        }
    }
}

/// Build `ListTransactionParams` from CLI args and dispatch to list or count.
fn run_list_or_count(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    presentation: (OutputFormat, bool),
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (format, use_color) = presentation;
    let TxnCommand::List {
        account,
        from,
        to,
        limit,
        offset,
        description,
        amount_gt,
        amount_lt,
        amount_eq,
        currency,
        reference,
        metadata,
        meta,
        tax_category,
        direction,
        count,
    } = sub
    else {
        unreachable!("run_list_or_count called with non-List variant");
    };

    // Determine currency for amount conversion (filter amounts are in major units).
    let env_currency = std::env::var("BEANKEEPER_CURRENCY").ok();
    let currency_code = currency
        .as_deref()
        .or(env_currency.as_deref())
        .unwrap_or("USD");
    let amount_currency = Currency::from_code(currency_code)
        .map_err(|e| CliError::Validation(format!("invalid currency for amount filter: {e}")))?;

    let to_minor = |s: &str| parse_amount_to_minor(s, amount_currency);

    let dir_str = direction.map(crate::cli::DirectionArg::as_str);

    let mut lp = transactions::ListTransactionParams::for_company(company);
    lp.account_filter = account.as_deref();
    lp.from_date = from.as_deref();
    lp.to_date = to.as_deref();
    lp.limit = *limit;
    lp.offset = *offset;
    lp.description_like = description.as_deref();
    lp.amount_gt = amount_gt.as_deref().map(to_minor).transpose()?;
    lp.amount_lt = amount_lt.as_deref().map(to_minor).transpose()?;
    lp.amount_eq = amount_eq.as_deref().map(to_minor).transpose()?;
    lp.currency_filter = currency.as_deref();
    lp.reference_filter = reference.as_deref();
    lp.metadata_like = metadata.as_deref();
    let meta_eq = db::metadata_filter::parse_filters(meta)?;
    lp.meta_eq = &meta_eq;
    lp.tax_category_filter = tax_category.as_deref();
    lp.direction_filter = dir_str;

    if *count {
        run_count(cli, db_handle, &lp, format, out)
    } else {
        run_list(cli, db_handle, &lp, format, use_color, out)
    }
}

/// Parse `--tax` flag arguments into a map of `account_code` -> `tax_category`.
///
/// Each argument should be `account_code=category`.
fn parse_tax_args(args: &[String]) -> Result<HashMap<String, String>, CliError> {
    let mut map = HashMap::new();
    for arg in args {
        let (code, category) = arg.split_once('=').ok_or_else(|| {
            CliError::Usage(format!(
                "invalid --tax format '{arg}': expected 'account_code=category'"
            ))
        })?;

        if code.is_empty() || category.is_empty() {
            return Err(CliError::Usage(format!(
                "invalid --tax format '{arg}': account code and category must be non-empty"
            )));
        }

        map.insert(code.to_string(), category.to_string());
    }
    Ok(map)
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
    if minor <= 0 {
        let amount = Money::from_minor(i128::from(minor), currency)
            .amount()
            .format_decimal(currency.minor_units());
        return Err(CliError::Validation(format!(
            "entry amount must be positive, got {amount} {currency}"
        )));
    }

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
/// - `-12.34` -> -1234 cents (an optional leading `-` or `+` applies to the
///   whole amount, fractional part included)
///
/// Both the whole and fractional parts must be plain ASCII digits; the whole
/// part must be present (`.5` is rejected, `0.5` is fine).
pub(crate) fn parse_amount_to_minor(s: &str, currency: Currency) -> Result<i64, CliError> {
    let minor_units = currency.minor_units();
    let multiplier = 10i64.pow(u32::from(minor_units));

    let (negative, unsigned) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (whole_str, frac_str) = match unsigned.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (unsigned, None),
    };

    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole_str.is_empty() || !all_digits(whole_str) {
        return Err(CliError::Usage(format!(
            "invalid amount '{s}': not a valid number"
        )));
    }
    let whole: i64 = whole_str
        .parse()
        .map_err(|_| CliError::Validation(format!("amount '{s}' is too large")))?;

    let frac: i64 = match frac_str {
        None => 0,
        Some(f) => {
            if !all_digits(f) {
                return Err(CliError::Usage(format!(
                    "invalid amount '{s}': cannot parse fractional part"
                )));
            }
            if f.len() > usize::from(minor_units) {
                return Err(CliError::Validation(format!(
                    "amount '{s}' has too many decimal places for {currency} \
                     (max {minor_units})"
                )));
            }
            // Pad fractional part to minor_units digits ("5" -> "50" for USD).
            let padded = format!("{f:0<width$}", width = usize::from(minor_units));
            if padded.is_empty() {
                0
            } else {
                padded.parse().map_err(|_| {
                    CliError::Usage(format!(
                        "invalid amount '{s}': cannot parse fractional part"
                    ))
                })?
            }
        }
    };

    let magnitude = whole
        .checked_mul(multiplier)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| CliError::Validation(format!("amount '{s}' is too large")))?;

    Ok(if negative { -magnitude } else { magnitude })
}

/// Normalize a user-supplied transaction reference for storage.
///
/// Trims surrounding whitespace and rejects empty input. The returned value is
/// stored verbatim in the `reference` column so that it round-trips through
/// `txn list --reference <KEY>`.
///
/// # Errors
///
/// Returns an error message if the reference is empty or whitespace-only.
fn normalize_reference(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("reference must not be empty");
    }
    Ok(trimmed.to_string())
}

/// Validate a posting and retain the input currency in balance diagnostics.
fn validate_journal(journal: JournalEntry, currency: Currency) -> Result<(), CliError> {
    match journal.post() {
        Ok(_) => Ok(()),
        Err(TransactionError::Unbalanced {
            total_debits,
            total_credits,
        }) => {
            let difference = Money::new(total_debits, currency)
                .checked_sub(Money::new(total_credits, currency))
                .map_err(|error| CliError::Validation(error.to_string()))?
                .abs()
                .amount()
                .format_decimal(currency.minor_units());
            let debits = total_debits.format_decimal(currency.minor_units());
            let credits = total_credits.format_decimal(currency.minor_units());
            Err(CliError::Validation(format!(
                "transaction is unbalanced: debits={debits} {currency}, credits={credits} {currency} (difference {difference} {currency})"
            )))
        }
        Err(error) => Err(error.into()),
    }
}

/// Execute the `txn post` subcommand.
fn execute_post(
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    actor: &db::Actor,
) -> Result<transactions::PostResult, CliError> {
    let TxnCommand::Post {
        description,
        debit: debit_args,
        credit: credit_args,
        metadata,
        meta: meta_args,
        currency: currency_code,
        date,
        correlate,
        reference,
        on_conflict,
        tax: tax_args,
    } = sub
    else {
        return Err(CliError::General("expected txn post subcommand".into()));
    };
    let date = date.as_deref();
    let metadata = metadata.as_deref();
    let reference = reference.as_deref();
    // 1. Parse currency
    let currency = Currency::from_code(currency_code)
        .map_err(|e| CliError::Validation(format!("invalid currency: {e}")))?;

    // 2. Parse debit/credit args into (code, minor_units, memo) tuples
    let parsed_debits = parse_entries(debit_args, currency)?;
    let parsed_credits = parse_entries(credit_args, currency)?;

    // 2b. Parse --tax flags into account_code -> category map
    let tax_map = parse_tax_args(tax_args)?;

    // 3. Determine the transaction date
    let effective_date = match date {
        Some(d) => d.to_string(),
        None => chrono::Local::now().format("%Y-%m-%d").to_string(),
    };
    let txn_date = chrono::NaiveDate::parse_from_str(&effective_date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid date '{effective_date}': {e}")))?;

    // 3b. Normalize -m and merge --meta pairs into a JSON object (issue #17).
    let meta_pairs = db::metadata::parse_meta_args(meta_args)?;
    let metadata = db::metadata::build_post_metadata(metadata, &meta_pairs)?;
    let metadata = metadata.as_deref();

    // 4. Look up accounts, resolve tax categories, and build library Entry objects
    let mut journal = JournalEntry::new(txn_date, description);
    if let Some(meta) = metadata {
        journal = journal.with_metadata(meta);
    }

    // Helper: resolve tax_category for an entry (explicit --tax > account default > None)
    let resolve_tax = |code: &str, account_row: &db::AccountRow| -> Option<String> {
        tax_map
            .get(code)
            .cloned()
            .or_else(|| account_row.default_tax_category.clone())
    };

    for (args, debit) in [(&parsed_debits, true), (&parsed_credits, false)] {
        for arg in args {
            let entry = library_entry(db_handle, company, arg, currency, debit, &resolve_tax)?;
            journal = journal.entry(entry);
        }
    }

    // 5. Validate via post() - enforces balance invariant
    validate_journal(journal, currency)?;

    // 6. Build entries for DB persistence
    let db_entries = build_db_entries(
        db_handle,
        company,
        &parsed_debits,
        &parsed_credits,
        &resolve_tax,
    )?;

    // Store the user-supplied reference key verbatim so that it round-trips
    // through `txn list --reference <KEY>`. The reference column is itself the
    // idempotency/dedup key, so the raw value is also what conflict checks
    // compare against.
    let reference = reference
        .map(normalize_reference)
        .transpose()
        .map_err(|e| CliError::Validation(format!("invalid reference: {e}")))?;

    let params = transactions::PostTransactionParams {
        company_slug: company,
        description,
        metadata,
        currency: currency_code,
        date: &effective_date,
        entries: &db_entries,
        correlate: *correlate,
        reference: reference.as_deref(),
        on_conflict: conflict_strategy(*on_conflict),
        actor,
    };

    transactions::post_transaction(db_handle.conn(), &params)
}
fn parse_entries(
    args: &[String],
    currency: Currency,
) -> Result<Vec<(String, i64, Option<String>)>, CliError> {
    args.iter()
        .map(|arg| parse_entry_arg(arg, currency))
        .collect()
}

/// Map the `--on-conflict` flag to the storage strategy.
fn conflict_strategy(arg: crate::cli::OnConflictArg) -> transactions::ConflictStrategy {
    match arg {
        crate::cli::OnConflictArg::Error => transactions::ConflictStrategy::Error,
        crate::cli::OnConflictArg::Skip => transactions::ConflictStrategy::Skip,
        crate::cli::OnConflictArg::Upsert => transactions::ConflictStrategy::Upsert,
    }
}

/// Look up the account of one parsed `--debit`/`--credit` argument and build
/// the library entry (with its memo and resolved tax category).
fn library_entry(
    db_handle: &Db,
    company: &str,
    (code, minor, memo): &(String, i64, Option<String>),
    currency: Currency,
    debit: bool,
    resolve_tax: &dyn Fn(&str, &db::AccountRow) -> Option<String>,
) -> Result<beankeeper::types::Entry, CliError> {
    use beankeeper::types::Entry;
    let row = accounts::get_account(db_handle.conn(), company, code)?;
    let account = db::row_to_account(&row)?;
    let money = Money::from_minor(i128::from(*minor), currency);
    let entry = match (debit, memo) {
        (true, Some(m)) => Entry::debit_with_memo(account, money, m)?,
        (true, None) => Entry::debit(account, money)?,
        (false, Some(m)) => Entry::credit_with_memo(account, money, m)?,
        (false, None) => Entry::credit(account, money)?,
    };
    Ok(match resolve_tax(code, &row) {
        Some(cat) => entry.with_tax_category(cat),
        None => entry,
    })
}

/// Report a posted (or skipped) transaction: JSON on stdout, status on stderr.
fn print_post_result(
    cli: &Cli,
    company: &str,
    post_result: transactions::PostResult,
) -> Result<(), CliError> {
    let format = crate::cli::resolve_format(None, cli);
    if format == crate::cli::OutputFormat::Json {
        let meta = crate::output::json::meta("txn.post", Some(company));
        let rendered = crate::output::json::render_post_result(post_result, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        match post_result {
            transactions::PostResult::Created(id) => eprintln!("[ok] transaction #{id} posted"),
            transactions::PostResult::Skipped(id) => {
                eprintln!("[skipped] duplicate reference; transaction already exists (id: {id})");
            }
        }
    }

    Ok(())
}

/// Build DB entry params from parsed debit/credit args.
fn build_db_entries(
    db_handle: &Db,
    company: &str,
    parsed_debits: &[(String, i64, Option<String>)],
    parsed_credits: &[(String, i64, Option<String>)],
    resolve_tax: &dyn Fn(&str, &db::AccountRow) -> Option<String>,
) -> Result<Vec<transactions::PostEntryParams>, CliError> {
    let mut db_entries = Vec::new();
    for (code, minor, memo) in parsed_debits {
        let row = accounts::get_account(db_handle.conn(), company, code)?;
        db_entries.push(transactions::PostEntryParams {
            account_code: code.clone(),
            direction: "debit".to_string(),
            amount: *minor,
            memo: memo.clone(),
            tax_category: resolve_tax(code, &row),
        });
    }
    for (code, minor, memo) in parsed_credits {
        let row = accounts::get_account(db_handle.conn(), company, code)?;
        db_entries.push(transactions::PostEntryParams {
            account_code: code.clone(),
            direction: "credit".to_string(),
            amount: *minor,
            memo: memo.clone(),
            tax_category: resolve_tax(code, &row),
        });
    }
    Ok(db_entries)
}

/// Execute the `txn list` subcommand.
fn run_list(
    cli: &Cli,
    db_handle: &Db,
    params: &transactions::ListTransactionParams<'_>,
    format: OutputFormat,
    use_color: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let rows = transactions::list_transactions(db_handle.conn(), params)?;

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_transaction_list(&rows, use_color);
            writeln!(out, "{rendered}")?;
        }
        OutputFormat::Json => {
            // For JSON, include entries for each transaction
            let mut entries_map: HashMap<i64, Vec<db::EntryRow>> = HashMap::new();
            for txn in &rows {
                let entries = transactions::get_entries_for_transaction(db_handle.conn(), txn.id)?;
                entries_map.insert(txn.id, entries);
            }
            let meta = output::json::meta("txn.list", Some(params.company_slug));
            let rendered = output::json::render_transactions(&rows, &entries_map, meta)?;
            writeln!(out, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_transactions(&rows)?;
            write!(out, "{rendered}")?;
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

/// Execute the `txn list --count` subcommand.
fn run_count(
    cli: &Cli,
    db_handle: &Db,
    params: &transactions::ListTransactionParams<'_>,
    format: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let count = transactions::count_transactions(db_handle.conn(), params)?;

    match format {
        OutputFormat::Json => {
            let meta = output::json::meta("txn.list", Some(params.company_slug));
            let rendered = output::json::render_count(count, meta)?;
            writeln!(out, "{rendered}")?;
        }
        OutputFormat::Table | OutputFormat::Csv => {
            writeln!(out, "{count}")?;
        }
    }

    if !cli.verbosity.quiet {
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
    db_handle: &Db,
    company: &str,
    id: i64,
    format: OutputFormat,
    use_color: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (txn, entries) = transactions::get_transaction(db_handle.conn(), company, id)?;
    let att_rows = attachments::list_attachments(db_handle.conn(), company, id)?;
    let mut clearings: HashMap<i64, Vec<db::clearings::Clearing>> = HashMap::new();
    for e in &entries {
        clearings.insert(e.id, db::clearings::list_clearings(db_handle.conn(), e.id)?);
    }

    // Determine currency minor units for formatting
    let currency_minor_units = Currency::from_code(&txn.currency).map_or(2, |c| c.minor_units());

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_transaction_detail_with_clearings(
                &txn,
                &entries,
                Some(&clearings),
                currency_minor_units,
                use_color,
            );
            writeln!(out, "{rendered}")?;
            if !att_rows.is_empty() {
                let att_rendered = output::table::render_attachments(&att_rows, use_color);
                writeln!(out, "{att_rendered}")?;
            }
        }
        OutputFormat::Json => {
            let mut entries_map: HashMap<i64, Vec<db::EntryRow>> = HashMap::new();
            entries_map.insert(txn.id, entries);
            let mut att_map: HashMap<i64, Vec<db::AttachmentRow>> = HashMap::new();
            att_map.insert(txn.id, att_rows);
            let meta = output::json::meta("txn.show", Some(company));
            let rendered = output::json::render_transactions_with_attachments(
                &[txn],
                &entries_map,
                &att_map,
                &clearings,
                meta,
            )?;
            writeln!(out, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_transactions(&[txn])?;
            write!(out, "{rendered}")?;
        }
    }

    Ok(())
}

/// Execute the `txn attach` subcommand.
fn run_attach(cli: &Cli, db_handle: &Db, company: &str, sub: &TxnCommand) -> Result<(), CliError> {
    let TxnCommand::Attach {
        transaction_id,
        file_path,
        document_type: document_type_str,
        entry: entry_id,
    } = sub
    else {
        return Err(CliError::General("expected txn attach subcommand".into()));
    };
    let transaction_id = *transaction_id;
    let entry_id = *entry_id;
    // 1. Validate document type
    let doc_type: DocumentType = document_type_str
        .parse()
        .map_err(|e| CliError::Validation(format!("{e}")))?;

    // 2. Validate the transaction exists for this company
    let _txn = transactions::get_transaction(db_handle.conn(), company, transaction_id)?;

    // 3. Validate source file exists
    let source = Path::new(file_path);
    if !source.exists() {
        return Err(CliError::NotFound(format!("file not found: {file_path}")));
    }

    // 4. Hash and store the file in content-addressed storage
    let (hash, stored_path) = attachments::hash_and_store_file(source, &cli.db)?;

    // 5. Derive the URI (relative to db parent) and original filename
    let uri = stored_path.file_name().map_or_else(
        || stored_path.to_string_lossy().to_string(),
        |n| format!("attachments/{}", n.to_string_lossy()),
    );

    let original_filename = source.file_name().map(|n| n.to_string_lossy().to_string());

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

    let format = crate::cli::resolve_format(None, cli);
    if format == crate::cli::OutputFormat::Json {
        let meta = crate::output::json::meta("txn.attach", Some(company));
        let rendered = crate::output::json::render_attached(att_id, transaction_id, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] attachment #{att_id} added to transaction #{transaction_id} ({doc_type}, {hash_short})",
            hash_short = &hash[..hash.len().min(12)]
        );
    }

    Ok(())
}
fn run_reconcile(
    db_handle: &Db,
    format: OutputFormat,
    use_color: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let orphans = transactions::find_orphaned_correlations(db_handle.conn())?;

    if orphans.is_empty() {
        match format {
            OutputFormat::Json => {
                let meta = output::json::meta("txn.reconcile", None);
                let rendered = output::json::render_orphaned_correlations(&[], meta)?;
                writeln!(out, "{rendered}")?;
            }
            OutputFormat::Csv => {
                writeln!(out, "transaction_id,company,description,date,partner_id")?;
            }
            OutputFormat::Table => eprintln!("[ok] no orphaned correlations found"),
        }
        return Ok(());
    }

    match format {
        OutputFormat::Table => {
            let rendered = output::table::render_orphaned_correlations(&orphans, use_color);
            writeln!(out, "{rendered}")?;
            eprintln!("[!!] {} orphaned correlation(s) found", orphans.len());
        }
        OutputFormat::Json => {
            let meta = output::json::meta("txn.reconcile", None);
            let rendered = output::json::render_orphaned_correlations(&orphans, meta)?;
            writeln!(out, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_orphaned_correlations(&orphans)?;
            write!(out, "{rendered}")?;
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

    fn fixture() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        db::create_company(db.conn(), "acme", "Acme", None)?;
        db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
        db::create_account(
            db.conn(),
            "acme",
            "4000",
            "Revenue",
            "revenue",
            Some("income"),
        )?;
        Ok(db)
    }

    fn execute_args(db: &Db, args: &[&str]) -> Result<serde_json::Value, CliError> {
        use clap::Parser;
        let cli = Cli::try_parse_from(
            ["bk", "--company", "acme", "--json", "--quiet"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .unwrap_or_else(|error| panic!("test arguments: {error}"));
        let crate::cli::Command::Txn(txn) = &cli.command else {
            panic!("expected transaction command");
        };
        execute_json(
            &cli,
            db,
            "acme",
            txn.command.as_ref(),
            &db::Actor::new("mcp:test"),
        )
    }

    #[test]
    fn shared_post_preserves_reference_metadata_tax_and_idempotency() -> Result<(), CliError> {
        let db = fixture()?;
        let args = [
            "txn",
            "post",
            "--description",
            "Sale",
            "--debit",
            "1000:12.50:memo",
            "--credit",
            "4000:12.50",
            "--date",
            "2026-03-01",
            "--reference",
            "invoice-1",
            "--on-conflict",
            "skip",
            "--meta",
            "invoice=one",
        ];
        let first = execute_args(&db, &args)?;
        let retry = execute_args(&db, &args)?;
        assert_eq!(first["created"], true);
        assert_eq!(retry["skipped"], true);
        assert_eq!(retry["existing_id"], first["id"]);
        let listed = execute_args(&db, &["txn", "list", "--reference", "invoice-1"])?;
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert_eq!(listed[0]["reference"], "invoice-1");
        assert_eq!(listed[0]["entries"][0]["amount"], 1250);
        assert_eq!(listed[0]["entries"][0]["memo"], "memo");
        assert_eq!(listed[0]["entries"][1]["tax_category"], "income");
        let metadata: serde_json::Value =
            serde_json::from_str(listed[0]["metadata"].as_str().unwrap_or_default())
                .unwrap_or_else(|error| panic!("metadata: {error}"));
        assert_eq!(metadata["invoice"], "one");
        Ok(())
    }

    #[test]
    fn shared_post_rejects_unbalanced_before_persisting() -> Result<(), CliError> {
        let db = fixture()?;
        let error = execute_args(
            &db,
            &[
                "txn",
                "post",
                "--description",
                "Bad",
                "--debit",
                "1000:10",
                "--credit",
                "4000:9",
            ],
        );
        assert!(matches!(error, Err(CliError::Validation(_))));
        let count = execute_args(&db, &["txn", "list", "--count"])?;
        assert_eq!(count["count"], 0);
        Ok(())
    }

    #[test]
    fn shared_clear_records_explicit_actor_and_show_breakdown() -> Result<(), CliError> {
        let db = fixture()?;
        let posted = execute_args(
            &db,
            &[
                "txn",
                "post",
                "--description",
                "Sale",
                "--debit",
                "1000:10",
                "--credit",
                "4000:10",
                "--date",
                "2026-03-01",
                "--reference",
                "clear-1",
            ],
        )?;
        let cleared = execute_args(
            &db,
            &[
                "txn",
                "clear",
                "--reference",
                "clear-1",
                "--account",
                "1000",
                "--amount",
                "4",
                "--date",
                "2026-03-02",
                "--statement",
                "bank-1",
            ],
        )?;
        assert_eq!(cleared["amount"], 400);
        assert_eq!(cleared["remaining_amount"], 600);
        let actor: String = db.conn().query_row(
            "SELECT actor FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(actor, "mcp:test");
        let id = posted["id"].to_string();
        let shown = execute_args(&db, &["txn", "show", &id])?;
        assert_eq!(shown[0]["entries"][0]["cleared_amount"], 400);
        Ok(())
    }

    #[test]
    fn shared_clear_batch_is_atomic_and_accepts_memory_input() -> Result<(), CliError> {
        let db = fixture()?;
        for reference in ["batch-a", "batch-b"] {
            execute_args(
                &db,
                &[
                    "txn",
                    "post",
                    "--description",
                    "Sale",
                    "--debit",
                    "1000:10",
                    "--credit",
                    "4000:10",
                    "--date",
                    "2026-03-01",
                    "--reference",
                    reference,
                ],
            )?;
        }
        let sub = TxnCommand::Clear {
            transaction_id: None,
            entry: None,
            reference: None,
            account: Some("1000".into()),
            from_file: Some("-".into()),
            amount: None,
            date: Some("2026-03-02".into()),
            statement: Some("batch-statement".into()),
            status: crate::cli::ClearanceArg::Cleared,
        };
        let actor = db::Actor::new("mcp:batch-test");
        let invalid = super::super::txn_clear::parse_batch("batch-a,5\nbatch-b,11")?;
        assert!(
            super::super::txn_clear::execute_json(&db, "acme", &sub, &actor, Some(&invalid),)
                .is_err()
        );
        let cleared: i64 =
            db.conn()
                .query_row("SELECT count(*) FROM entry_clearings", [], |row| row.get(0))?;
        assert_eq!(cleared, 0);
        let valid = super::super::txn_clear::parse_batch("batch-a,5\nbatch-b,6")?;
        let result =
            super::super::txn_clear::execute_json(&db, "acme", &sub, &actor, Some(&valid))?;
        assert_eq!(result["cleared_count"], 2);
        assert_eq!(result["cleared_total"], 1100);
        let actors: i64 = db.conn().query_row(
            "SELECT count(*) FROM audit_log WHERE actor = 'mcp:batch-test'",
            [],
            |row| row.get(0),
        )?;
        assert!(actors >= 2);
        Ok(())
    }

    #[test]
    fn normalize_reference_stores_verbatim() {
        // Regression for issue #12: the user key must be stored as-is, not
        // hashed into a txnref_ token.
        assert_eq!(
            normalize_reference("RENT-2026-03").as_deref(),
            Ok("RENT-2026-03")
        );
    }

    #[test]
    fn normalize_reference_trims_whitespace() {
        assert_eq!(normalize_reference("  INV-001  ").as_deref(), Ok("INV-001"));
    }

    #[test]
    fn normalize_reference_rejects_empty() {
        assert!(normalize_reference("").is_err());
    }

    #[test]
    fn normalize_reference_rejects_whitespace_only() {
        assert!(normalize_reference("   ").is_err());
    }

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
    fn parse_amount_negative_fraction() {
        // Regression: "-12.34" used to come out as -1166 (-1200 + 34).
        assert_eq!(
            parse_amount_to_minor("-12.34", Currency::USD).ok(),
            Some(-1234)
        );
        // Regression: "-0.50" used to lose its sign ("-0" parses as 0).
        assert_eq!(
            parse_amount_to_minor("-0.50", Currency::USD).ok(),
            Some(-50)
        );
        assert_eq!(parse_amount_to_minor("-7", Currency::USD).ok(), Some(-700));
        assert_eq!(parse_amount_to_minor("+7.5", Currency::USD).ok(), Some(750));
    }

    #[test]
    fn parse_amount_rejects_malformed_input() {
        for bad in [
            "", "-", ".5", "1.-5", "1.+5", "--1", "1e3", " 1", "1,000", "1.2.3", "abc",
        ] {
            assert!(
                parse_amount_to_minor(bad, Currency::USD).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // JPY has no minor units: a trailing "." is fine, a fraction is not.
        assert_eq!(parse_amount_to_minor("50.", Currency::JPY).ok(), Some(50));
        assert!(parse_amount_to_minor("50.5", Currency::JPY).is_err());
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
