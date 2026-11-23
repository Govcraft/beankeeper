//! `bk audit` command: inspect the append-only audit trail.

mod receipts;
pub use receipts::{ReceiptRequest, ReceiptResponse};

use crate::operation::Operation;
use beankeeper::types::Currency;
use rusqlite::OptionalExtension as _;
use serde_json::Value;

use super::audit_snapshot::{embedded_currency, project_snapshot};
use crate::cli::{ActorFilterArg, AuditLogArgs, Cli, OutputFormat, resolve_format};
use crate::db::audit::{AuditAction, AuditEntity, AuditPageLimit, AuditPageOffset};
use crate::db::companies::CompanySlug;
use crate::db::connection::ReadConn;
use crate::db::{ActorLabel, AuditLogRow, Db, ListAuditParams, count_audit, list_audit};
use crate::error::CliError;
use crate::output;

pub(crate) fn show_hint_notice(notice: &str) {
    use std::io::Write as _;

    let _ = writeln!(std::io::stderr().lock(), "{notice}");
}

/// Filters and paging for one audit history snapshot.
#[derive(Debug, Default)]
pub struct AuditQuery {
    pub(crate) actor: Option<ActorLabel>,
    pub(crate) agent: Option<String>,
    pub(crate) company: Option<CompanySlug>,
    pub(crate) entity: Option<AuditEntity>,
    pub(crate) action: Option<AuditAction>,
    pub(crate) limit: AuditPageLimit,
    pub(crate) offset: AuditPageOffset,
}

impl TryFrom<&AuditLogArgs> for AuditQuery {
    type Error = CliError;

    fn try_from(args: &AuditLogArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            actor: ActorFilterArg::resolve_optional(args.by_actor.as_ref())?.cloned(),
            agent: args.agent.clone(),
            company: args.company.clone(),
            entity: args.entity,
            action: args.action,
            limit: args.limit.unwrap_or_default(),
            offset: args.offset.unwrap_or_default(),
        })
    }
}

impl AuditQuery {
    /// Read matching rows, money displays, and pagination in one snapshot.
    ///
    /// # Errors
    /// Returns database errors if the audit page or related records cannot be read.
    pub(crate) fn execute(self, db: &Db) -> Result<AuditResponse, CliError> {
        db.read_snapshot(move |reader| {
            let params = ListAuditParams {
                actor: self.actor.as_ref(),
                agent_uri: self.agent.as_deref(),
                company_slug: self.company.as_ref(),
                entity: self.entity,
                action: self.action,
                limit: self.limit,
                offset: self.offset,
            };
            let rows = list_audit(reader, &params)?;
            let pagination = output::json::Pagination::new(
                i64::from(params.limit.get()),
                params.offset.into(),
                rows.len(),
                count_audit(reader, &params)?,
            )
            .with_next_offset();
            let rows = rows
                .into_iter()
                .map(|row| AuditEntry::project(reader, row))
                .collect::<Result<_, _>>()?;
            Ok(AuditResponse {
                company: self.company,
                pagination,
                rows,
            })
        })
    }
}

/// Audit rows with the page boundaries obtained in the same snapshot.
pub struct AuditResponse {
    company: Option<CompanySlug>,
    pagination: output::json::Pagination,
    rows: Vec<AuditEntry>,
}

