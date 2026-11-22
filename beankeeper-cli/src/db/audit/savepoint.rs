//! Savepoint lifetime for an audited mutation or a nested audited batch.

use rusqlite::{Connection, Params};

use super::{Actor, Creation};
use crate::db::connection::ReadConn;

use crate::error::CliError;

pub(in crate::db) struct AuditedTx<'a> {
    connection: &'a Connection,
    name: &'static str,
    active: bool,
}

impl<'a> AuditedTx<'a> {
    fn begin(connection: &'a Connection, name: &'static str) -> Result<Self, CliError> {
        connection.execute_batch(&format!("SAVEPOINT {name}"))?;
        Ok(Self {
            connection,
            name,
            active: true,
        })
    }

    pub(super) const fn connection(&self) -> &'a Connection {
        self.connection
    }

    pub(in crate::db) const fn read(&self) -> ReadConn<'_> {
        ReadConn::new(self.connection)
    }

    pub(super) fn insert(&self, sql: &str, params: impl Params) -> Result<i64, CliError> {
        self.connection.execute(sql, params)?;
        Ok(self.connection.last_insert_rowid())
    }

    pub(super) fn record_insert(
        &self,
        actor: &Actor,
        company: &crate::db::companies::CompanySlug,
        creation: Creation,
        id: &str,
        data: &serde_json::Value,
    ) -> Result<(), CliError> {
        super::record_insert(self.connection, actor, company, creation, id, data)
    }

    fn rollback(mut self) -> Result<(), CliError> {
        let name = self.name;
        self.connection
            .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"))?;
        self.active = false;
        Ok(())
    }

    fn commit(mut self) -> Result<(), CliError> {
        self.connection
            .execute_batch(&format!("RELEASE {}", self.name))?;
        self.active = false;
        Ok(())
    }
}

impl Drop for AuditedTx<'_> {
    fn drop(&mut self) {
        if self.active {
            let name = self.name;
            // Keep the originating error (including an unwind) authoritative.
            let _ = self
                .connection
                .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
        }
    }
}

pub(in crate::db) fn with_audited_tx<T>(
    connection: &Connection,
    name: &'static str,
    run: impl FnOnce(&AuditedTx<'_>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let transaction = AuditedTx::begin(connection, name)?;
    let value = run(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

pub(in crate::db) fn with_rolled_back_audited_tx<T>(
    connection: &Connection,
    name: &'static str,
    run: impl FnOnce(&AuditedTx<'_>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let transaction = AuditedTx::begin(connection, name)?;
    let value = run(&transaction)?;
    transaction.rollback()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_rolls_back_the_audited_savepoint() -> Result<(), CliError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("CREATE TABLE sample(value INTEGER)")?;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_audited_tx(&connection, "sp_example", |_| -> Result<(), CliError> {
                connection.execute("INSERT INTO sample VALUES (1)", [])?;
                panic!("simulate an unwind during an audited mutation");
            });
        }));

        assert!(result.is_err());
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM sample", [], |row| row.get(0))?;
        assert_eq!(count, 0);
        assert!(connection.is_autocommit());
        Ok(())
    }

    #[test]
    fn commit_error_rolls_back_the_audited_savepoint() -> Result<(), CliError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )?;

        let result = with_audited_tx(&connection, "sp_example", |_| {
            connection.execute("INSERT INTO child VALUES (1)", [])?;
            Ok(())
        });

        assert!(result.is_err());
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM child", [], |row| row.get(0))?;
        assert_eq!(count, 0);
        assert!(connection.is_autocommit());
        Ok(())
    }
}
