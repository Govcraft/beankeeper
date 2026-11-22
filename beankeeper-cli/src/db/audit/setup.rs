//! Audited lifecycle for credentials limited to the active setup session.

pub(crate) mod books;
mod requests;

use std::num::NonZeroI64;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mti::prelude::MagicTypeId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{Actor, AuditAction, AuditChange, AuditEntity, atomically, record};
use crate::capability::Capability;
use crate::db::Db;
use crate::error::CliError;
use crate::identity::{self, Granted, IssuedCredential, SetupOwner};

const MAX_TTL: Duration = Duration::from_hours(1);

/// The monotonically increasing setup generation of one ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Generation(NonZeroI64);

impl Generation {
    /// Returns the positive generation number.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0.get()
    }
}

impl TryFrom<i64> for Generation {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        NonZeroI64::new(value)
            .filter(|generation| generation.is_positive())
            .map(Self)
            .ok_or_else(|| CliError::Validation("setup generation must be positive".into()))
    }
}

impl<'de> Deserialize<'de> for Generation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        i64::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl FromStr for Generation {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| {
                CliError::Validation("setup generation must be a positive integer".into())
            })?
            .try_into()
    }
}

/// The persisted lifecycle state of a setup generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Requests authenticated by this generation may apply setup operations.
    Active,
    /// The owner completed this generation.
    Finished,
    /// The owner cancelled this generation.
    Revoked,
}

/// Public setup session details, without credentials or key material.
#[derive(Debug, Serialize)]
pub struct Session {
    pub generation: Generation,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    pub token_jti: MagicTypeId,
    pub agent_uri: String,
    pub expires_at: DateTime<Utc>,
    pub state: SessionState,
    pub ended_at: Option<DateTime<Utc>>,
}

/// A newly registered setup generation and its one-time credential.
#[derive(Debug)]
pub struct Started {
    pub session: Session,
    pub credential: IssuedCredential,
}

/// One owner-selected terminal action.
#[derive(Debug, Clone, Copy)]
pub enum End {
    /// Complete setup and deactivate the setup credential.
    Finish,
    /// Cancel setup and deactivate the setup credential.
    Revoke,
}

impl End {
    const fn state(self) -> SessionState {
        match self {
            Self::Finish => SessionState::Finished,
            Self::Revoke => SessionState::Revoked,
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::Finish => "finished",
            Self::Revoke => "revoked",
        }
    }
}

fn require_encryption(conn: &Connection) -> Result<(), CliError> {
    if identity::ledger_encrypted(conn)? {
        Ok(())
    } else {
        Err(CliError::Validation(
            "setup sessions require an encrypted ledger".into(),
        ))
    }
}

/// Issue and register the single active setup credential in one savepoint.
///
/// # Errors
///
/// Rejects a missing or insufficient owner grant, an unencrypted ledger, invalid
/// issuance inputs, or an already-active setup generation.
pub fn begin(
    db: &Db,
    owner: &Actor,
    name: &str,
    public_key: &str,
    ttl: Duration,
) -> Result<Started, CliError> {
    let conn = db.conn();
    atomically(conn, || {
        require_encryption(conn)?;
        let owner = Granted::<SetupOwner>::check(conn, owner)?;
        if ttl.is_zero() || ttl > MAX_TTL {
            return Err(CliError::Validation(
                "setup token lifetime must be positive and at most one hour".into(),
            ));
        }
        let active = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM setup_sessions WHERE state = 'active')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(CliError::Validation(
                "finish or revoke the active setup session before starting another".into(),
            ));
        }

        let credential = identity::issue_setup_credential(&owner, name, public_key, ttl)?;
        let generation = conn
            .query_row(
                "INSERT INTO setup_sessions (token_jti, state) VALUES (?1, 'active')
             RETURNING generation",
                [credential.token_jti.as_str()],
                |row| row.get::<_, i64>(0),
            )?
            .try_into()?;
        let session = find_session(conn, generation)?
            .ok_or_else(|| CliError::Database("new setup session is missing".into()))?;
        record_session(conn, owner.actor(), None, &session)?;
        Ok(Started {
            session,
            credential,
        })
    })
}

