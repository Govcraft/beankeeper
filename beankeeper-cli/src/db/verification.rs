//! Verification of persisted ledger invariants, independent of CLI input.

use std::io::{self, Write};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::audit::{self, VerifiedAuditChain};
use super::connection::{Db, ReadConn};
use super::{periods, policy, proposals, schema, spend_limits};
use crate::error::CliError;

mod accounting;
pub use accounting::{VerifiedAccounting, verify_accounting};

/// Evidence from one ledger snapshot. Only a completed verification can produce
/// this value, which both the CLI and MCP serialize directly.
///
/// ```compile_fail
/// use beankeeper_cli::db::verification::VerifiedLedger;
///
/// let forged: Result<VerifiedLedger, _> = serde_json::from_str(r#"{"status":"healthy"}"#);
/// ```
#[derive(Debug, Serialize)]
pub struct VerifiedLedger {
    schema_version: i64,
    status: Status,
    companies: VerifiedAccounting,
    audit_chain: VerifiedAuditChain,
    row_counts: RowCounts,
    checks: Vec<CheckResult>,
    desktop_view: DesktopView,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Healthy,
}

#[derive(Debug, Serialize)]
struct RowCounts {
    companies: usize,
    accounts: i64,
    transactions: i64,
    entries: i64,
    audit_rows: u64,
}

#[derive(Debug, Serialize)]
struct DesktopView {
    link: DesktopLink,
}

#[derive(Debug, Serialize)]
#[serde(tag = "view_id", rename_all = "snake_case")]
enum DesktopLink {
    Review,
}

#[derive(Debug, Clone, Copy)]
enum Check {
    SchemaVersion,
    AuditHashChain,
    LedgerPolicy,
    PeriodLocks,
    ProposalResolutions,
    SpendLimits,
    Accounting,
    CreationSnapshots,
    AccountArchives,
    TransactionLinks,
    StatementBatches,
    ForeignKeys,
    SqliteIntegrity,
    RowCounts,
}

impl Check {
    const fn name(self) -> &'static str {
        match self {
            Self::SchemaVersion => "schema_version",
            Self::AuditHashChain => "audit_hash_chain",
            Self::LedgerPolicy => "ledger_policy",
            Self::PeriodLocks => "period_locks",
            Self::ProposalResolutions => "proposal_resolutions",
            Self::SpendLimits => "spend_limits",
            Self::Accounting => "accounting",
            Self::CreationSnapshots => "creation_snapshots",
            Self::AccountArchives => "account_archives",
            Self::TransactionLinks => "transaction_links",
            Self::StatementBatches => "statement_batches",
            Self::ForeignKeys => "foreign_keys",
            Self::SqliteIntegrity => "sqlite_integrity",
            Self::RowCounts => "row_counts",
        }
    }

    fn record<T>(
        self,
        result: Result<T, CliError>,
        checks: &mut Vec<CheckResult>,
    ) -> Result<T, CliError> {
        let evidence = result
            .map_err(|error| CliError::Database(format!("{} failed: {error}", self.name())))?;
        checks.push(CheckResult {
            name: self,
            outcome: Outcome::Passed,
        });
        Ok(evidence)
    }
}

impl Serialize for Check {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: Check,
    outcome: Outcome,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Passed,
    Skipped,
}

impl VerifiedLedger {
    /// Verify the entire ledger using a single consistent read snapshot.
    ///
    /// # Errors
    /// Returns a database error naming the first failing check and its source.
    pub fn read(db: &Db) -> Result<Self, CliError> {
        db.read_snapshot(Self::from_snapshot)
    }

    fn from_snapshot(reader: ReadConn<'_>) -> Result<Self, CliError> {
        let mut checks = Vec::new();
        let schema_version =
            Check::SchemaVersion.record(schema::get_schema_version(reader), &mut checks)?;
        let audit_chain = Check::AuditHashChain.record(
            audit::verify_hash_chain_at(reader, schema_version),
            &mut checks,
        )?;
        Check::LedgerPolicy.record(policy::get(reader), &mut checks)?;
        Check::PeriodLocks.record(periods::verify(reader), &mut checks)?;
        Check::ProposalResolutions.record(proposals::verify_resolutions(reader), &mut checks)?;
        Check::SpendLimits.record(spend_limits::verify(reader), &mut checks)?;
        let companies = Check::Accounting.record(verify_accounting(reader), &mut checks)?;
        Check::CreationSnapshots.record(audit::verify_creations(reader), &mut checks)?;
        if schema_version >= schema::RECEIPT_SCHEMA_VERSION {
            Check::AccountArchives.record(audit::receipts::verify_archives(reader), &mut checks)?;
        }
        Check::TransactionLinks.record(verify_links(reader), &mut checks)?;
        if schema_version >= schema::STATEMENT_BATCH_SCHEMA_VERSION {
            Check::StatementBatches
                .record(audit::setup::statement_batches::verify(reader), &mut checks)?;
        } else {
            checks.push(CheckResult {
                name: Check::StatementBatches,
                outcome: Outcome::Skipped,
            });
        }
        Check::ForeignKeys.record(verify_foreign_keys(reader), &mut checks)?;
        Check::SqliteIntegrity.record(verify_integrity(reader), &mut checks)?;
        let row_counts =
            Check::RowCounts.record(row_counts(reader, &companies, &audit_chain), &mut checks)?;

        Ok(Self {
            schema_version,
            status: Status::Healthy,
            companies,
            audit_chain,
            row_counts,
            checks,
            desktop_view: DesktopView {
                link: DesktopLink::Review,
            },
        })
    }

