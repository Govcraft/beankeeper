//! The schema partition for in-place restore.

use rusqlite::{Connection, config::DbConfig, params, params_from_iter, types::ToSqlOutput};

use super::CompatibleBackup;
use crate::db::audit::savepoint::AuditedTx;
use crate::error::CliError;

macro_rules! table_set {
    ($name:ident { $($kind:ident => $table:literal),+ $(,)? }) => {
        #[derive(Clone, Copy)]
        enum $name { $($kind),+ }

        impl $name {
            const ALL: &[Self] = &[$(Self::$kind),+];

            const fn name(self) -> &'static str {
                match self { $(Self::$kind => $table),+ }
            }
        }
    };
}

table_set!(RestorableTable {
    Companies => "companies",
    Accounts => "accounts",
    Transactions => "transactions",
    Entries => "entries",
    Attachments => "attachments",
    Budgets => "budgets",
    Clearings => "entry_clearings",
    Rules => "categorization_rules",
    Schedules => "schedules",
    ScheduleEntries => "schedule_entries",
    Securities => "securities",
    SecurityPrices => "security_prices",
    Lots => "investment_lots",
    LotReliefs => "lot_reliefs",
    Proposals => "txn_proposals",
    PeriodLocks => "period_locks",
    BookProfiles => "setup_book_profiles",
    SetupRequests => "setup_requests",
    SetupStatementBatches => "setup_statement_batches",
    StandingSettings => "standing_settings",
    TransactionLinks => "transaction_links",
    StatementReconciliations => "statement_reconciliations",
    StatementMembers => "statement_reconciliation_clearings",
    FxAccounts => "fx_accounts",
    FxPostings => "fx_postings",
});

// These tables are outside the mutation API entirely. Sequence rows are
// advanced for copied tables only, so their retired IDs cannot be issued again.
#[cfg(test)]
table_set!(PreservedTable {
    SchemaVersion => "schema_version",
    Sequences => "sqlite_sequence",
    AuditLog => "audit_log",
    IdentityRoot => "identity_root",
    AgentTokens => "agent_tokens",
    AgentRevocations => "agent_revocations",
    LedgerPolicy => "ledger_policy",
    AgentSpendLimits => "agent_spend_limits",
    SetupSessions => "setup_sessions",
});

pub(super) fn replace(tx: &AuditedTx<'_>, backup: &CompatibleBackup) -> Result<(), CliError> {
    let live = tx.connection();
    let source = backup.db.read().connection();
    // Company-scoped limits remain live. Defer their foreign keys while the
    // company rows are replaced, then validate the completed ledger.
    live.pragma_update(None, "defer_foreign_keys", true)?;
    let triggers = SuspendedTriggers::new(live)?;
    for table in RestorableTable::ALL.iter().rev() {
        live.execute(&format!("DELETE FROM {}", table.name()), [])?;
    }
    for table in RestorableTable::ALL {
        table.copy(live, source)?;
    }
    triggers.resume()?;
    Ok(())
}

impl RestorableTable {
    fn copy(self, live: &Connection, source: &Connection) -> Result<(), CliError> {
        let name = self.name();
        // Select columns by name so the backup's physical column order has no
        // bearing on the values inserted under the live schema's constraints.
        let quoted = live.prepare(&format!("SELECT * FROM {name}"))?;
        let columns = quoted.column_names();
        let names = columns
            .iter()
            .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        let column_names = names.join(", ");
        let source_columns = names
            .iter()
            .map(|name| format!("backup.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = vec!["?"; columns.len()].join(", ");
        let mut read = source.prepare(&format!("SELECT {source_columns} FROM {name} backup"))?;
        let mut rows = read.query([])?;
        let mut insert = live.prepare(&format!(
            "INSERT INTO {name} ({column_names}) VALUES ({placeholders})"
        ))?;
        while let Some(row) = rows.next()? {
            let values = (0..columns.len())
                .map(|index| row.get_ref(index).map(ToSqlOutput::Borrowed))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            insert.execute(params_from_iter(&values))?;
        }
        let backup_sequence: Option<i64> = source.query_row(
            "SELECT MAX(seq) FROM sqlite_sequence WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?;
        if let Some(sequence) = backup_sequence {
            let found = live.execute(
                "UPDATE sqlite_sequence SET seq = MAX(seq, ?2) WHERE name = ?1",
                params![name, sequence],
            )?;
            if found == 0 {
                live.execute(
                    "INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)",
                    params![name, sequence],
                )?;
            }
        }
        Ok(())
    }
}

// Posted rows are immutable to normal commands. A restore copies verified
// rows using this connection-local guard. Other connections and the ledger's
// authorization policy retain their settings, even while this guard is live.
struct SuspendedTriggers<'a>(&'a Connection);

impl<'a> SuspendedTriggers<'a> {
    const CONFIG: DbConfig = DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER;

    fn new(connection: &'a Connection) -> Result<Self, CliError> {
        connection.set_db_config(Self::CONFIG, false)?;
        Ok(Self(connection))
    }

    fn resume(&self) -> Result<(), CliError> {
        self.0.set_db_config(Self::CONFIG, true)?;
        Ok(())
    }
}

impl Drop for SuspendedTriggers<'_> {
    fn drop(&mut self) {
        let _ = self.resume();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::db::Db;

    #[test]
    fn every_schema_table_is_classified_exactly_once() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let classified: BTreeSet<_> = RestorableTable::ALL
            .iter()
            .map(|table| table.name().to_owned())
            .chain(
                PreservedTable::ALL
                    .iter()
                    .map(|table| table.name().to_owned()),
            )
            .collect();
        let mut tables = db
            .read()
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?;
        let schema = tables
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            classified.len(),
            RestorableTable::ALL.len() + PreservedTable::ALL.len()
        );
        assert_eq!(classified, schema);
        Ok(())
    }
}
