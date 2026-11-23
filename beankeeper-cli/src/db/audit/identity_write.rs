//! Persist the trust root and registry in the same scope as their audit events.

use std::time::Duration;

use agent_uri::AgentId;
use agent_uri_attestation::{SigningKey, VerifyingKey};
use mti::prelude::*;
use rusqlite::params;
use secrecy::{ExposeSecret, SecretBox};
use serde_json::{Value, json};

use super::grant::{Admin, Granted, SetupOwner};
use super::{Actor, IdentityEvent, record_identity, savepoint::AuditedTx};
use crate::capability::Capability;
use crate::error::CliError;
use crate::identity::{
    IssueRequest, IssuedCredential, WriteMode, ledger_encrypted, prepare_credential,
    prepare_setup_credential,
};

/// Create one trust root, preserving any existing key. Private material stays in `SQLCipher`.
///
/// # Errors
/// Rejects unencrypted databases and repeated initialization.
pub fn init_root(grant: Granted<'_, Admin>) -> Result<Value, CliError> {
    grant.mutate("sp_identity_root", |tx, actor| {
        let conn = tx.connection();
        if !ledger_encrypted(conn)? {
            return Err(CliError::Validation(
                "root keys require an encrypted ledger; initialize with `bk init --encrypt`".into(),
            ));
        }
        let reader = tx.read();
        let exists: bool =
            reader.query_row("SELECT EXISTS(SELECT 1 FROM identity_root)", [], |r| {
                r.get(0)
            })?;
        if exists {
            return Err(CliError::Validation(
                "ledger root key already initialized".into(),
            ));
        }
        let kid = "key".create_type_id::<V7>();
        let root = format!(
            "{}.bk.local",
            "ledger"
                .create_type_id::<V7>()
                .to_string()
                .replace('_', "-")
        );
        let key = SigningKey::generate();
        let seed = SecretBox::new(Box::new(key.to_bytes()));
        let public = key.verifying_key().to_base64();
        conn.execute("INSERT INTO identity_root (singleton, kid, trust_root, private_key, public_key) VALUES (1, ?1, ?2, ?3, ?4)",
            params![kid.to_string(), root, seed.expose_secret().as_slice(), public])?;
        let data = json!({"kid": kid.to_string(), "trust_root": root, "public_key": public});
        record_identity(
            conn,
            actor,
            IdentityEvent::KeyInitialized,
            kid.as_str(),
            &data,
        )?;
        Ok(data)
    })
}

/// Issue a credential and return the existing JSON representation.
///
/// Rust callers can use [`issue_credential`] to keep secrets in redacted types.
///
/// # Errors
/// Returns the same issuance errors as [`issue_credential`].
pub fn issue(grant: Granted<'_, Admin>, request: &IssueRequest<'_>) -> Result<Value, CliError> {
    Ok(issue_credential(grant, request)?.response_json())
}

/// Issue a token for a supplied public key or explicitly generate a local keypair.
///
/// # Errors
/// Returns validation errors for invalid keys, capabilities, names or TTLs.
pub fn issue_credential(
    grant: Granted<'_, Admin>,
    request: &IssueRequest<'_>,
) -> Result<IssuedCredential, CliError> {
    issue_credential_with_mode(grant, request, WriteMode::default())
}

/// Issue a credential with the requested signed write mode.
///
/// # Errors
/// Returns validation errors for incompatible grants or invalid issuance inputs.
pub fn issue_credential_with_mode(
    grant: Granted<'_, Admin>,
    request: &IssueRequest<'_>,
    write_mode: WriteMode,
) -> Result<IssuedCredential, CliError> {
    if request
        .capabilities
        .iter()
        .any(|grant| grant == Capability::Setup.path())
    {
        return Err(CliError::Usage(
            "issue setup credentials through `bk setup begin`".into(),
        ));
    }
    grant.mutate("sp_token_issue", |tx, actor| {
        issue_in_scope(tx, actor, request, write_mode)
    })
}

pub(super) fn issue_setup_credential(
    owner: &Granted<'_, SetupOwner>,
    agent_id: &AgentId,
    public_key: &VerifyingKey,
    ttl: Duration,
) -> Result<IssuedCredential, CliError> {
    let tx = owner.audited_tx();
    let credential = prepare_setup_credential(tx.read(), agent_id, public_key, ttl)?;
    record_credential(tx, owner.actor(), credential)
}

fn issue_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    request: &IssueRequest<'_>,
    write_mode: WriteMode,
) -> Result<IssuedCredential, CliError> {
    let credential = prepare_credential(tx.read(), request, write_mode)?;
    record_credential(tx, actor, credential)
}

fn record_credential(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    credential: IssuedCredential,
) -> Result<IssuedCredential, CliError> {
    let conn = tx.connection();
    let capabilities = serde_json::to_string(&credential.capabilities)
        .map_err(|error| CliError::General(error.to_string()))?;
    conn.execute(
        "INSERT INTO agent_tokens (jti, agent_uri, agent_key, capabilities, issued_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            credential.token_jti.as_str(),
            credential.agent_uri.as_str(),
            credential.agent_key,
            capabilities,
            credential.issued_at.to_rfc3339(),
            credential.expires_at.to_rfc3339(),
        ],
    )?;
    record_identity(
        conn,
        actor,
        IdentityEvent::TokenIssued,
        credential.token_jti.as_str(),
        &credential.public_data(),
    )?;
    Ok(credential)
}

/// Revoke a token ID or all tokens currently issued for an agent URI. Repeating is a no-op.
///
/// # Errors
/// Returns `NOT_FOUND` if no token matches.
pub fn revoke(grant: Granted<'_, Admin>, target: &str) -> Result<Value, CliError> {
    grant.mutate("sp_token_revoke", |tx, actor| {
        revoke_in_scope(tx, actor, target)
    })
}

pub(super) fn revoke_setup(owner: &Granted<'_, SetupOwner>, target: &str) -> Result<(), CliError> {
    revoke_in_scope(owner.audited_tx(), owner.actor(), target)?;
    Ok(())
}

fn revoke_in_scope(tx: &AuditedTx<'_>, actor: &Actor, target: &str) -> Result<Value, CliError> {
    let conn = tx.connection();
    let reader = tx.read();
    let mut stmt = reader
        .prepare("SELECT jti FROM agent_tokens WHERE jti = ?1 OR agent_uri = ?1 ORDER BY jti")?;
    let ids = stmt
        .query_map([target], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err(CliError::NotFound("agent URI or token ID not found".into()));
    }
    let mut changed = 0;
    for id in &ids {
        if conn.execute(
            "INSERT INTO agent_revocations (jti) VALUES (?1) ON CONFLICT DO NOTHING",
            [id],
        )? > 0
        {
            record_identity(
                conn,
                actor,
                IdentityEvent::TokenRevoked,
                id,
                &json!({"token_jti": id}),
            )?;
            changed += 1;
        }
    }
    Ok(json!({"target": target, "revoked": changed}))
}
