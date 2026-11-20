//! Per-ledger enforcement policy. Missing or unaudited state fails closed.

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::error::CliError;

/// The singleton ledger policy and its last change time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerPolicy {
    pub enforce: bool,
    pub updated_at: String,
}

/// Read the policy and check it against its most recent audit snapshot.
///
/// # Errors
/// Returns a database error for absent, invalid, or unaudited policy state.
pub fn get(conn: &Connection) -> Result<LedgerPolicy, CliError> {
    let policy = conn
        .query_row(
            "SELECT enforce, updated_at FROM ledger_policy WHERE singleton = 1",
            [],
            |row| {
                Ok(LedgerPolicy {
                    enforce: row.get(0)?,
                    updated_at: row.get(1)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| CliError::Database("ledger policy is missing".into()))?;
    let latest: Option<String> = conn.query_row(
        "SELECT after FROM audit_log WHERE entity = 'policy' AND action = 'policy_change' ORDER BY id DESC LIMIT 1",
        [], |row| row.get(0),
    ).optional()?;
    let valid = match latest {
        Some(snapshot) => {
            serde_json::from_str::<serde_json::Value>(&snapshot).ok()
                == Some(
                    serde_json::json!({"enforce": policy.enforce, "updated_at": policy.updated_at}),
                )
        }
        None => !policy.enforce,
    };
    if !valid {
        return Err(CliError::Database(
            "ledger policy does not match its audit history".into(),
        ));
    }
    Ok(policy)
}
