//! Canonical SQLite decoding for entry directions.

use beankeeper::types::DebitOrCredit;
use rusqlite::{Row, types::Type};

pub(crate) fn read(row: &Row<'_>, index: usize) -> rusqlite::Result<DebitOrCredit> {
    let code: String = row.get(index)?;
    DebitOrCredit::from_code(&code).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, Error};

    #[test]
    fn stored_direction_accepts_only_canonical_codes() -> rusqlite::Result<()> {
        let connection = Connection::open_in_memory()?;
        let query = |code: &str| connection.query_row("SELECT ?1", [code], |row| read(row, 0));

        assert_eq!(query("debit")?, DebitOrCredit::Debit);
        assert_eq!(query("credit")?, DebitOrCredit::Credit);
        for invalid in ["CR", "Debit", "unknown"] {
            assert!(matches!(
                query(invalid),
                Err(Error::FromSqlConversionFailure(0, Type::Text, _))
            ));
        }
        Ok(())
    }
}
