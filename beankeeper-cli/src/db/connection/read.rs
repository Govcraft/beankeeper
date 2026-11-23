//! A query-only capability for the same SQLite snapshot used by audited writes.

use rusqlite::{CachedStatement, Connection, MappedRows, Params, Result, Row, Rows, Statement};

use super::{ConnectionId, Db};

/// A query-only borrow of a ledger connection.
///
/// Queries share the caller's snapshot. This handle exposes no write or
/// transaction control operations.
///
/// # Examples
///
/// ```compile_fail
/// use beankeeper_cli::db::Db;
/// fn write_through_reader(db: &Db) {
///     db.read().execute("DROP TABLE entries", []);
/// }
/// ```
#[derive(Clone, Copy)]
pub struct ReadConn<'a> {
    connection: &'a Connection,
    id: Option<&'a ConnectionId>,
}

impl<'a> ReadConn<'a> {
    pub(in crate::db) const fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            id: None,
        }
    }

    pub(super) const fn for_db(db: &'a Db) -> Self {
        Self {
            connection: &db.conn,
            id: Some(&db.id),
        }
    }

    pub(in crate::db) const fn connection(self) -> &'a Connection {
        self.connection
    }

    pub(crate) fn belongs_to(self, id: &ConnectionId) -> bool {
        self.id.is_some_and(|current| current.matches(id))
    }

    /// Prepares one query returning rows.
    ///
    /// # Errors
    /// Returns a SQLite error for invalid SQL or a statement that can change state.
    pub fn prepare(self, sql: &str) -> Result<ReadStatement<'a>> {
        require_query(sql)?;
        let statement = self.connection.prepare(sql)?;
        require_readonly(&statement)?;
        Ok(ReadStatement(Query::Direct(statement)))
    }

    /// Prepares a query using the SQLite statement cache.
    ///
    /// # Errors
    /// Returns a SQLite error for invalid SQL or a statement that can change state.
    pub fn prepare_cached(self, sql: &str) -> Result<ReadStatement<'a>> {
        require_query(sql)?;
        let statement = self.connection.prepare_cached(sql)?;
        require_readonly(&statement)?;
        Ok(ReadStatement(Query::Cached(statement)))
    }

    /// Executes a query and maps its first result row.
    ///
    /// # Errors
    /// Returns a SQLite error on invalid SQL, a missing row, or failed decoding.
    pub fn query_row<T, P, F>(self, sql: &str, params: P, map: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.prepare(sql)?.query_row(params, map)
    }
}

fn require_query(mut sql: &str) -> Result<()> {
    // Some PRAGMAs modify connection settings during preparation and SQLite
    // classifies transaction-control statements as readonly. Restrict the
    // statement grammar before invoking SQLite, then let SQLite classify
    // state-changing statements inside a WITH expression.
    loop {
        sql = sql.trim_start();
        if let Some(comment) = sql.strip_prefix("--") {
            sql = comment.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(comment) = sql.strip_prefix("/*") {
            sql = comment.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            break;
        }
    }
    let head = sql.split(|ch: char| !ch.is_ascii_alphabetic()).next();
    if head.is_some_and(|head| {
        ["SELECT", "WITH", "VALUES"]
            .iter()
            .any(|keyword| head.eq_ignore_ascii_case(keyword))
    }) {
        Ok(())
    } else {
        Err(rusqlite::Error::InvalidQuery)
    }
}

fn require_readonly(statement: &Statement<'_>) -> Result<()> {
    if statement.readonly() && statement.column_count() > 0 {
        Ok(())
    } else {
        Err(rusqlite::Error::InvalidQuery)
    }
}

enum Query<'a> {
    Direct(Statement<'a>),
    Cached(CachedStatement<'a>),
}

/// A prepared query that cannot execute any SQLite write.
pub struct ReadStatement<'a>(Query<'a>);

impl<'a> ReadStatement<'a> {
    fn statement(&mut self) -> &mut Statement<'a> {
        match &mut self.0 {
            Query::Direct(statement) => statement,
            Query::Cached(statement) => statement,
        }
    }

    /// Runs this query and returns its rows.
    ///
    /// # Errors
    /// Returns a SQLite error if binding or starting the query fails.
    pub fn query<P: Params>(&mut self, params: P) -> Result<Rows<'_>> {
        self.statement().query(params)
    }

    /// Maps each row of this query.
    ///
    /// # Errors
    /// Returns a SQLite error if binding or starting the query fails.
    pub fn query_map<T, P, F>(&mut self, params: P, map: F) -> Result<MappedRows<'_, F>>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        self.statement().query_map(params, map)
    }

    /// Runs the query and maps its first result row.
    ///
    /// # Errors
    /// Returns a SQLite error on invalid parameters, a missing row, or failed decoding.
    pub fn query_row<T, P, F>(&mut self, params: P, map: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.statement().query_row(params, map)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Db;

    #[test]
    fn read_capability_queries_the_ledger() -> Result<(), crate::error::CliError> {
        let db = Db::open_in_memory()?;
        let read = db.read();
        let schema: i64 = read.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
        assert_eq!(schema, crate::db::schema::CURRENT_VERSION);
        let mut query = read.prepare_cached(
            "/* snapshot */ -- result\n WITH x(v) AS (VALUES (1), (2)) SELECT v FROM x",
        )?;
        let values = query
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(values, [1, 2]);
        Ok(())
    }

    #[test]
    fn read_capability_rejects_modifying_and_connection_control_statements()
    -> Result<(), crate::error::CliError> {
        let db = Db::open_in_memory()?;
        let read = db.read();
        for statement in [
            "PRAGMA locking_mode = EXCLUSIVE",
            "BEGIN",
            "SAVEPOINT extra",
            "WITH candidate(x) AS (VALUES (1)) INSERT INTO schema_version SELECT x FROM candidate RETURNING version",
        ] {
            assert!(
                matches!(read.prepare(statement), Err(rusqlite::Error::InvalidQuery)),
                "{statement}"
            );
            assert!(
                matches!(
                    read.prepare_cached(statement),
                    Err(rusqlite::Error::InvalidQuery)
                ),
                "{statement}"
            );
        }
        let mode: String = db
            .conn()
            .pragma_query_value(None, "locking_mode", |row| row.get(0))?;
        assert_eq!(mode, "normal");
        Ok(())
    }
}
