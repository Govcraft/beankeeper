use std::{fs::OpenOptions, path::Path};

use beankeeper_cli::disbursement::DisbursementId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use super::{Environment, Error, Order, Reservation, Result};

pub(super) struct Store(Connection);

impl Store {
    pub(super) fn open(path: &Path, environment: Environment) -> Result<Self> {
        create_private_file(path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|_| Error::Storage)?;
        Self::initialize(conn, environment)
    }

    fn initialize(conn: Connection, environment: Environment) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS wise_bridge_environment (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                environment TEXT NOT NULL CHECK(environment IN ('sandbox','live'))
             ) STRICT;
             CREATE TABLE IF NOT EXISTS wise_orders (
                key TEXT PRIMARY KEY,
                payload TEXT NOT NULL CHECK(json_valid(payload)),
                CHECK(key IS json_extract(payload,'$.payment.key'))
             ) STRICT;
             CREATE TRIGGER IF NOT EXISTS wise_orders_no_delete BEFORE DELETE ON wise_orders
             BEGIN SELECT RAISE(ABORT,'cannot delete a payment key'); END;
             CREATE TRIGGER IF NOT EXISTS wise_orders_protect BEFORE UPDATE ON wise_orders
             WHEN OLD.key IS NOT NEW.key
               OR json_extract(OLD.payload,'$.environment') IS NOT json_extract(NEW.payload,'$.environment')
               OR json_extract(OLD.payload,'$.payment') IS NOT json_extract(NEW.payload,'$.payment')
               OR json_extract(OLD.payload,'$.profile') IS NOT json_extract(NEW.payload,'$.profile')
               OR json_extract(OLD.payload,'$.recipient') IS NOT json_extract(NEW.payload,'$.recipient')
               OR json_extract(OLD.payload,'$.balance') IS NOT json_extract(NEW.payload,'$.balance')
               OR json_extract(OLD.payload,'$.rejected_on') IS NOT json_extract(NEW.payload,'$.rejected_on')
               OR json_extract(OLD.payload,'$.quote') IS NOT json_extract(NEW.payload,'$.quote')
               OR json_extract(OLD.payload,'$.created') IS NOT json_extract(NEW.payload,'$.created')
               OR (json_extract(OLD.payload,'$.funding_attempted')=1 AND json_extract(NEW.payload,'$.funding_attempted') IS NOT 1)
               OR (json_extract(OLD.payload,'$.funding_receipt') IS NOT NULL
                  AND json_extract(OLD.payload,'$.funding_receipt') IS NOT json_extract(NEW.payload,'$.funding_receipt'))
               OR (json_extract(OLD.payload,'$.transfer') IS NOT NULL
                  AND json_extract(OLD.payload,'$.transfer') IS NOT json_extract(NEW.payload,'$.transfer'))
             BEGIN SELECT RAISE(ABORT,'cannot change a payment identity or repeat funding'); END;"
        ).map_err(|_| Error::Storage)?;
        conn.execute(
            "INSERT OR IGNORE INTO wise_bridge_environment(singleton,environment) VALUES(1,?1)",
            [environment.name()],
        )
        .map_err(|_| Error::Storage)?;
        let correct_environment: bool = conn
            .query_row(
                "SELECT environment IS ?1 FROM wise_bridge_environment WHERE singleton=1",
                [environment.name()],
                |row| row.get(0),
            )
            .map_err(|_| Error::Storage)?;
        if !correct_environment {
            return Err(Error::Configuration);
        }
        Ok(Self(conn))
    }

    pub(super) fn read(&self, id: &DisbursementId) -> Result<Option<Reservation>> {
        let row = self
            .0
            .query_row(
                "SELECT payload FROM wise_orders WHERE key=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| Error::Storage)?;
        row.as_deref()
            .map(|value| serde_json::from_str(value).map_err(|_| Error::Storage))
            .transpose()
    }

    pub(super) fn insert(&self, key: &DisbursementId, row: &impl Serialize) -> Result<()> {
        let payload = serde_json::to_string(row).map_err(|_| Error::Storage)?;
        self.0
            .execute(
                "INSERT INTO wise_orders VALUES(?1,?2)",
                params![key, payload],
            )
            .map_err(|_| Error::Storage)?;
        Ok(())
    }

    pub(super) fn save(&self, order: &Order) -> Result<()> {
        let payload = serde_json::to_string(order).map_err(|_| Error::Storage)?;
        if self
            .0
            .execute(
                "UPDATE wise_orders SET payload=?2 WHERE key=?1",
                params![order.payment.key, payload],
            )
            .map_err(|_| Error::Storage)?
            != 1
        {
            return Err(Error::Storage);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(Error::Configuration)?;
    let directory = std::fs::symlink_metadata(parent).map_err(|_| Error::Configuration)?;
    if !directory.is_dir() || directory.permissions().mode().trailing_zeros() < 6 {
        return Err(Error::Configuration);
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = std::fs::symlink_metadata(path).map_err(|_| Error::Configuration)?;
            if !file.is_file() || file.permissions().mode().trailing_zeros() < 6 {
                return Err(Error::Configuration);
            }
            Ok(())
        }
        Err(_) => Err(Error::Storage),
    }
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<()> {
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| Error::Storage)?;
    Ok(())
}
