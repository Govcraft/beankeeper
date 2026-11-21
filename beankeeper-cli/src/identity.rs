//! Local bearer attestations and optional per-ledger capability enforcement.

use std::str::FromStr;
use std::time::Duration;

use agent_uri::{AgentId, AgentUri, CapabilityPath};
use agent_uri_attestation::{
    AttestationClaims, AttestationError, Denylist, Issuer, SigningKey, Verifier, VerifyingKey,
};
use mti::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};
use secrecy::{ExposeSecret, SecretBox};
use serde_json::{Value, json};

use crate::capability::Capability;
use crate::db::Actor;
use crate::error::CliError;
use crate::operation::{Operation, Permission};

/// Identity claims accepted by the ledger's verifier.
#[derive(Debug, Clone)]
pub struct VerifiedAgent {
    uri: String,
    jti: MagicTypeId,
    capabilities: Vec<String>,
}

impl VerifiedAgent {
    /// Canonical agent URI from the signed token.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Unique identifier of the verified token.
    #[must_use]
    pub fn jti(&self) -> &str {
        self.jti.as_str()
    }

    /// Explicit grants from the signed claims, never the mutable registry JSON.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Whether these signed grants allow a CLI operation or MCP tool.
    ///
    /// # Errors
    /// Returns an error for an unknown operation or invalid capability.
    pub fn allows_operation(&self, operation: &str) -> Result<bool, CliError> {
        self.allows_known_operation(operation.parse()?)
    }

    pub(crate) fn allows_known_operation(&self, operation: Operation) -> Result<bool, CliError> {
        match operation.permission() {
            Permission::ProposalList => Ok(self.allows(Capability::Read)?
                || self.allows(Capability::Propose)?
                || self.allows(Capability::Approve)?
                || self.allows(Capability::Admin)?),
            Permission::ProposalReject => {
                Ok(self.allows(Capability::Propose)? || self.allows(Capability::Approve)?)
            }
            permission => self.allows(permission.primary()?),
        }
    }

    /// Whether the signed grants cover this leaf capability.
    ///
    /// # Errors
    /// Returns a validation error if an internal capability path is invalid.
    pub fn allows(&self, capability: Capability) -> Result<bool, CliError> {
        let path = CapabilityPath::parse(capability.path())
            .map_err(|error| CliError::Validation(error.to_string()))?;
        Ok(agent_uri_attestation::capability_covers(
            &self.capabilities,
            &path,
        ))
    }
}

fn unauthorized() -> CliError {
    CliError::Unauthorized("invalid, expired, revoked, or untrusted agent token".into())
}

/// Verify the signature, local issuer, URI binding, validity and live revocations.
///
/// # Errors
/// Returns `UNAUTHORIZED` for rejected tokens and database errors on storage failure.
pub fn verify(conn: &Connection, token: &str) -> Result<VerifiedAgent, CliError> {
    verify_required(conn, token, None)
}

/// Verify optional credentials and enforce the current ledger policy.
/// Navigation always requires a verified `ui/navigate` grant, even with policy off.
///
/// # Errors
/// Returns `UNAUTHORIZED` for missing or rejected credentials, `FORBIDDEN` for
/// an insufficient grant, or database errors for invalid policy/storage state.
pub fn authorize(
    conn: &Connection,
    token: Option<&str>,
    required: Capability,
) -> Result<Option<VerifiedAgent>, CliError> {
    let enforce = crate::db::policy::get(conn)?.enforce || required == Capability::Navigate;
    match token {
        Some(token) => verify_required(conn, token, enforce.then_some(required)).map(Some),
        None if enforce => Err(CliError::Unauthorized(format!(
            "operation requires an agent token for {}",
            required.path()
        ))),
        None => Ok(None),
    }
}