    /// Schema version of the verified snapshot.
    #[must_use]
    pub const fn schema_version(&self) -> i64 {
        self.schema_version
    }

    pub(crate) fn write_summary(&self, output: &mut impl Write) -> io::Result<()> {
        let counts = &self.row_counts;
        writeln!(
            output,
            "Schema {} | {} companies, {} accounts, {} transactions, {} entries",
            self.schema_version,
            counts.companies,
            counts.accounts,
            counts.transactions,
            counts.entries,
        )?;
        self.companies.write_summary(output)?;
        self.audit_chain.write_summary(output)?;
        write!(output, "Checks passed:")?;
        for check in &self.checks {
            if matches!(check.outcome, Outcome::Passed) {
                write!(output, " {}", check.name.name())?;
            }
        }
        writeln!(output)?;
        for check in &self.checks {
            if matches!(check.outcome, Outcome::Skipped) {
                writeln!(output, "Skipped on this schema: {}", check.name.name())?;
            }
        }
        Ok(())
    }
}

fn row_counts(
    reader: ReadConn<'_>,
    companies: &VerifiedAccounting,
    audit_chain: &VerifiedAuditChain,
) -> Result<RowCounts, CliError> {
    Ok(reader.query_row(
        "SELECT (SELECT COUNT(*) FROM accounts),
                (SELECT COUNT(*) FROM transactions),
                (SELECT COUNT(*) FROM entries)",
        [],
        |row| {
            Ok(RowCounts {
                companies: companies.company_count(),
                accounts: row.get(0)?,
                transactions: row.get(1)?,
                entries: row.get(2)?,
                audit_rows: audit_chain.length(),
            })
        },
    )?)
}

fn verify_foreign_keys(reader: ReadConn<'_>) -> Result<(), CliError> {
    let first = reader
        .query_row(
            "SELECT \"table\", rowid, parent, fkid
             FROM pragma_foreign_key_check ORDER BY \"table\", rowid, fkid LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((table, row, parent, constraint)) = first {
        let row = row.map_or_else(
            || "row without a SQLite rowid".into(),
            |id| format!("row #{id}"),
        );
        return Err(CliError::Database(format!(
            "{table} {row}: foreign key #{constraint} has no matching {parent} row"
        )));
    }
    Ok(())
}

fn verify_integrity(reader: ReadConn<'_>) -> Result<(), CliError> {
    let integrity: String = reader.query_row(
        "SELECT integrity_check FROM pragma_integrity_check",
        [],
        |row| row.get(0),
    )?;
    if integrity != "ok" {
        return Err(CliError::Database(integrity));
    }
    Ok(())
}

/// Verify that ledger-controlled relationship metadata refers to existing records.
///
/// # Errors
/// Returns a database error identifying an invalid link or a missing target.
pub fn verify_links(conn: ReadConn<'_>) -> Result<(), CliError> {
    use super::metadata::LinkKey;

    let mut transactions = conn.prepare(
        "SELECT id, company_slug, metadata FROM transactions WHERE metadata IS NOT NULL ORDER BY id",
    )?;
    let rows = transactions.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, company, metadata) = row?;
        let Some(fields) = super::metadata::link_fields(&metadata) else {
            continue;
        };
        for (key, value) in fields {
            let Some(kind) = LinkKey::classify(&key) else {
                continue;
            };
            if value.is_null() || kind == LinkKey::Settlement {
                continue;
            }
            let invalid = || {
                CliError::Database(format!(
                    "transaction #{id}: invalid or missing '{key}' target"
                ))
            };
            let found: bool = if kind == LinkKey::Proposal {
                let target = value.as_str().ok_or_else(invalid)?;
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM txn_proposals WHERE id = ?1 AND company_slug = ?2)",
                    params![target, company],
                    |row| row.get(0),
                )?
            } else {
                let target = value.as_i64().ok_or_else(invalid)?;
                conn.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM transactions
                        WHERE id = ?1 AND (?2 OR company_slug = ?3)
                    )",
                    params![target, kind == LinkKey::Correlate, company],
                    |row| row.get(0),
                )?
            };
            if !found {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{companies::create_company, connection::Db};
    use crate::identity::{Admin, Granted};

    #[test]
    fn missing_persisted_relationship_identifies_its_transaction() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        let row = db.conn().query_row(
            "INSERT INTO transactions (company_slug, description, metadata, date)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params!["acme", "Legacy entry", r#"{"reverses":999}"#, "2024-01-15"],
            |row| row.get::<_, i64>(0),
        )?;
        assert!(matches!(
            verify_links(db.read()),
            Err(CliError::Database(message))
                if message == format!("transaction #{row}: invalid or missing 'reverses' target")
        ));
        Ok(())
    }
}