/// Complete or revoke a generation and revoke its credential atomically.
///
/// # Errors
///
/// Rejects a missing or insufficient owner grant, unknown generation,
/// conflicting terminal action or database failure.
pub fn end(db: &Db, owner: &Actor, generation: Generation, end: End) -> Result<Session, CliError> {
    let conn = db.conn();
    atomically(conn, || {
        require_encryption(conn)?;
        let owner = Granted::<SetupOwner>::check(conn, owner)?;
        let mut session = find_session(conn, generation)?.ok_or_else(|| {
            CliError::NotFound(format!("setup generation {} not found", generation.get()))
        })?;
        if session.state == end.state() {
            return Ok(session);
        }
        if session.state != SessionState::Active {
            return Err(CliError::Validation("setup session already ended".into()));
        }

        let before = serialize_session(&session)?;
        let ended_at = Utc::now();
        let changed = conn.execute(
            "UPDATE setup_sessions SET state = ?1, ended_at = ?2
             WHERE generation = ?3 AND state = 'active'",
            params![end.code(), ended_at.to_rfc3339(), generation.get()],
        )?;
        if changed != 1 {
            return Err(CliError::Validation(
                "setup session is no longer active".into(),
            ));
        }
        identity::revoke(conn, owner.actor(), session.token_jti.as_str())?;
        session.state = end.state();
        session.ended_at = Some(ended_at);
        record_session(conn, owner.actor(), Some(&before), &session)?;
        Ok(session)
    })
}

/// Proof of a checked setup generation within its audited transaction.
pub struct ActiveSession<'a> {
    connection: &'a Connection,
    session: &'a Session,
    actor: &'a Actor,
}

impl ActiveSession<'_> {
    /// Verified session metadata.
    #[must_use]
    pub const fn session(&self) -> &Session {
        self.session
    }

    /// Principal to use for setup audit events.
    #[must_use]
    pub const fn actor(&self) -> &Actor {
        self.actor
    }
}

