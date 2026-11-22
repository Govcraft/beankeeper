//! SQL decoding for calendar dates stored as text.

use chrono::NaiveDate;
use rusqlite::{
    Row,
    types::{Type, ValueRef},
};

pub(crate) fn read(row: &Row<'_>, column: usize) -> rusqlite::Result<NaiveDate> {
    parse_day(row.get_ref(column)?.as_str()?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

pub(super) fn parse_day(raw: &str) -> Result<NaiveDate, chrono::ParseError> {
    // Older transactions and their migrated clearings can carry a time suffix.
    raw.split_once([' ', 'T'])
        .map_or(raw, |(day, _)| day)
        .parse()
}

pub(super) fn read_optional(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<NaiveDate>> {
    match row.get_ref(column)? {
        ValueRef::Null => Ok(None),
        _ => read(row, column).map(Some),
    }
}
