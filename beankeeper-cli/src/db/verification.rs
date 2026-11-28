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
    schema_version: schema::SchemaVersion,
    status: Status,
    companies: VerifiedAccounting,
    audit_chain: Option<VerifiedAuditChain>,
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
    AccountLabels,
    AccountClassifications,
    Counterparties,
    BankFeed,

    PayrollRuns,
    OpenItems,
    Vat,
    ContractorTax,
    Projects,
    Disbursements,
    TransactionLinks,
    StatementBatches,
    StatementReconciliations,
    FixedAssets,
    ExpenseClaims,
    Inventory,
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
            Self::AccountLabels => "account_labels",
            Self::PayrollRuns => "payroll_runs",
            Self::AccountClassifications => "account_classifications",
            Self::Counterparties => "counterparties",
            Self::BankFeed => "bank_feed",
            Self::OpenItems => "open_items",
            Self::Vat => "vat",
            Self::ContractorTax => "contractor_tax",
            Self::Projects => "projects",
            Self::Disbursements => "disbursements",
            Self::TransactionLinks => "transaction_links",
            Self::StatementBatches => "statement_batches",
            Self::FixedAssets => "fixed_assets",
            Self::StatementReconciliations => "statement_reconciliations",
            Self::ExpenseClaims => "expense_claims",
            Self::Inventory => "inventory",
            Self::ForeignKeys => "foreign_keys",
            Self::SqliteIntegrity => "sqlite_integrity",
            Self::RowCounts => "row_counts",
        }
    }

    fn pass(self, checks: &mut Vec<CheckResult>) {
        checks.push(CheckResult {
            name: self,
            outcome: Outcome::Passed,
        });
    }

    fn skip(self, checks: &mut Vec<CheckResult>) {
        checks.push(CheckResult {
            name: self,
            outcome: Outcome::Skipped,
        });
    }

    fn when(
        self,
        applicable: bool,
        checks: &mut Vec<CheckResult>,
        verify: impl FnOnce() -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        if applicable {
            self.record(verify(), checks)
        } else {
            self.skip(checks);
            Ok(())
        }
    }

    fn record<T>(
        self,
        result: Result<T, CliError>,
        checks: &mut Vec<CheckResult>,
    ) -> Result<T, CliError> {
        let evidence = result
            .map_err(|error| CliError::Database(format!("{} failed: {error}", self.name())))?;
        self.pass(checks);
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

fn verify_commercial_subledgers(
    reader: ReadConn<'_>,
    version: schema::SchemaVersion,
    checks: &mut Vec<CheckResult>,
) -> Result<(), CliError> {
    Check::Counterparties.when(
        version >= schema::COUNTERPARTY_SCHEMA_VERSION,
        checks,
        || audit::counterparties::verify(reader),
    )?;
    if version >= schema::BANK_FEED_SCHEMA_VERSION {
        Check::BankFeed.record(super::bank_feed::verify(reader), checks)?;
    }
    Check::OpenItems.when(version >= schema::DOCUMENT_SCHEMA_VERSION, checks, || {
        audit::documents::verify(reader)
    })?;
    Check::Vat.when(version >= schema::VAT_SCHEMA_VERSION, checks, || {
        audit::tax_codes::verify(reader)
    })?;
    Check::ContractorTax.when(
        version >= schema::CONTRACTOR_TAX_SCHEMA_VERSION,
        checks,
        || audit::contractor_tax::verify(reader),
    )?;
    Check::Projects.when(version >= schema::PROJECT_SCHEMA_VERSION, checks, || {
        audit::projects::verify(reader)
    })?;
    Check::Inventory.when(version >= schema::INVENTORY_SCHEMA_VERSION, checks, || {
        audit::inventory::verify(reader)
    })?;
    Check::Disbursements.when(
        version >= schema::DISBURSEMENT_SCHEMA_VERSION,
        checks,
        || audit::disbursements::verify(reader),
    )?;
    Ok(())
}

impl VerifiedLedger {
    /// Verify the entire ledger using a single consistent read snapshot.
    ///
    /// # Errors
    /// Returns a database error naming the first failing check and its source.
    pub fn read(db: &Db) -> Result<Self, CliError> {
        db.read_snapshot(|reader| {
            let version = schema::stored_schema(reader)?;
            Self::verify_at(reader.at_schema(version), version, |_| Ok(()))
        })
    }

    pub(super) fn verify_at(
        reader: ReadConn<'_>,
        version: schema::SchemaVersion,
        authorize: impl FnOnce(Option<&policy::LedgerPolicy>) -> Result<(), CliError>,
    ) -> Result<Self, CliError> {
        let mut checks = Vec::new();
        Check::SchemaVersion.pass(&mut checks);
        let audit_chain = if version >= schema::HASH_CHAIN_SCHEMA_VERSION {
            Some(
                Check::AuditHashChain
                    .record(audit::verify_hash_chain_at(reader, version), &mut checks)?,
            )
        } else {
            Check::AuditHashChain.skip(&mut checks);
            None
        };
        let policy = if version >= schema::POLICY_SCHEMA_VERSION {
            Some(Check::LedgerPolicy.record(policy::get(reader), &mut checks)?)
        } else {
            Check::LedgerPolicy.skip(&mut checks);
            None
        };
        authorize(policy.as_ref())?;
        Check::PeriodLocks.when(
            version >= schema::PERIOD_LOCK_SCHEMA_VERSION,
            &mut checks,
            || periods::verify(reader),
        )?;
        let has_proposals = version >= schema::PROPOSAL_SCHEMA_VERSION;
        Check::ProposalResolutions.when(has_proposals, &mut checks, || {
            proposals::verify_resolutions(reader)
        })?;
        Check::SpendLimits.when(has_proposals, &mut checks, || spend_limits::verify(reader))?;
        let companies = Check::Accounting.record(verify_accounting(reader), &mut checks)?;
        Check::CreationSnapshots.when(
            version >= schema::HASH_CHAIN_SCHEMA_VERSION,
            &mut checks,
            || audit::verify_creations_at(reader, version),
        )?;
        if version >= schema::RECEIPT_SCHEMA_VERSION {
            Check::AccountArchives.record(audit::receipts::verify_archives(reader), &mut checks)?;
        }
        if version >= schema::ACCOUNT_RENAME_SCHEMA_VERSION {
            Check::AccountLabels.record(audit::account_rename::verify(reader), &mut checks)?;
        }
        if version >= schema::CASH_CLASS_SCHEMA_VERSION {
            Check::AccountClassifications
                .record(audit::cash_classification::verify(reader), &mut checks)?;
        }
        verify_commercial_subledgers(reader, version, &mut checks)?;
        Check::TransactionLinks.when(
            version >= schema::NORMALIZED_METADATA_SCHEMA_VERSION,
            &mut checks,
            || {
                verify_links_at(reader, has_proposals)?;
                if version >= schema::TXN_LINK_SCHEMA_VERSION {
                    audit::txn_links::verify(reader)?;
                }
                Ok(())
            },
        )?;
        Check::StatementBatches.when(
            version >= schema::STATEMENT_BATCH_SCHEMA_VERSION,
            &mut checks,
            || audit::setup::statement_batches::verify(reader),
        )?;
        if version >= schema::STATEMENT_RECONCILIATION_SCHEMA_VERSION {
            Check::StatementReconciliations
                .record(audit::statement::verify(reader), &mut checks)?;
        }
        if version >= schema::PAYROLL_SCHEMA_VERSION {
            Check::PayrollRuns.record(audit::payroll::verify(reader), &mut checks)?;
        }
        if version >= schema::FIXED_ASSETS_SCHEMA_VERSION {
            Check::FixedAssets.record(super::fixed_assets::verify(reader), &mut checks)?;
        }
        if version >= schema::EXPENSE_CLAIMS_SCHEMA_VERSION {
            Check::ExpenseClaims.record(super::expense_claims::verify(reader), &mut checks)?;
        }
        Check::ForeignKeys.record(verify_foreign_keys(reader), &mut checks)?;
        Check::SqliteIntegrity.record(verify_integrity(reader), &mut checks)?;
        let row_counts = Check::RowCounts.record(
            row_counts(reader, version, &companies, audit_chain.as_ref()),
            &mut checks,
        )?;

        Ok(Self {
            schema_version: version,
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
    pub const fn schema_version(&self) -> schema::SchemaVersion {
        self.schema_version
    }

    pub(super) fn audit_head(&self) -> Option<(super::ids::AuditRowId, &str)> {
        self.audit_chain.as_ref().and_then(VerifiedAuditChain::head)
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
        if let Some(audit_chain) = &self.audit_chain {
            audit_chain.write_summary(output)?;
        }
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
    version: schema::SchemaVersion,
    companies: &VerifiedAccounting,
    audit_chain: Option<&VerifiedAuditChain>,
) -> Result<RowCounts, CliError> {
    let audit_rows = match audit_chain {
        Some(chain) => chain.length(),
        None if version >= schema::AUDIT_LOG_SCHEMA_VERSION => {
            let count = reader.query_row("SELECT COUNT(*) FROM audit_log", [], |row| {
                row.get::<_, i64>(0)
            })?;
            u64::try_from(count)
                .map_err(|error| CliError::Database(format!("invalid audit row count: {error}")))?
        }
        None => 0,
    };
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
                audit_rows,
            })
        },
    )?)
}

pub(super) fn verify_foreign_keys(reader: ReadConn<'_>) -> Result<(), CliError> {
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
    verify_links_at(
        conn,
        conn.schema_version()? >= schema::PROPOSAL_SCHEMA_VERSION,
    )
}

fn verify_links_at(conn: ReadConn<'_>, has_proposals: bool) -> Result<(), CliError> {
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
            if value.is_null()
                || kind == LinkKey::Settlement
                || (!has_proposals && kind == LinkKey::Proposal)
            {
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