/// Authorize an operation, including the alternate proposal list/withdraw grants.
///
/// # Errors
/// Returns unauthorized, forbidden, policy integrity or database errors.
pub fn authorize_operation(
    conn: &Connection,
    token: Option<&str>,
    operation: &str,
) -> Result<Option<VerifiedAgent>, CliError> {
    authorize_known_operation(conn, token, operation.parse()?)
}

pub(crate) fn authorize_known_operation(
    conn: &Connection,
    token: Option<&str>,
    operation: Operation,
) -> Result<Option<VerifiedAgent>, CliError> {
    let permission = operation.permission();
    let required = permission.primary()?;
    if let Permission::Capability(_) = permission {
        return authorize(conn, token, required);
    }
    let enforce = crate::db::policy::get(conn)?.enforce;
    let identity = match token {
        Some(token) => Some(verify(conn, token)?),
        None if enforce => {
            return Err(CliError::Unauthorized(
                "ledger enforcement requires an agent token".into(),
            ));
        }
        None => None,
    };
    if enforce
        && let Some(identity) = &identity
        && !identity.allows_known_operation(operation)?
    {
        return Err(CliError::Forbidden(format!(
            "operation requires capability {} or ledger/approve",
            required.path()
        )));
    }
    Ok(identity)
}

fn verify_required(
    conn: &Connection,
    token: &str,
    required: Option<Capability>,
) -> Result<VerifiedAgent, CliError> {
    let root: Option<(String, String)> = conn
        .query_row(
            "SELECT trust_root, public_key FROM identity_root WHERE singleton = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (root, key) = root.ok_or_else(unauthorized)?;
    let mut denylist = Denylist::new();
    let mut stmt = conn.prepare("SELECT jti FROM agent_revocations")?;
    for jti in stmt.query_map([], |r| r.get::<_, String>(0))? {
        denylist.insert_token(jti?);
    }
    let mut verifier = Verifier::with_leeway(Duration::ZERO).with_revocation(denylist);
    verifier.add_trusted_root(
        root,
        VerifyingKey::from_base64(&key).map_err(|_| unauthorized())?,
    );
    let claims = verifier.verify(token).map_err(|_| unauthorized())?;
    let uri = AgentUri::parse(&claims.agent_uri).map_err(|_| unauthorized())?;
    let claims = verifier
        .verify_for_uri(token, &uri)
        .map_err(|_| unauthorized())?;
    let registered: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_tokens WHERE jti = ?1 AND agent_uri = ?2 AND agent_key = ?3)",
        params![claims.jti, claims.agent_uri, claims.agent_key], |r| r.get(0),
    )?;
    if !registered {
        return Err(unauthorized());
    }
    if claims
        .capabilities
        .iter()
        .any(|capability| capability == "ledger" || capability == "ui")
    {
        return Err(unauthorized());
    }
    if let Some(required) = required {
        let path = CapabilityPath::parse(required.path())
            .map_err(|error| CliError::Validation(error.to_string()))?;
        verifier
            .verify_for_capability(token, &uri, &path)
            .map_err(|error| match error {
                // The token and its claim scope have already verified above.
                // A requested operation outside that scope is a missing grant.
                AttestationError::InsufficientCapabilities { .. }
                | AttestationError::CapabilityOutsideIdentity { .. } => CliError::Forbidden(
                    format!("operation requires capability {}", required.path()),
                ),
                _ => unauthorized(),
            })?;
    }
    Ok(VerifiedAgent {
        uri: uri.to_string(),
        jti: MagicTypeId::from_str(&claims.jti).map_err(|_| unauthorized())?,
        capabilities: claims.capabilities,
    })
}