/// Bind a live setup credential to its generation for one audited write.
///
/// # Errors
///
/// Rejects an unencrypted ledger, a missing, expired or revoked credential,
/// an insufficient grant, a mismatched generation, or a database failure.
pub fn with_active_session<T>(
    db: &Db,
    actor: &Actor,
    generation: Generation,
    write: impl FnOnce(&ActiveSession<'_>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let conn = db.conn();
    atomically(conn, || {
        require_encryption(conn)?;
        let credential = identity::require_live_capability(conn, actor, Capability::Setup)?;
        let session = find_session(conn, generation)?
            .filter(|session| {
                credential.is_setup_credential()
                    && session.state == SessionState::Active
                    && session.token_jti.as_str() == credential.jti()
            })
            .ok_or_else(|| {
                CliError::Forbidden("setup credential is not active for this generation".into())
            })?;
        write(&ActiveSession {
            connection: conn,
            session: &session,
            actor,
        })
    })
}

fn find_session(conn: &Connection, generation: Generation) -> Result<Option<Session>, CliError> {
    type SessionRow = (String, String, String, String, Option<String>);
    let row: Option<SessionRow> = conn
        .query_row(
            "SELECT s.token_jti, t.agent_uri, t.expires_at, s.state, s.ended_at
             FROM setup_sessions s JOIN agent_tokens t ON t.jti = s.token_jti
             WHERE s.generation = ?1",
            [generation.get()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    row.map(|(jti, agent_uri, expires_at, state, ended_at)| {
        let date = |raw: &str| {
            raw.parse::<DateTime<Utc>>().map_err(|error| {
                CliError::Database(format!("invalid stored setup timestamp: {error}"))
            })
        };
        let state = match state.as_str() {
            "active" => SessionState::Active,
            "finished" => SessionState::Finished,
            "revoked" => SessionState::Revoked,
            _ => return Err(CliError::Database("invalid stored setup state".into())),
        };
        Ok(Session {
            generation,
            token_jti: jti
                .parse()
                .map_err(|_| CliError::Database("invalid stored setup token ID".into()))?,
            agent_uri,
            expires_at: date(&expires_at)?,
            state,
            ended_at: ended_at.as_deref().map(date).transpose()?,
        })
    })
    .transpose()
}

fn serialize_session(session: &Session) -> Result<String, CliError> {
    serde_json::to_string(session)
        .map_err(|error| CliError::General(format!("setup session serialization failed: {error}")))
}

fn record_session(
    conn: &Connection,
    owner: &Actor,
    before: Option<&str>,
    after: &Session,
) -> Result<(), CliError> {
    let entity_id = format!("setup.session.{}", after.generation.get());
    let after = serialize_session(after)?;
    record(
        conn,
        owner,
        AuditChange {
            company_slug: None,
            entity: AuditEntity::Agent,
            entity_id: &entity_id,
            action: if before.is_some() {
                AuditAction::StatusChange
            } else {
                AuditAction::Create
            },
            snapshots: (before, Some(&after)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_uri_attestation::SigningKey;
    use secrecy::{ExposeSecret, SecretString};

    fn owner(db: &Db) -> Result<Actor, CliError> {
        let local = Actor::new("owner");
        identity::init_root(db.conn(), &local)?;
        let credential = identity::issue_credential(
            db.conn(),
            &local,
            &identity::IssueRequest {
                name: "owner",
                capabilities: &[Capability::Admin.path().into()],
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
        )?;
        let authenticated = identity::verify(db.conn(), credential.token().expose_secret())?;
        Ok(local.with_identity(authenticated))
    }

    #[test]
    fn setup_requires_the_live_owner_and_the_selected_active_generation() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let passphrase: SecretString = "setup-test-passphrase".into();
        let db = Db::open(&directory.path().join("setup.db"), Some(&passphrase))?;
        let owner = owner(&db)?;
        let plain = Db::open(&directory.path().join("plain.db"), None)?;
        let public_key = SigningKey::generate().verifying_key().to_base64();
        let ttl = Duration::from_mins(10);

        assert!(matches!(
            begin(&plain, &owner, "setup", &public_key, ttl),
            Err(CliError::Validation(message)) if message == "setup sessions require an encrypted ledger"
        ));
        let unnamed = Actor::new("owner");
        assert!(matches!(
            begin(&db, &unnamed, "setup", &public_key, ttl),
            Err(CliError::Unauthorized(_))
        ));
        let setup = begin(&db, &owner, "setup", &public_key, ttl)?;
        assert_eq!(setup.credential.capabilities, [Capability::Setup.path()]);
        assert!(matches!(
            begin(&db, &owner, "another", &public_key, ttl),
            Err(CliError::Validation(_))
        ));

        let token = setup.credential.token().expose_secret();
        let authenticated = identity::verify(db.conn(), token)?;
        let agent = Actor::new("setup agent").with_identity(authenticated);
        let generation = setup.session.generation;
        let used = with_active_session(&db, &agent, generation, |proof| {
            assert_eq!(
                proof.actor().token_jti(),
                Some(setup.credential.token_jti.as_str())
            );
            Ok(proof.session().generation)
        })?;
        assert_eq!(used, generation);
        assert!(matches!(
            with_active_session(&db, &agent, (generation.get() + 1).try_into()?, |_| Ok(())),
            Err(CliError::Forbidden(_))
        ));
        let other = Db::open(&directory.path().join("other.db"), Some(&passphrase))?;
        assert!(matches!(
            with_active_session(&other, &agent, generation, |_| Ok(())),
            Err(CliError::Unauthorized(_))
        ));

        for enforce in [false, true] {
            if enforce {
                super::super::set_policy(db.conn(), &owner, true)?;
            }
            assert!(identity::authorize_operation(db.conn(), Some(token), "setup.status").is_ok());
            for operation in ["txn.post", "account.create", "agent.issue", "setup.finish"] {
                assert!(matches!(
                    identity::authorize_operation(db.conn(), Some(token), operation),
                    Err(CliError::Forbidden(_))
                ));
            }
        }

        let ended = end(&db, &owner, generation, End::Finish)?;
        assert_eq!(ended.state, SessionState::Finished);
        assert!(matches!(
            with_active_session(&db, &agent, generation, |_| Ok(())),
            Err(CliError::Unauthorized(_))
        ));
        let retry = end(&db, &owner, generation, End::Finish)?;
        assert_eq!(retry.ended_at, ended.ended_at);
        assert!(matches!(
            end(&db, &owner, generation, End::Revoke),
            Err(CliError::Validation(_))
        ));
        let next = begin(&db, &owner, "setup", &public_key, ttl)?;
        assert!(next.session.generation.get() > generation.get());
        let revoked = end(&db, &owner, next.session.generation, End::Revoke)?;
        assert_eq!(revoked.state, SessionState::Revoked);
        let mut audit = db.conn().prepare(
            "SELECT action, after FROM audit_log
             WHERE entity = 'agent' AND entity_id = ?1 ORDER BY id",
        )?;
        let first_session_events = audit
            .query_map([format!("setup.session.{}", generation.get())], |row| {
                Ok((row.get::<_, AuditAction>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(first_session_events.len(), 2);
        assert_eq!(first_session_events[0].0, AuditAction::Create);
        assert_eq!(first_session_events[1].0, AuditAction::StatusChange);
        assert!(
            first_session_events
                .iter()
                .all(|(_, after)| !after.contains(token))
        );
        super::super::verify_hash_chain(db.read())
    }
}
