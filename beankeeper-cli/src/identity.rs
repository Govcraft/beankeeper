//! Local bearer attestations and optional per-ledger capability enforcement.

use std::marker::PhantomData;
use std::str::FromStr;
use std::time::Duration;

use agent_uri::{AgentId, AgentUri, CapabilityPath};
use agent_uri_attestation::{
    AttestationClaims, Denylist, Issuer, SigningKey, Verifier, VerifyingKey,
};
use chrono::{DateTime, Utc};
use mti::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde_json::{Value, json};

use crate::capability::Capability;
use crate::db::Actor;
use crate::error::CliError;
use crate::operation::{Operation, Permission};

/// Identity claims accepted by the ledger's verifier.
#[derive(Debug, Clone)]
pub struct VerifiedAgent {
    uri: AgentUri,
    jti: MagicTypeId,
    grants: Vec<Capability>,
    signed_capabilities: Vec<String>,
    agent_key: String,
    issuer_key: String,
    expires_at: DateTime<Utc>,
}

impl VerifiedAgent {
    /// Canonical agent URI from the signed token.
    #[must_use]
    pub fn uri(&self) -> &str {
        self.uri.as_str()
    }

    /// Unique identifier of the verified token.
    #[must_use]
    pub fn jti(&self) -> &str {
        self.jti.as_str()
    }

    /// Explicit grants from the signed claims, never the mutable registry JSON.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.signed_capabilities
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
            Permission::SetupStatus => {
                Ok(self.allows(Capability::Read)? || self.allows(Capability::Setup)?)
            }
            permission => self.allows(permission.primary()?),
        }
    }

    pub(crate) fn is_setup_credential(&self) -> bool {
        self.signed_capabilities == [Capability::Setup.path()]
    }

    pub(crate) fn enforces_permissions(&self, ledger_enforced: bool) -> bool {
        ledger_enforced || self.is_setup_credential()
    }

    /// Whether the verified identity holds the requested ledger or UI grant.
    ///
    /// # Errors
    /// This method retains its fallible signature for existing callers.
    pub fn allows(&self, capability: Capability) -> Result<bool, CliError> {
        Ok(self.grants.contains(&capability))
    }
}

mod sealed {
    pub trait GrantKind {
        const REQUIRED: crate::capability::Capability;
        const EXPLICIT: bool = false;
    }
}

pub(crate) enum Approve {}

impl sealed::GrantKind for Approve {
    const REQUIRED: Capability = Capability::Approve;
}

/// Authority to change ledger policy and administer the ledger.
pub enum Admin {}

impl sealed::GrantKind for Admin {
    const REQUIRED: Capability = Capability::Admin;
}

pub(crate) enum SetupOwner {}

impl sealed::GrantKind for SetupOwner {
    const REQUIRED: Capability = Capability::Admin;
    const EXPLICIT: bool = true;
}

/// Authorization for a single capability, bound to this ledger and principal.
pub struct Granted<'a, C> {
    conn: &'a Connection,
    actor: &'a Actor,
    capability: PhantomData<C>,
}

impl<'a, C: sealed::GrantKind> Granted<'a, C> {
    /// Run one authorized mutation inside its ledger's savepoint.
    ///
    /// The grant is bound to the current actor and connection. The callback's
    /// result cannot retain the grant.
    ///
    /// # Errors
    /// Returns authorization or storage errors and rolls back callback failures.
    pub fn with<T>(
        conn: &Connection,
        actor: &Actor,
        run: impl for<'grant> FnOnce(Granted<'grant, C>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        crate::db::audit::atomically(conn, || run(Granted::check(conn, actor)?))
    }

    pub(crate) const fn into_parts(self) -> (&'a Connection, &'a Actor) {
        (self.conn, self.actor)
    }

    // Call inside the same audited savepoint as the operation.
    pub(crate) fn check(conn: &'a Connection, actor: &'a Actor) -> Result<Self, CliError> {
        let enforce = crate::db::policy::get(conn)?.enforce || C::EXPLICIT;
        if let Some(agent) = actor.identity() {
            agent.check_live(conn)?;
            if agent.enforces_permissions(enforce) && !agent.allows(C::REQUIRED)? {
                return Err(CliError::Forbidden(format!(
                    "operation requires capability {}",
                    C::REQUIRED.path()
                )));
            }
        } else if enforce {
            return Err(CliError::Unauthorized(
                "ledger enforcement requires an agent token".into(),
            ));
        }
        Ok(Self {
            conn,
            actor,
            capability: PhantomData,
        })
    }

