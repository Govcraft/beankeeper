use std::io::Write;
use std::path::Path;

use crate::cli::{Cli, ExportFormat};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run the `bk export` command.
///
/// Exports all companies, accounts, and transactions from the database.
/// Output goes to a file if `--output` is specified, otherwise to stdout.
///
/// # Errors
///
/// Returns [`CliError`] if export fails.
pub fn run(
    cli: &Cli,
    format: Option<ExportFormat>,
    output_path: Option<&Path>,
) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;

    let export_format = format.unwrap_or(ExportFormat::Json);

    let rendered = match export_format {
        ExportFormat::Json => export_json(&db)?,
        ExportFormat::Csv => export_csv(&db)?,
    };

    if let Some(path) = output_path {
        let mut file = std::fs::File::create(path)?;
        file.write_all(rendered.as_bytes())?;
        if !cli.verbosity.quiet {
            eprintln!("[ok] exported to {}", path.display());
        }
    } else {
        print!("{rendered}");
        if !cli.verbosity.quiet {
            eprintln!("[ok] export complete");
        }
    }

    Ok(())
}

/// Export all data as JSON.
fn export_json(db: &Db) -> Result<String, CliError> {
    let companies = db::list_companies(db.conn())?;

    let mut company_exports = Vec::new();

    for company in &companies {
        let accounts = db::list_accounts(
            db.conn(),
            &db::accounts::ListAccountParams {
                company_slug: &company.slug,
                type_filter: None,
                name_filter: None,
            },
        )?;
        let mut txn_params = db::ListTransactionParams::for_company(&company.slug);
        txn_params.limit = i64::MAX;
        let transactions = db::list_transactions(db.conn(), &txn_params)?;

        let mut txn_exports = Vec::new();
        for txn in &transactions {
            let entries = db::get_entries_for_transaction(db.conn(), txn.id)?;
            let entry_jsons: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "account_code": e.account_code,
                        "direction": e.direction,
                        "amount": e.amount,
                    })
                })
                .collect();

            txn_exports.push(serde_json::json!({
                "id": txn.id,
                "description": txn.description,
                "metadata": txn.metadata,
                "currency": txn.currency,
                "date": txn.date,
                "posted_at": txn.posted_at,
                "entries": entry_jsons,
            }));
        }

        let account_jsons: Vec<serde_json::Value> = accounts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "code": a.code,
                    "name": a.name,
                    "type": a.account_type,
                })
            })
            .collect();

        company_exports.push(serde_json::json!({
            "slug": company.slug,
            "name": company.name,
            "created_at": company.created_at,
            "accounts": account_jsons,
            "transactions": txn_exports,
        }));
    }

    let export = serde_json::json!({
        "beankeeper_export": {
            "version": 1,
            "companies": company_exports,
        }
    });

    serde_json::to_string_pretty(&export)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Export all data as CSV.
///
/// CSV export outputs three sections separated by blank lines:
/// 1. Companies
/// 2. Accounts
/// 3. Transactions with entries
fn export_csv(db: &Db) -> Result<String, CliError> {
    let companies = db::list_companies(db.conn())?;

    let mut sections = Vec::new();

    // Section 1: Companies
    let companies_csv = output::csv::render_companies(&companies)?;
    sections.push(format!("# Companies\n{companies_csv}"));

    // Section 2: Accounts (all companies)
    let mut all_accounts = Vec::new();
    for company in &companies {
        let accounts = db::list_accounts(
            db.conn(),
            &db::accounts::ListAccountParams {
                company_slug: &company.slug,
                type_filter: None,
                name_filter: None,
            },
        )?;
        all_accounts.extend(accounts);
    }

    let mut acct_wtr = csv::Writer::from_writer(Vec::new());
    acct_wtr
        .write_record(["company_slug", "code", "name", "type"])
        .map_err(|e| CliError::General(format!("CSV serialization failed: {e}")))?;
    for a in &all_accounts {
        acct_wtr
            .write_record([&a.company_slug, &a.code, &a.name, &a.account_type])
            .map_err(|e| CliError::General(format!("CSV serialization failed: {e}")))?;
    }
    let acct_bytes = acct_wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    let accounts_csv = String::from_utf8(acct_bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))?;
    sections.push(format!("# Accounts\n{accounts_csv}"));

    // Section 3: Transactions with entries
    let mut entry_wtr = csv::Writer::from_writer(Vec::new());
    entry_wtr
        .write_record([
            "company_slug",
            "transaction_id",
            "date",
            "description",
            "metadata",
            "currency",
            "account_code",
            "direction",
            "amount",
        ])
        .map_err(|e| CliError::General(format!("CSV serialization failed: {e}")))?;

    for company in &companies {
        let mut txn_params = db::ListTransactionParams::for_company(&company.slug);
        txn_params.limit = i64::MAX;
        let transactions = db::list_transactions(db.conn(), &txn_params)?;
        for txn in &transactions {
            let entries = db::get_entries_for_transaction(db.conn(), txn.id)?;
            for entry in &entries {
                let txn_id_str = txn.id.to_string();
                let meta = txn.metadata.as_deref().unwrap_or("");
                let amount_str = entry.amount.to_string();
                entry_wtr
                    .write_record([
                        company.slug.as_str(),
                        txn_id_str.as_str(),
                        txn.date.as_str(),
                        txn.description.as_str(),
                        meta,
                        txn.currency.as_str(),
                        entry.account_code.as_str(),
                        entry.direction.as_str(),
                        amount_str.as_str(),
                    ])
                    .map_err(|e| CliError::General(format!("CSV serialization failed: {e}")))?;
            }
        }
    }

    let entry_bytes = entry_wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    let entries_csv = String::from_utf8(entry_bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))?;
    sections.push(format!("# Entries\n{entries_csv}"));

    Ok(sections.join("\n"))
}
