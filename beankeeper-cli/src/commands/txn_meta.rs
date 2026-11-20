//! Audited annotations on existing transactions, shared by CLI and MCP.
use crate::cli::{Cli, OutputFormat, TxnMetaCommand, resolve_format};
use crate::db::{Actor, audit, connection::Db, metadata};
use crate::error::CliError;

fn execute(
    db: &Db,
    company: &str,
    sub: &TxnMetaCommand,
    actor: &Actor,
) -> Result<audit::MetadataResult, CliError> {
    // A single variadic positional list supports both ID + keys and a reference
    // with any number of keys, without clap assigning the first key to the ID.
    let (selector, arguments) = match sub {
        TxnMetaCommand::Set { reference, values } => (reference, values.as_slice()),
        TxnMetaCommand::Unset { reference, keys } => (reference, keys.as_slice()),
    };
    let reference = selector
        .as_deref()
        .map(super::txn::normalize_reference)
        .transpose()
        .map_err(|error| CliError::Validation(error.to_string()))?;
    let (target, arguments) = if let Some(reference) = reference.as_deref() {
        (audit::MetadataTarget::Reference(reference), arguments)
    } else {
        let (id, rest) = arguments
            .split_first()
            .ok_or_else(|| CliError::Usage("provide a transaction ID or --reference".into()))?;
        let id = id.parse().map_err(|_| {
            CliError::Usage(
                "expected a transaction ID; use --reference REF to select by reference".into(),
            )
        })?;
        (audit::MetadataTarget::Id(id), rest)
    };
    let edit = match sub {
        TxnMetaCommand::Set { .. } => {
            metadata::MetadataEdit::Set(metadata::parse_meta_args(arguments)?)
        }
        TxnMetaCommand::Unset { .. } => metadata::MetadataEdit::Unset(arguments.to_vec()),
    };
    audit::edit_metadata(db.conn(), actor, company, target, &edit)
}

/// Execute an annotation edit with an explicit actor and no terminal output.
///
/// # Errors
/// Returns lookup, validation or database errors without a partial edit.
pub fn execute_json(
    db: &Db,
    company: &str,
    sub: &TxnMetaCommand,
    actor: &Actor,
) -> Result<serde_json::Value, CliError> {
    serde_json::to_value(execute(db, company, sub, actor)?)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

/// Render an annotation edit in JSON or as a terminal confirmation.
///
/// # Errors
/// Returns the same errors as [`execute_json`].
pub fn run(cli: &Cli, db: &Db, company: &str, sub: &TxnMetaCommand) -> Result<(), CliError> {
    let data = execute_json(db, company, sub, &cli.principal())?;
    if resolve_format(None, cli) == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({"ok": true, "meta": cli.meta(sub.command_name(), Some(company)), "data": data})
        );
    } else if !cli.verbosity.quiet {
        println!(
            "Transaction #{} metadata: {}",
            data["id"],
            data["metadata"].as_str().unwrap_or("{}")
        );
    }
    Ok(())
}