    pub(crate) const fn connection(&self) -> &'a Connection {
        self.conn
    }

    pub(crate) const fn actor(&self) -> &'a Actor {
        self.actor
    }
}

impl VerifiedAgent {
    pub(crate) fn check_live(&self, conn: &Connection) -> Result<(), CliError> {
        if self.expires_at <= Utc::now() {
            return Err(unauthorized());
        }
        let live = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM agent_tokens t
                WHERE t.jti = ?1 AND t.agent_uri = ?2 AND t.agent_key = ?3
                AND NOT EXISTS(SELECT 1 FROM agent_revocations r WHERE r.jti = t.jti)
                AND EXISTS (
                    SELECT 1 FROM identity_root WHERE singleton = 1
                    AND public_key = ?4 AND LOWER(trust_root) = ?5
                )
             )",
            params![
                self.jti(),
                self.uri(),
                self.agent_key,
                self.issuer_key,
                self.uri.trust_root().as_str(),
            ],
            |row| row.get::<_, bool>(0),
        )?;
        if live { Ok(()) } else { Err(unauthorized()) }
    }
}

pub(crate) fn require_live_capability<'a>(
    conn: &Connection,
    actor: &'a Actor,
    required: Capability,
) -> Result<&'a VerifiedAgent, CliError> {
    let identity = actor.identity().ok_or_else(|| {
        CliError::Unauthorized(format!(
            "operation requires an agent token for {}",
            required.path()
        ))
    })?;
    identity.check_live(conn)?;
    if !identity.allows(required)? {
        return Err(CliError::Forbidden(format!(
            "operation requires capability {}",
            required.path()
        )));
    }
    Ok(identity)
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
/// Navigation and setup always require a verified grant.
///
/// # Errors
/// Returns `UNAUTHORIZED` for missing or rejected credentials, `FORBIDDEN` for
/// an insufficient grant, or database errors for invalid policy/storage state.
pub fn authorize(
    conn: &Connection,
    token: Option<&str>,
    required: Capability,
) -> Result<Option<VerifiedAgent>, CliError> {
    let enforce = crate::db::policy::get(conn)?.enforce
        || matches!(required, Capability::Navigate | Capability::Setup);
    match token {
        Some(token) => {
            let identity = verify_required(conn, token, enforce.then_some(required))?;
            if !enforce && identity.is_setup_credential() && !identity.allows(required)? {
                return Err(CliError::Forbidden(format!(
                    "operation requires capability {}",
                    required.path()
                )));
            }
            Ok(Some(identity))
        }
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
    if let Permission::Capability(required) = permission {
        return authorize(conn, token, required);
    }
    permission.primary()?;
    if permission == Permission::SetupOwner {
        let token = token.ok_or_else(|| {
            CliError::Unauthorized("setup lifecycle requires a ledger/admin token".into())
        })?;
        return verify_required(conn, token, Some(Capability::Admin)).map(Some);
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
    if let Some(identity) = &identity {
        require_operation_grant(identity, operation, enforce)?;
    }
    Ok(identity)
}

pub(crate) fn authorize_verified_operation(
    conn: &Connection,
    identity: &VerifiedAgent,
    operation: Operation,
) -> Result<(), CliError> {
    let enforce = if operation.permission() == Permission::SetupOwner {
        true
    } else {
        crate::db::policy::get(conn)?.enforce
    };
    identity.check_live(conn)?;
    require_operation_grant(identity, operation, enforce)
}

fn require_operation_grant(
    identity: &VerifiedAgent,
    operation: Operation,
    ledger_enforced: bool,
) -> Result<(), CliError> {
    let permission = operation.permission();
    let required = permission.primary()?;
    let enforce = ledger_enforced
        || permission == Permission::SetupOwner
        || matches!(
            permission,
            Permission::Capability(Capability::Navigate | Capability::Setup)
        );
    if !identity.enforces_permissions(enforce) || identity.allows_known_operation(operation)? {
        return Ok(());
    }
    let alternative = match permission {
        Permission::ProposalList | Permission::ProposalReject => Some(Capability::Approve),
        Permission::SetupStatus => Some(Capability::Setup),
        _ => None,
    };
    let requirement = match alternative {
        Some(alternative) => format!("{} or {}", required.path(), alternative.path()),
        None => required.path().to_owned(),
    };
    Err(CliError::Forbidden(format!(
        "operation requires capability {requirement}"
    )))
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
    // The verifier has bound each signed grant to the URI's capability
    // path or a descendant. Ledger operations require explicit leaf grants,
    // and namespace-wide grants were rejected above.
    let grants: Vec<_> = claims
        .capabilities
        .iter()
        .filter_map(|path| Capability::from_path(path))
        .collect();
    if let Some(required) = required
        && !grants.contains(&required)
    {
        return Err(CliError::Forbidden(format!(
            "operation requires capability {}",
            required.path()
        )));
    }

    Ok(VerifiedAgent {
        uri,
        jti: MagicTypeId::from_str(&claims.jti).map_err(|_| unauthorized())?,
        grants,
        signed_capabilities: claims.capabilities,
        agent_key: claims.agent_key,
        issuer_key: key,
        expires_at: claims.exp,
    })
}

pub(crate) fn ledger_encrypted(conn: &Connection) -> Result<bool, CliError> {
    Ok(conn
        .query_row("PRAGMA cipher_salt", [], |r| r.get::<_, String>(0))
        .optional()?
        .is_some())
}

/// Create one trust root, preserving any existing key. Private material stays in `SQLCipher`.
///
/// # Errors
/// Rejects unencrypted databases and repeated initialization.
pub fn init_root(conn: &Connection, actor: &Actor) -> Result<Value, CliError> {
    if !ledger_encrypted(conn)? {
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

/// A newly issued credential and its public claims.
#[derive(Debug)]
pub struct IssuedCredential {
    /// Canonical URI of the agent.
    pub agent_uri: AgentUri,
    /// Unique identity of this token in the ledger registry.
    pub token_jti: MagicTypeId,
    /// Base64 representation of the agent's public Ed25519 key.
    pub agent_key: String,
    /// Explicit grants attached to the credential.
    pub capabilities: Vec<String>,
    /// UTC issuance time.
    pub issued_at: DateTime<Utc>,
    /// UTC expiration time.
    pub expires_at: DateTime<Utc>,
    token: SecretString,
    private_key: Option<SecretString>,
}

impl IssuedCredential {
    /// Bearer token. Use `ExposeSecret` to transfer it into secure storage.
    #[must_use]
    pub fn token(&self) -> &SecretString {
        &self.token
    }

    /// Hex-encoded private key when issuance generated the keypair.
    #[must_use]
    pub fn generated_private_key(&self) -> Option<&SecretString> {
        self.private_key.as_ref()
    }

    fn public_data(&self) -> Value {
        json!({
            "agent_uri": self.agent_uri.as_str(),
            "token_jti": self.token_jti.as_str(),
            "agent_key": self.agent_key,
            "capabilities": self.capabilities,
            "issued_at": self.issued_at,
            "expires_at": self.expires_at,
        })
    }

    pub(crate) fn response_json(&self) -> Value {
        let mut data = self.public_data();
        data["token"] = json!(self.token.expose_secret());
        if let Some(private_key) = self.generated_private_key() {
            data["private_key"] = json!(private_key.expose_secret());
        }
        data
    }
}

/// Issue a credential and return the existing JSON representation.
///
/// Rust callers can use [`issue_credential`] to keep secrets in redacted types.
///
/// # Errors
/// Returns the same issuance errors as [`issue_credential`].
pub fn issue(
    conn: &Connection,
    actor: &Actor,
    request: &IssueRequest<'_>,
) -> Result<Value, CliError> {
    Ok(issue_credential(conn, actor, request)?.response_json())
}

/// Issue a token for a supplied public key or explicitly generate a local keypair.
///
/// # Errors
/// Returns validation errors for invalid keys, capabilities, names or TTLs.
pub fn issue_credential(
    conn: &Connection,
    actor: &Actor,
    request: &IssueRequest<'_>,
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
    issue_checked_credential(conn, actor, request)
}

pub(crate) fn issue_setup_credential(
    owner: &Granted<'_, SetupOwner>,
    name: &str,
    public_key: &str,
    ttl: Duration,
) -> Result<IssuedCredential, CliError> {
    issue_checked_credential(
        owner.connection(),
        owner.actor(),
        &IssueRequest {
            name,
            capabilities: &[Capability::Setup.path().to_owned()],
            ttl,
            public_key: Some(public_key),
            generate_key: false,
        },
    )
}

fn issue_checked_credential(
    conn: &Connection,
    actor: &Actor,
    request: &IssueRequest<'_>,
) -> Result<IssuedCredential, CliError> {
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
    let token_jti = "token".create_type_id::<V7>();
    let claims = AttestationClaims::builder()
        .jti(token_jti.to_string())
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
    let credential = IssuedCredential {
        agent_uri: uri,
        token_jti,
        agent_key: claims.agent_key,
        capabilities: claims.capabilities,
        issued_at: claims.iat,
        expires_at: claims.exp,
        token: token.into(),
        private_key: generated.as_ref().map(generated_key_hex),
    };
    crate::db::audit::atomically(conn, || {
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
        crate::db::audit::record_identity(
            conn,
            actor,
            crate::db::audit::IdentityEvent::TokenIssued,
            credential.token_jti.as_str(),
            &credential.public_data(),
        )
    })?;
    Ok(credential)
}

fn generated_key_hex(key: &SigningKey) -> SecretString {
    use std::fmt::Write as _;

    let seed = SecretBox::new(Box::new(key.to_bytes()));
    let mut hex = String::with_capacity(64);
    for byte in seed.expose_secret() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.into()
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

#[cfg(test)]
mod capability_witness_tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn verification_preserves_multiple_explicit_leaf_grants() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let passphrase: SecretString = "ephemeral test ledger".into();
        let ledger = Db::open(&directory.path().join("books.db"), Some(&passphrase))?;
        let owner = Actor::new("owner");
        init_root(ledger.conn(), &owner)?;
        let capabilities = [
            "ledger/read".into(),
            "ledger/approve".into(),
            "ledger/post/scheduled".into(),
        ];
        let issued = issue_credential(
            ledger.conn(),
            &owner,
            &IssueRequest {
                name: "reviewer",
                capabilities: &capabilities,
                ttl: Duration::from_mins(15),
                public_key: None,
                generate_key: true,
            },
        )?;
        let identity = verify(ledger.conn(), issued.token().expose_secret())?;
        assert_eq!(identity.capabilities(), &capabilities);
        assert!(identity.allows(Capability::Read)?);
        assert!(identity.allows(Capability::Approve)?);
        assert!(!identity.allows(Capability::Post)?);
        assert!(!identity.allows(Capability::Navigate)?);
        Ok(())
    }

    #[test]
    fn approval_witness_rechecks_ledger_registration_and_revocation() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let passphrase: secrecy::SecretString = "ephemeral test ledger".into();
        let ledger = Db::open(&directory.path().join("books.db"), Some(&passphrase))?;
        let owner = Actor::new("owner");
        init_root(ledger.conn(), &owner)?;
        let issued = issue(
            ledger.conn(),
            &owner,
            &IssueRequest {
                name: "reviewer",
                capabilities: &["ledger/approve".into()],
                ttl: Duration::from_mins(15),
                public_key: None,
                generate_key: true,
            },
        )?;
        let token = issued["token"]
            .as_str()
            .ok_or_else(|| CliError::General("missing test token".into()))?;
        let identity = verify(ledger.conn(), token)?;
        let actor = Actor::new("reviewer").with_identity(identity.clone());

        assert!(Granted::<Approve>::check(ledger.conn(), &actor).is_ok());
        let foreign = Db::open_in_memory()?;
        assert!(matches!(
            Granted::<Approve>::check(foreign.conn(), &actor),
            Err(CliError::Unauthorized(_))
        ));
        revoke(ledger.conn(), &owner, identity.jti())?;
        assert!(matches!(
            Granted::<Approve>::check(ledger.conn(), &actor),
            Err(CliError::Unauthorized(_))
        ));
        Ok(())
    }
}
