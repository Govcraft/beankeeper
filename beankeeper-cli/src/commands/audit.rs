//! `bk audit` command: inspect the append-only audit trail.

use crate::cli::{AuditCommand, Cli, OutputFormat, resolve_format};
use crate::db::{Db, ListAuditParams, list_audit};
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Execute the `audit` command group.
///
/// # Errors
///
/// Returns [`CliError`] on database or rendering failure.
pub fn run(cli: &Cli, sub: &AuditCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open_existing(&cli.db, pp.as_ref())?;
    let format = resolve_format(None, cli);

    if format == OutputFormat::Json {
        let data = execute_json(&db, sub)?;
        let AuditCommand::Log { company, .. } = sub;
        let meta = output::json::meta("audit.log", company.as_deref());
        println!(
            "{}",
            serde_json::json!({"ok": true, "meta": meta, "data": data})
        );
        return Ok(());
    }

    match sub {
        AuditCommand::Log {
            company,
            entity,
            action,
            limit,
        } => run_log(
            cli,
            &db,
            &ListAuditParams {
                company_slug: company.as_deref(),
                entity: entity.map(crate::cli::AuditEntityArg::as_str),
                action: action.map(crate::cli::AuditActionArg::as_str),
                limit: (*limit).max(0),
            },
            format,
        ),
    }
}

fn run_log(
    cli: &Cli,
    db: &Db,
    params: &ListAuditParams<'_>,
    format: OutputFormat,
) -> Result<(), CliError> {
    let rows = list_audit(db.conn(), params)?;

    match format {
        OutputFormat::Json => {
            let meta = output::json::meta("audit.log", params.company_slug);
            let data = audit_data(&rows);
            let rendered = serde_json::json!({ "ok": true, "meta": meta, "data": data });
            let json = serde_json::to_string(&rendered)
                .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
            println!("{json}");
        }
        OutputFormat::Csv => {
            println!("id,changed_at,actor,company,entity,entity_id,action");
            for r in &rows {
                println!(
                    "{},{},{},{},{},{},{}",
                    r.id,
                    csv_field(&r.changed_at),
                    csv_field(&r.actor),
                    csv_field(r.company_slug.as_deref().unwrap_or("")),
                    csv_field(&r.entity),
                    csv_field(&r.entity_id),
                    csv_field(&r.action),
                );
            }
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                if !cli.verbosity.quiet {
                    eprintln!("[ok] no audit entries");
                }
                return Ok(());
            }
            println!(
                "{:<20}  {:<14}  {:<12}  {:<14}  {:<10}  COMPANY",
                "CHANGED AT", "ACTOR", "ENTITY", "ACTION", "ID"
            );
            for r in &rows {
                println!(
                    "{:<20}  {:<14}  {:<12}  {:<14}  {:<10}  {}",
                    r.changed_at,
                    truncate(&r.actor, 14),
                    r.entity,
                    r.action,
                    truncate(&r.entity_id, 10),
                    r.company_slug.as_deref().unwrap_or("-"),
                );
            }
            if !cli.verbosity.quiet {
                eprintln!("[ok] {} audit entries", rows.len());
            }
        }
    }

    Ok(())
}

/// Minimal CSV field escaping: quote when the value contains a comma, quote, or
/// newline, doubling embedded quotes.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Read the audit trail without writing to stdout.
///
/// # Errors
///
/// Returns database errors if the audit trail cannot be read.
pub fn execute_json(db: &Db, sub: &AuditCommand) -> Result<serde_json::Value, CliError> {
    let AuditCommand::Log {
        company,
        entity,
        action,
        limit,
    } = sub;
    let rows = list_audit(
        db.conn(),
        &ListAuditParams {
            company_slug: company.as_deref(),
            entity: entity.map(crate::cli::AuditEntityArg::as_str),
            action: action.map(crate::cli::AuditActionArg::as_str),
            limit: (*limit).max(0),
        },
    )?;
    Ok(serde_json::Value::Array(audit_data(&rows)))
}

fn audit_data(rows: &[crate::db::AuditLogRow]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "changed_at": r.changed_at,
                "actor": r.actor,
                "company_slug": r.company_slug,
                "entity": r.entity,
                "entity_id": r.entity_id,
                "action": r.action,
                "before": r.before,
                "after": r.after,
                "prev_hash": r.prev_hash,
                "row_hash": r.row_hash,
            })
        })
        .collect()
}
