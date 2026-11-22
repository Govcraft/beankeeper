//! Stable setup receipts checked and recorded inside the active session savepoint.

use std::{fmt, str::FromStr};

use beankeeper::types::Sha256Hex;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::{AuditAction, AuditChange, AuditEntity, record};
use super::ActiveSession;
use crate::db::companies::CompanySlug;
use crate::error::CliError;

#[derive(Debug, Clone)]
pub(crate) struct SetupReference(Box<str>);

impl SetupReference {
    pub(super) fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Display for SetupReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(&self.0)
    }
}

impl FromStr for SetupReference {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() || value.len() > 128 {
            return Err(CliError::Validation(
                "setup reference must contain 1 to 128 bytes".into(),
            ));
        }
        Ok(Self(value.into()))
    }
}

pub(super) fn apply_once(
    session: &ActiveSession<'_>,
    reference: &SetupReference,
    company: &CompanySlug,
    input: &Value,
    apply: impl FnOnce() -> Result<Value, CliError>,
) -> Result<Value, CliError> {
    let conn = session.connection;
    let fingerprint = digest(input.to_string());
    let receipt: Option<(String, String)> = conn
        .query_row(
            "SELECT fingerprint, outcome FROM setup_requests WHERE reference = ?1",
            [reference.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((expected, outcome)) = receipt {
        let expected = Sha256Hex::new(expected)
            .map_err(|_| CliError::Database("invalid stored setup request fingerprint".into()))?;
        if expected != fingerprint {
            return Err(CliError::Validation(format!(
                "setup reference '{reference}' belongs to a different request"
            )));
        }
        return serde_json::from_str(&outcome)
            .map_err(|error| CliError::Database(format!("invalid stored setup outcome: {error}")));
    }

    let outcome = apply()?;
    let outcome_json = outcome.to_string();
    let generation = session.session.generation;
    conn.execute(
        "INSERT INTO setup_requests (reference, generation, fingerprint, outcome)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            reference.as_str(),
            generation.get(),
            fingerprint.as_str(),
            outcome_json,
        ],
    )?;
    let id = format!("setup.request.{reference}");
    let snapshot = json!({
        "generation": generation,
        "request_fingerprint": fingerprint.as_str(),
        "outcome_fingerprint": digest(outcome_json).as_str(),
    })
    .to_string();
    record(
        conn,
        session.actor(),
        AuditChange {
            company_slug: Some(company.as_str()),
            entity: AuditEntity::Company,
            entity_id: &id,
            action: AuditAction::Create,
            snapshots: (None, Some(&snapshot)),
        },
    )?;
    Ok(outcome)
}

pub(super) fn digest(value: impl AsRef<[u8]>) -> Sha256Hex {
    Sha256Hex::from_digest(Sha256::digest(value).into())
}