/// Create one trust root, preserving any existing key. Private material stays in `SQLCipher`.
///
/// # Errors
/// Rejects unencrypted databases and repeated initialization.
pub fn init_root(conn: &Connection, actor: &Actor) -> Result<Value, CliError> {
    let encrypted = conn
        .query_row("PRAGMA cipher_salt", [], |r| r.get::<_, String>(0))
        .optional()?
        .is_some();
    if !encrypted {
        return Err(CliError::Validation(
            "root keys require an encrypted ledger; initialize with `bk init --encrypt`".into(),
        ));
    }
    crate::db::audit::atomically(conn, || {
        let exists: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM identity_root)", [], |r| {
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
        crate::db::audit::record_identity(
            conn,
            actor,
            crate::db::audit::IdentityEvent::KeyInitialized,
            kid.as_str(),
            &data,
        )?;
        Ok(data)
    })
}

/// Token issuance inputs with explicit grants in one namespace: `ledger/*` or `ui/*`.
pub struct IssueRequest<'a> {
    pub name: &'a str,
    pub capabilities: &'a [String],
    pub ttl: Duration,
    pub public_key: Option<&'a str>,
    pub generate_key: bool,
}

/// Issue a token for a supplied public key or explicitly generate a local keypair.
///
/// # Errors
/// Returns validation errors for invalid keys, capabilities, names or TTLs.
pub fn issue(
    conn: &Connection,
    actor: &Actor,
    request: &IssueRequest<'_>,
) -> Result<Value, CliError> {
    let scope = issuance_scope(request.capabilities)?;
    if request.public_key.is_some() == request.generate_key {
        return Err(CliError::Usage(
            "supply --public-key BASE64 or --generate-key".into(),
        ));
    }
    let agent_id =
        AgentId::try_new(request.name).map_err(|e| CliError::Validation(e.to_string()))?;
    let generated = request.generate_key.then(SigningKey::generate);
    let agent_key = if let Some(key) = &generated {
        key.verifying_key()
    } else {
        VerifyingKey::from_base64(request.public_key.unwrap_or_default()).map_err(|_| {
            CliError::Validation("invalid Ed25519 public key (expected base64)".into())
        })?
    };
    let (root, raw): (String, Vec<u8>) = conn
        .query_row(
            "SELECT trust_root, private_key FROM identity_root WHERE singleton = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            CliError::Validation("initialize the ledger root with `bk key init` first".into())
        })?;
    let raw = SecretBox::new(Box::new(raw));
    let seed = <&[u8; 32]>::try_from(raw.expose_secret().as_slice())
        .map_err(|_| CliError::Database("invalid root key length".into()))?;
    let key =
        SigningKey::from_bytes(seed).map_err(|_| CliError::Database("invalid root key".into()))?;
    let uri = AgentUri::parse(&format!("agent://{root}/{scope}/{agent_id}"))
        .map_err(|e| CliError::Validation(e.to_string()))?;
    let claims = AttestationClaims::builder()
        .jti("token".create_type_id::<V7>().to_string())
        .agent_uri(uri.to_string())
        .agent_key(&agent_key)
        .capabilities(request.capabilities.to_vec())
        .issuer(&root)
        .ttl(request.ttl)
        .build()
        .map_err(|e| CliError::Validation(e.to_string()))?;
    let token = Issuer::new(root, key, request.ttl)
        .issue_claims(&claims)
        .map_err(|e| CliError::Validation(e.to_string()))?;
    let public_data = json!({"agent_uri": claims.agent_uri, "token_jti": claims.jti,
        "agent_key": claims.agent_key, "capabilities": claims.capabilities,
        "issued_at": claims.iat, "expires_at": claims.exp});
    crate::db::audit::atomically(conn, || {
        conn.execute("INSERT INTO agent_tokens (jti, agent_uri, agent_key, capabilities, issued_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![claims.jti, claims.agent_uri, claims.agent_key, serde_json::to_string(&claims.capabilities).map_err(|e| CliError::General(e.to_string()))?, claims.iat.to_rfc3339(), claims.exp.to_rfc3339()])?;
        crate::db::audit::record_identity(
            conn,
            actor,
            crate::db::audit::IdentityEvent::TokenIssued,
            &claims.jti,
            &public_data,
        )
    })?;
    let mut result = public_data;
    result["token"] = json!(token);
    if let Some(key) = generated {
        use std::fmt::Write;
        let seed = SecretBox::new(Box::new(key.to_bytes()));
        let mut hex = String::with_capacity(64);
        for byte in seed.expose_secret() {
            let _ = write!(hex, "{byte:02x}");
        }
        result["private_key"] = json!(hex);
    }
    Ok(result)
}