impl AuditResponse {
    /// The requested audit entries.
    #[must_use]
    pub fn data(&self) -> impl serde::Serialize + '_ {
        &self.rows
    }

    /// Pagination for this result.
    #[must_use]
    pub fn pagination(&self) -> &output::json::Pagination {
        &self.pagination
    }

    /// Render the audit page to the command line.
    ///
    /// # Errors
    /// Returns errors serializing or writing the response.
    pub fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let pagination = &self.pagination;
        let rows = &self.rows;
        let format = resolve_format(None, cli);
        match format {
            OutputFormat::Json => {
                let meta = cli
                    .meta(
                        Operation::AuditLog,
                        self.company.as_ref().map(CompanySlug::as_str),
                    )
                    .with_pagination(pagination.clone());
                let envelope = output::json::Envelope {
                    ok: true,
                    meta,
                    data: Some(self.data()),
                    error: None,
                };
                let json = serde_json::to_string(&envelope)
                    .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
                println!("{json}");
            }
            OutputFormat::Csv => {
                println!(
                    "id,changed_at,actor,company,entity,entity_id,action,agent_uri,token_jti,unverified_client_runtime"
                );
                for entry in rows {
                    let r = &entry.row;
                    println!(
                        "{},{},{},{},{},{},{},{},{},{}",
                        r.id,
                        csv_field(&r.changed_at),
                        csv_field(&r.actor),
                        csv_field(
                            r.company_slug
                                .as_ref()
                                .map_or("", crate::db::companies::CompanySlug::as_str)
                        ),
                        csv_field(r.entity.as_str()),
                        csv_field(&r.entity_id),
                        csv_field(r.action.as_str()),
                        csv_field(r.agent_uri.as_deref().unwrap_or("")),
                        csv_field(r.token_jti.as_deref().unwrap_or("")),
                        r.client_hint()
                            .map(|hint| hint.runtime().to_string())
                            .unwrap_or_default(),
                    );
                }
            }
            OutputFormat::Table => {
                if rows.is_empty() {
                    if !cli.verbosity.quiet {
                        print_count(rows.len(), pagination);
                    }
                    return Ok(());
                }
                println!(
                    "{:<20}  {:<14}  {:<12}  {:<14}  {:<10}  COMPANY",
                    "CHANGED AT", "ACTOR", "ENTITY", "ACTION", "ID"
                );
                for entry in rows {
                    let r = &entry.row;
                    println!(
                        "{:<20}  {:<14}  {:<12}  {:<14}  {:<10}  {}",
                        r.changed_at,
                        truncate(&r.actor, 14),
                        r.entity,
                        r.action,
                        truncate(&r.entity_id, 10),
                        r.company_slug
                            .as_ref()
                            .map_or("-", crate::db::companies::CompanySlug::as_str),
                    );
                    if let Some(uri) = &r.agent_uri {
                        println!(
                            "  Agent: {uri}  Token: {}",
                            r.token_jti.as_deref().unwrap_or("unknown")
                        );
                    }
                    if let Some(hint) = r.client_hint() {
                        println!("  Client hint: {} (unverified)", hint.runtime());
                    }
                }
                if !cli.verbosity.quiet {
                    print_count(rows.len(), pagination);
                }
            }
        }

        Ok(())
    }
}

/// Execute `bk audit log`.
///
/// # Errors
/// Returns validation, database or rendering errors.
pub fn run(cli: &Cli, args: &AuditLogArgs) -> Result<(), CliError> {
    AuditQuery::try_from(args)?
        .execute(&cli.open_db()?)?
        .render(cli)
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

struct AuditEntry {
    row: AuditLogRow,
    before_display: Value,
    after_display: Value,
}

impl AuditEntry {
    fn project(reader: ReadConn<'_>, row: AuditLogRow) -> Result<Self, CliError> {
        let [mut before_display, mut after_display] = [&row.before, &row.after].map(|snapshot| {
            snapshot
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or(Value::Null)
        });
        let currency = snapshot_currency(reader, &row, [&after_display, &before_display])?;
        project_snapshot(&mut before_display, row.entity, currency);
        project_snapshot(&mut after_display, row.entity, currency);
        Ok(Self {
            row,
            before_display,
            after_display,
        })
    }
}

impl serde::Serialize for AuditEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let row = &self.row;
        let undo_of = row.undo_of();
        let mut output =
            serializer.serialize_struct("AuditEntry", 17 + usize::from(undo_of.is_some()))?;
        output.serialize_field("id", &row.id)?;
        output.serialize_field("changed_at", &row.changed_at)?;
        output.serialize_field("actor", &row.actor)?;
        output.serialize_field("agent_uri", &row.agent_uri)?;
        output.serialize_field("token_jti", &row.token_jti)?;
        output.serialize_field("client_hint", &row.client_hint())?;
        output.serialize_field("hash_version", &row.hash_version)?;
        output.serialize_field("company_slug", &row.company_slug)?;
        output.serialize_field("entity", &row.entity)?;
        output.serialize_field("entity_id", &row.entity_id)?;
        output.serialize_field("action", &row.action)?;
        if let Some(receipt) = undo_of {
            output.serialize_field("undo_of", &receipt)?;
        }
        output.serialize_field("before", &row.before)?;
        output.serialize_field("after", &row.after)?;
        output.serialize_field("before_display", &self.before_display)?;
        output.serialize_field("after_display", &self.after_display)?;
        output.serialize_field("prev_hash", &row.prev_hash)?;
        output.serialize_field("row_hash", &row.row_hash)?;
        output.end()
    }
}

fn snapshot_currency(
    reader: ReadConn<'_>,
    row: &AuditLogRow,
    snapshots: [&Value; 2],
) -> Result<Option<Currency>, CliError> {
    if let Some(currency) = snapshots.into_iter().find_map(embedded_currency) {
        return Ok(Some(currency));
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
    let currency = reader
        .query_row(
            sql,
            rusqlite::params![row.entity_id, row.company_slug],
            |r| r.get(0),
        )
        .optional()?;
    Ok(currency)
}
