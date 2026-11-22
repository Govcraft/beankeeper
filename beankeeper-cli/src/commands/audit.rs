//! `bk audit` command: inspect the append-only audit trail.

use crate::operation::Operation;
use beankeeper::types::Currency;
use rusqlite::OptionalExtension as _;

use crate::cli::{AuditCommand, Cli, OutputFormat, resolve_format};
use crate::db::audit::AuditEntity;
use crate::db::{Db, ListAuditParams, count_audit, list_audit};
use crate::error::CliError;
use crate::output;

/// Execute the `audit` command group.
///
/// # Errors
///
/// Returns [`CliError`] on database or rendering failure.
pub fn run(cli: &Cli, sub: &AuditCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let format = resolve_format(None, cli);

    run_log(cli, &db, &list_params(sub), format)
}

fn list_params(sub: &AuditCommand) -> ListAuditParams<'_> {
    let AuditCommand::Log {
        agent,
        company,
        entity,
        action,
        limit,
        offset,
    } = sub;
    ListAuditParams {
        agent_uri: agent.as_deref(),
        company_slug: company.as_deref(),
        entity: *entity,
        action: *action,
        limit: (*limit).max(0),
        offset: (*offset).max(0),
    }
}

fn run_log(
    cli: &Cli,
    db: &Db,
    params: &ListAuditParams<'_>,
    format: OutputFormat,
) -> Result<(), CliError> {
    let rows = list_audit(db.conn(), params)?;
    let pagination = output::json::Pagination::new(
        params.limit,
        params.offset,
        rows.len(),
        count_audit(db.conn(), params)?,
    );

    match format {
        OutputFormat::Json => {
            let meta = cli
                .meta(Operation::AuditLog, params.company_slug)
                .with_pagination(pagination);
            let data = audit_data(db, &rows)?;
            let rendered = serde_json::json!({ "ok": true, "meta": meta, "data": data });
            let json = serde_json::to_string(&rendered)
                .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
            println!("{json}");
        }
        OutputFormat::Csv => {
            println!("id,changed_at,actor,company,entity,entity_id,action,agent_uri,token_jti");
            for r in &rows {
                println!(
                    "{},{},{},{},{},{},{},{},{}",
                    r.id,
                    csv_field(&r.changed_at),
                    csv_field(&r.actor),
                    csv_field(r.company_slug.as_deref().unwrap_or("")),
                    csv_field(r.entity.as_str()),
                    csv_field(&r.entity_id),
                    csv_field(r.action.as_str()),
                    csv_field(r.agent_uri.as_deref().unwrap_or("")),
                    csv_field(r.token_jti.as_deref().unwrap_or("")),
                );
            }
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                if !cli.verbosity.quiet {
                    print_count(rows.len(), &pagination);
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
                if let Some(uri) = &r.agent_uri {
                    println!(
                        "  Agent: {uri}  Token: {}",
                        r.token_jti.as_deref().unwrap_or("unknown")
                    );
                }
            }
            if !cli.verbosity.quiet {
                print_count(rows.len(), &pagination);
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

fn print_count(returned: usize, pagination: &output::json::Pagination) {
    if pagination.has_more {
        eprintln!(
            "[ok] showing {returned} of {} audit entries (use --limit/--offset)",
            pagination.total
        );
    } else if returned == 0 {
        eprintln!("[ok] no audit entries");
    } else {
        eprintln!("[ok] {returned} audit entries");
    }
}

/// Read the audit trail without writing to stdout.
///
/// # Errors
///
/// Returns database errors if the audit trail cannot be read.
pub fn execute_json(db: &Db, sub: &AuditCommand) -> Result<serde_json::Value, CliError> {
    let rows = list_audit(db.conn(), &list_params(sub))?;
    Ok(serde_json::Value::Array(audit_data(db, &rows)?))
}

/// Build pagination metadata for a previously read audit page.
///
/// # Errors
///
/// Returns database errors if matching audit rows cannot be counted.
pub fn pagination(
    db: &Db,
    sub: &AuditCommand,
    returned: usize,
) -> Result<output::json::Pagination, CliError> {
    let params = list_params(sub);
    Ok(output::json::Pagination::new(
        params.limit,
        params.offset,
        returned,
        count_audit(db.conn(), &params)?,
    ))
}

fn audit_data(
    db: &Db,
    rows: &[crate::db::AuditLogRow],
) -> Result<Vec<serde_json::Value>, CliError> {
    rows.iter()
        .map(|r| {
            let currency = snapshot_currency(db, r)?;
            Ok(serde_json::json!({
                "id": r.id,
                "changed_at": r.changed_at,
                "actor": r.actor,
                "agent_uri": r.agent_uri,
                "token_jti": r.token_jti,
                "hash_version": r.hash_version,
                "company_slug": r.company_slug,
                "entity": r.entity,
                "entity_id": r.entity_id,
                "action": r.action,
                "before": r.before,
                "after": r.after,
                "before_display": display_snapshot(r.before.as_deref(), currency),
                "after_display": display_snapshot(r.after.as_deref(), currency),
                "prev_hash": r.prev_hash,
                "row_hash": r.row_hash,
            }))
        })
        .collect()
}

fn display_snapshot(raw: Option<&str>, currency: Option<Currency>) -> serde_json::Value {
    raw.and_then(|raw| serde_json::from_str(raw).ok())
        .map_or(serde_json::Value::Null, |value| {
            output::json::with_amount_displays(value, currency)
        })
}

fn snapshot_currency(db: &Db, row: &crate::db::AuditLogRow) -> Result<Option<Currency>, CliError> {
    for raw in [&row.after, &row.before].into_iter().flatten() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
            && let Some(code) = value.get("currency").and_then(serde_json::Value::as_str)
            && let Ok(currency) = Currency::from_code(code)
        {
            return Ok(Some(currency));
        }
    }
    // Entry and transaction financial fields are immutable, so a lookup is
    // safe for snapshots recorded before currency was included. Mutable or
    // deleted budget rows cannot safely identify a historical currency.
    let sql = match row.entity {
        AuditEntity::Entry => {
            "SELECT t.currency FROM entries e JOIN transactions t ON t.id = e.transaction_id WHERE e.id = ?1 AND e.company_slug = ?2"
        }
        AuditEntity::Transaction => {
            "SELECT currency FROM transactions WHERE id = ?1 AND company_slug = ?2"
        }
        _ => return Ok(None),
    };
    let code: Option<String> = db
        .conn()
        .query_row(
            sql,
            rusqlite::params![row.entity_id, row.company_slug],
            |r| r.get(0),
        )
        .optional()?;
    Ok(code.and_then(|code| Currency::from_code(&code).ok()))
}

#[cfg(test)]
mod tests {
    use super::display_snapshot;
    use serde_json::{Value, json};

    #[test]
    fn legacy_snapshots_with_unknown_currency_keep_minor_units_without_guessing() {
        assert_eq!(
            display_snapshot(Some(r#"{"amount": 1234}"#), None),
            json!({"amount":1234, "amount_display":null})
        );
        assert_eq!(display_snapshot(None, None), Value::Null);
    }
}
