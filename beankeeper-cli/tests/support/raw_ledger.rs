//! Independent `SQLite` connections for deliberate fixture mutations.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

pub fn open(path: &Path, passphrase: Option<&str>) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    if let Some(passphrase) = passphrase {
        connection.pragma_update(None, "key", passphrase)?;
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(connection)
}