// The attestation builder and issuer both enforce subject scope. Keep grants
// verbatim and select their namespace instead of weakening that validation.
fn issuance_scope(capabilities: &[String]) -> Result<&'static str, CliError> {
    let mut scope = None;
    for capability in capabilities {
        CapabilityPath::parse(capability)
            .map_err(|error| CliError::Validation(error.to_string()))?;
        let namespace = if capability.starts_with("ledger/") {
            "ledger"
        } else if capability.starts_with("ui/") {
            "ui"
        } else {
            return Err(CliError::Validation(
                "issue explicit ledger/* or ui/* grants; bare namespace grants are not allowed"
                    .into(),
            ));
        };
        if scope.is_some_and(|scope| scope != namespace) {
            return Err(CliError::Validation(
                "ledger/* and ui/* grants require separate tokens to preserve attestation scope"
                    .into(),
            ));
        }
        scope = Some(namespace);
    }
    scope.ok_or_else(|| CliError::Validation("at least one explicit grant is required".into()))
}

/// List issued claims without returning tokens or private keys.
///
/// # Errors
/// Returns database errors if the registry cannot be read.
pub fn list(conn: &Connection) -> Result<Value, CliError> {
    let mut stmt = conn.prepare("SELECT t.jti, t.agent_uri, t.agent_key, t.capabilities, t.issued_at, t.expires_at, r.revoked_at FROM agent_tokens t LEFT JOIN agent_revocations r ON r.jti = t.jti ORDER BY t.issued_at, t.jti")?;
    let rows = stmt.query_map([], |r| Ok(json!({
        "token_jti": r.get::<_, String>(0)?, "agent_uri": r.get::<_, String>(1)?,
        "agent_key": r.get::<_, String>(2)?, "capabilities": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
        "issued_at": r.get::<_, String>(4)?, "expires_at": r.get::<_, String>(5)?,
        "revoked_at": r.get::<_, Option<String>>(6)?,
    })))?;
    Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
}

/// Revoke a token ID or all tokens currently issued for an agent URI. Repeating is a no-op.
///
/// # Errors
/// Returns `NOT_FOUND` if no token matches.
pub fn revoke(conn: &Connection, actor: &Actor, target: &str) -> Result<Value, CliError> {
    crate::db::audit::atomically(conn, || {
        let mut stmt = conn.prepare(
            "SELECT jti FROM agent_tokens WHERE jti = ?1 OR agent_uri = ?1 ORDER BY jti",
        )?;
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
                crate::db::audit::record_identity(
                    conn,
                    actor,
                    crate::db::audit::IdentityEvent::TokenRevoked,
                    id,
                    &json!({"token_jti": id}),
                )?;
                changed += 1;
            }
        }
        Ok(json!({"target": target, "revoked": changed}))
    })
}

/// Parse a positive TTL with an explicit `s`, `m`, `h` or `d` suffix.
///
/// # Errors
/// Returns a validation error for zero, overflow or unknown units.
pub fn parse_ttl(input: &str) -> Result<Duration, CliError> {
    let invalid = || {
        CliError::Validation(
            "TTL must be positive with unit s, m, h or d (e.g. 8h), at most 30d".into(),
        )
    };
    let (digits, unit) = input
        .split_at_checked(input.len().saturating_sub(1))
        .ok_or_else(invalid)?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(invalid()),
    };
    let seconds = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|&n| n > 0 && n <= 30 * 86400)
        .ok_or_else(invalid)?;
    Ok(Duration::from_secs(seconds))
}
