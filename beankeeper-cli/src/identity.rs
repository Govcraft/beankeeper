//! Local bearer attestations and optional per-ledger capability enforcement.

use std::str::FromStr;
use std::time::Duration;

use agent_uri::{AgentId, AgentUri};
use agent_uri_attestation::{
    AttestationClaims, Denylist, Issuer, SigningKey, Verifier, VerifyingKey,
};
use chrono::{DateTime, Utc};
use mti::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde::{Serializer, ser::SerializeMap};
use serde_json::{Value, json};

use crate::capability::Capability;
use crate::db::Actor;
use crate::db::audit::grant::{
    self, AgentUndo, Bookkeeping, Chart, CorrectionPropose, OnboardingPost, OnboardingPropose,
    RefreshFeed, RefreshFeedProposed, StatementReconcile,
};
pub use crate::db::audit::grant::{
    Admin, Approve, Budget, Granted, OwnerUndo, Post, Propose, Reconcile, ResolveProposal, Reverse,
};
pub(crate) use crate::db::audit::grant::{SetupOwner, StatementApprover};
pub use crate::db::audit::identity_write::{
    commit_rotation, init_root, issue, issue_credential, issue_credential_with_mode,
    prepare_rotation, revoke,
};
use crate::db::connection::ReadConn;
use crate::error::CliError;
use crate::operation::{Operation, Permission};

mod delegation;
mod rotation;
mod write_mode;
use delegation::DelegatedTrust;
pub(crate) use delegation::StartupAuthority;
pub use rotation::{CredentialStatus, PreparedRotation, TokenId, credential_status};
pub use write_mode::WriteMode;
use write_mode::{EffectiveGrants, IssuanceGrants};

/// Identity claims accepted by the ledger's verifier.
#[derive(Debug, Clone)]
pub struct VerifiedAgent {
    uri: AgentUri,
    jti: MagicTypeId,
    grants: EffectiveGrants,
    signed_capabilities: Vec<String>,
    agent_key: String,
    issuer_key: String,
    expires_at: DateTime<Utc>,
    delegation: Option<DelegatedTrust>,
}

impl VerifiedAgent {
    /// Canonical agent URI from the signed token.
    #[must_use]
    pub fn uri(&self) -> &str {
        self.typed_uri().as_str()
    }

    pub(crate) fn typed_uri(&self) -> &AgentUri {
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
        &self.signed_capabilities
    }

    /// Signed policy for ordinary transaction posts.
    #[must_use]
    pub const fn write_mode(&self) -> WriteMode {
        self.grants.mode()
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
            Permission::Posting => self.allows(self.write_mode().posting_capability()),
            Permission::FeedRefresh => match self.write_mode() {
                WriteMode::Direct => grant::permits::<RefreshFeed>(self),
                WriteMode::ProposeOnly => grant::permits::<RefreshFeedProposed>(self),
            },
            Permission::Bookkeeping => match self.write_mode() {
                WriteMode::Direct => grant::permits::<Bookkeeping>(self),
                WriteMode::ProposeOnly => grant::permits::<CorrectionPropose>(self),
            },
            Permission::StatementReconciliation => match self.write_mode() {
                WriteMode::Direct => grant::permits::<StatementReconcile>(self),
                WriteMode::ProposeOnly => grant::permits::<CorrectionPropose>(self),
            },
            Permission::ReadOrReconcile => {
                Ok(self.allows(Capability::Read)? || self.allows(Capability::Reconcile)?)
            }
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
            Permission::SetupOrChart => {
                Ok(self.allows(Capability::Setup)? || grant::permits::<Chart>(self)?)
            }
            Permission::Chart => match self.write_mode() {
                WriteMode::Direct => grant::permits::<OnboardingPost>(self),
                WriteMode::ProposeOnly => grant::permits::<OnboardingPropose>(self),
            },
            Permission::AgentUndo | Permission::Reversal => match self.write_mode() {
                WriteMode::ProposeOnly => grant::permits::<CorrectionPropose>(self),
                WriteMode::Direct if operation.permission() == Permission::AgentUndo => {
                    Ok(grant::permits::<AgentUndo>(self)? || grant::permits::<OwnerUndo>(self)?)
                }
                WriteMode::Direct => grant::permits::<Reverse>(self),
            },
            permission => self.allows(permission.primary()?),
        }
    }

    pub(crate) fn can_discover(
        &self,
        operation: Operation,
        ledger_enforced: bool,
    ) -> Result<bool, CliError> {
        if self.enforces_permissions(ledger_enforced)
            || operation.requires_onboarding_grant()
            || matches!(operation.permission(), Permission::Capability(capability) if self.requires_explicit_capability(capability))
            || matches!(
                operation.permission(),
                Permission::SetupOwner
                    | Permission::OwnerUndo
                    | Permission::ExplicitAdmin
                    | Permission::Bookkeeping
                    | Permission::StatementReconciliation
                    | Permission::FeedRefresh
                    | Permission::AgentUndo
                    | Permission::Reversal
            )
        {
            self.allows_known_operation(operation)
        } else {
            Ok(true)
        }
    }

    pub(crate) fn is_setup_credential(&self) -> bool {
        self.signed_capabilities == [Capability::Setup.path()]
    }

    pub(crate) fn enforces_permissions(&self, ledger_enforced: bool) -> bool {
        ledger_enforced
            || self.delegation.is_some()
            || self.is_setup_credential()
            || self.write_mode().restricts_capabilities()
    }

    pub(crate) fn requires_explicit_capability(&self, capability: Capability) -> bool {
        capability == Capability::TaxDetails
            || capability.is_disbursement_write()
            || (capability == Capability::Admin && self.grants.is_bookkeeper())
    }

    /// Whether the verified identity holds the requested ledger or UI grant.
    ///
    /// # Errors
    /// This method retains its fallible signature for existing callers.
    pub fn allows(&self, capability: Capability) -> Result<bool, CliError> {
        Ok(self.grants.contains(capability))
    }
}

impl VerifiedAgent {
    pub(crate) fn check_live(&self, conn: ReadConn<'_>) -> Result<(), CliError> {
        match &self.delegation {
            Some(authority) => authority.check_live(self, conn),
            None => self.check_registry(conn),
        }
    }

    fn check_registry(&self, conn: ReadConn<'_>) -> Result<(), CliError> {
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
    conn: ReadConn<'_>,
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
pub fn verify(conn: ReadConn<'_>, token: &str) -> Result<VerifiedAgent, CliError> {
    verify_required(conn, token, None)
}

/// Verify optional credentials and enforce the current ledger policy.
/// Navigation and setup always require a verified grant.
///
/// # Errors
/// Returns `UNAUTHORIZED` for missing or rejected credentials, `FORBIDDEN` for
/// an insufficient grant, or database errors for invalid policy/storage state.
pub fn authorize(
    conn: ReadConn<'_>,
    token: Option<&str>,
    required: Capability,
) -> Result<Option<VerifiedAgent>, CliError> {
    let enforce = crate::db::policy::get(conn)?.enforce
        || matches!(
            required,
            Capability::Navigate | Capability::Setup | Capability::TaxDetails
        );
    authorize_with_policy(conn, token, required, enforce, |_| {})
}

pub(crate) fn authorize_backup(
    conn: ReadConn<'_>,
    token: Option<&str>,
    version: crate::db::SchemaVersion,
    policy: Option<&crate::db::policy::LedgerPolicy>,
    on_verified: impl FnOnce(&VerifiedAgent),
) -> Result<(), CliError> {
    if token.is_some() && version < crate::db::schema::IDENTITY_SCHEMA_VERSION {
        return Err(unauthorized());
    }
    let enforce = policy.is_some_and(|policy| policy.enforce);
    authorize_with_policy(conn, token, Capability::Admin, enforce, on_verified).map(|_| ())
}

fn authorize_with_policy(
    conn: ReadConn<'_>,
    token: Option<&str>,
    required: Capability,
    enforce: bool,
    on_verified: impl FnOnce(&VerifiedAgent),
) -> Result<Option<VerifiedAgent>, CliError> {
    match token {
        Some(token) => {
            let identity = verify_required(conn, token, None)?;
            on_verified(&identity);
            if (identity.enforces_permissions(enforce)
                || identity.requires_explicit_capability(required))
                && !identity.allows(required)?
            {
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
    conn: ReadConn<'_>,
    token: Option<&str>,
    operation: &str,
) -> Result<Option<VerifiedAgent>, CliError> {
    authorize_known_operation(conn, token, operation.parse()?)
}

pub(crate) fn authorize_known_operation(
    conn: ReadConn<'_>,
    token: Option<&str>,
    operation: Operation,
) -> Result<Option<VerifiedAgent>, CliError> {
    let permission = operation.permission();
    if let Permission::Capability(required) = permission {
        return authorize(conn, token, required);
    }
    permission.primary()?;
    if matches!(permission, Permission::SetupOwner | Permission::OwnerUndo) {
        let token = token.ok_or_else(|| {
            CliError::Unauthorized(format!(
                "{} requires a ledger/admin token",
                operation.name()
            ))
        })?;
        return verify_required(conn, token, Some(Capability::Admin)).map(Some);
    }
    let enforce = crate::db::policy::get(conn)?.enforce
        || operation.requires_onboarding_grant()
        || permission == Permission::AgentUndo;
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
    conn: ReadConn<'_>,
    identity: VerifiedAgent,
    operation: Operation,
) -> Result<VerifiedAgent, CliError> {
    let enforce = matches!(
        operation.permission(),
        Permission::SetupOwner | Permission::OwnerUndo
    ) || crate::db::policy::get(conn)?.enforce;
    identity.check_live(conn)?;
    require_operation_grant(&identity, operation, enforce)?;
    Ok(identity)
}

fn require_operation_grant(
    identity: &VerifiedAgent,
    operation: Operation,
    ledger_enforced: bool,
) -> Result<(), CliError> {
    let permission = operation.permission();
    let required = match permission {
        Permission::Posting
        | Permission::Bookkeeping
        | Permission::StatementReconciliation
        | Permission::FeedRefresh => identity.write_mode().posting_capability(),
        Permission::AgentUndo | Permission::Reversal
            if identity.write_mode() == WriteMode::ProposeOnly =>
        {
            Capability::Propose
        }
        _ => permission.primary()?,
    };
    let enforce = ledger_enforced
        || matches!(permission, Permission::Capability(capability) if identity.requires_explicit_capability(capability))
        || matches!(
            permission,
            Permission::SetupOwner
                | Permission::OwnerUndo
                | Permission::ExplicitAdmin
                | Permission::Bookkeeping
                | Permission::StatementReconciliation
                | Permission::FeedRefresh
                | Permission::AgentUndo
                | Permission::Reversal
        )
        || operation.requires_onboarding_grant()
        || matches!(
            permission,
            Permission::Capability(
                Capability::Navigate | Capability::Setup | Capability::TaxDetails
            )
        );
    if !identity.enforces_permissions(enforce) || identity.allows_known_operation(operation)? {
        return Ok(());
    }
    let alternative = match permission {
        Permission::ProposalList | Permission::ProposalReject => Some(Capability::Approve),
        Permission::ReadOrReconcile => Some(Capability::Reconcile),
        Permission::StatementReconciliation if identity.write_mode() == WriteMode::Direct => {
            Some(Capability::Reconcile)
        }
        Permission::FeedRefresh => Some(Capability::Admin),
        Permission::SetupStatus | Permission::SetupOrChart => Some(Capability::Setup),
        Permission::Chart | Permission::AgentUndo | Permission::Bookkeeping
            if identity.write_mode() == WriteMode::Direct =>
        {
            Some(Capability::Admin)
        }
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
    conn: ReadConn<'_>,
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
    let agent = VerifiedAgent {
        uri,
        jti: MagicTypeId::from_str(&claims.jti).map_err(|_| unauthorized())?,
        grants: EffectiveGrants::from(claims.capabilities.as_slice()),
        signed_capabilities: claims.capabilities,
        agent_key: claims.agent_key,
        issuer_key: key,
        expires_at: claims.exp,
        delegation: None,
    };
    if let Some(required) = required
        && !agent.allows(required)?
    {
        return Err(CliError::Forbidden(format!(
            "operation requires capability {}",
            required.path()
        )));
    }

    Ok(agent)
}

pub(crate) fn ledger_encrypted(conn: &Connection) -> Result<bool, CliError> {
    Ok(conn
        .query_row("PRAGMA cipher_salt", [], |r| r.get::<_, String>(0))
        .optional()?
        .is_some())
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
    write_mode: WriteMode,
    token: SecretString,
    private_key: Option<SecretString>,
}

impl IssuedCredential {
    /// Signed policy for ordinary transaction posts.
    #[must_use]
    pub const fn write_mode(&self) -> WriteMode {
        self.write_mode
    }

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

    pub(crate) fn public_data(&self) -> Value {
        json!({
            "agent_uri": self.agent_uri.as_str(),
            "token_jti": self.token_jti.as_str(),
            "agent_key": self.agent_key,
            "capabilities": self.capabilities,
            "write_mode": self.write_mode,
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

pub(crate) fn serialize_credential_response<S: Serializer>(
    credential: &IssuedCredential,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut fields = serializer.serialize_map(None)?;
    fields.serialize_entry("agent_uri", credential.agent_uri.as_str())?;
    fields.serialize_entry("token_jti", credential.token_jti.as_str())?;
    fields.serialize_entry("agent_key", &credential.agent_key)?;
    fields.serialize_entry("capabilities", &credential.capabilities)?;
    fields.serialize_entry("write_mode", &credential.write_mode)?;
    fields.serialize_entry("issued_at", &credential.issued_at)?;
    fields.serialize_entry("expires_at", &credential.expires_at)?;
    fields.serialize_entry("token", credential.token.expose_secret())?;
    if let Some(private_key) = credential.generated_private_key() {
        fields.serialize_entry("private_key", private_key.expose_secret())?;
    }
    fields.end()
}

pub(crate) fn prepare_credential(
    conn: ReadConn<'_>,
    request: &IssueRequest<'_>,
    write_mode: WriteMode,
) -> Result<IssuedCredential, CliError> {
    let grants = IssuanceGrants::for_regular_issuance(write_mode, request.capabilities)?;
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
    prepare_credential_for_agent(
        conn,
        grants,
        request.ttl,
        &agent_id,
        &agent_key,
        generated.as_ref(),
    )
}

pub(crate) fn prepare_setup_credential(
    conn: ReadConn<'_>,
    agent_id: &AgentId,
    agent_key: &VerifyingKey,
    ttl: Duration,
) -> Result<IssuedCredential, CliError> {
    let capabilities = [Capability::Setup.path().to_owned()];
    let grants = IssuanceGrants::try_from((WriteMode::default(), capabilities.as_slice()))?;
    prepare_credential_for_agent(conn, grants, ttl, agent_id, agent_key, None)
}

fn prepare_credential_for_agent(
    conn: ReadConn<'_>,
    grants: IssuanceGrants,
    ttl: Duration,
    agent_id: &AgentId,
    agent_key: &VerifyingKey,
    generated: Option<&SigningKey>,
) -> Result<IssuedCredential, CliError> {
    let scope = grants.scope();
    let write_mode = grants.mode();
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
    let token_jti = TokenId::PREFIX.create_type_id::<V7>();
    let claims = AttestationClaims::builder()
        .jti(token_jti.to_string())
        .agent_uri(uri.to_string())
        .agent_key(agent_key)
        .capabilities(Vec::from(grants))
        .issuer(&root)
        .ttl(ttl)
        .build()
        .map_err(|e| CliError::Validation(e.to_string()))?;
    let token = Issuer::new(root, key, ttl)
        .issue_claims(&claims)
        .map_err(|e| CliError::Validation(e.to_string()))?;
    Ok(IssuedCredential {
        agent_uri: uri,
        token_jti,
        agent_key: claims.agent_key,
        write_mode,
        capabilities: claims.capabilities,
        issued_at: claims.iat,
        expires_at: claims.exp,
        token: token.into(),
        private_key: generated.map(generated_key_hex),
    })
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

/// List issued claims without returning tokens or private keys.
///
/// # Errors
/// Returns database errors if the registry cannot be read.
pub fn list(conn: ReadConn<'_>) -> Result<Value, CliError> {
    let mut stmt = conn.prepare("SELECT t.jti, t.agent_uri, t.agent_key, t.capabilities, t.issued_at, t.expires_at, r.revoked_at FROM agent_tokens t LEFT JOIN agent_revocations r ON r.jti = t.jti ORDER BY t.issued_at, t.jti")?;
    let rows = stmt.query_map([], |r| {
        let capabilities = serde_json::from_str::<Vec<String>>(&r.get::<_, String>(3)?).ok();
        let write_mode = capabilities.as_deref().map(WriteMode::from);
        Ok(json!({
            "token_jti": r.get::<_, String>(0)?, "agent_uri": r.get::<_, String>(1)?,
            "agent_key": r.get::<_, String>(2)?, "capabilities": capabilities,
            "write_mode": write_mode,
            "issued_at": r.get::<_, String>(4)?, "expires_at": r.get::<_, String>(5)?,
            "revoked_at": r.get::<_, Option<String>>(6)?,
        }))
    })?;
    Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
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
    fn static_catalog_uses_effective_bookkeeper_grants() -> Result<(), CliError> {
        use crate::mcp::LedgerAccess;
        let directory = tempfile::tempdir()?;
        let passphrase: SecretString = "catalog test ledger".into();
        let ledger = Db::open(&directory.path().join("books.db"), Some(&passphrase))?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&ledger, &owner, init_root)?;
        for (mode, capabilities) in [
            (
                WriteMode::Direct,
                vec!["ledger/read".into(), "ledger/post".into()],
            ),
            (
                WriteMode::ProposeOnly,
                vec!["ledger/read".into(), "ledger/propose".into()],
            ),
        ] {
            let issued = Granted::<Admin>::with_db(&ledger, &owner, |grant| {
                issue_credential_with_mode(
                    grant,
                    &IssueRequest {
                        name: "catalog_bookkeeper",
                        capabilities: &capabilities,
                        ttl: Duration::from_mins(15),
                        public_key: None,
                        generate_key: true,
                    },
                    mode,
                )
            })?;
            let identity = verify(ledger.read(), issued.token().expose_secret())?;
            for enforced in [false, true] {
                let entries = LedgerAccess::ReadWrite.catalog_for(&identity, enforced)?;
                let includes =
                    |operation| entries.iter().any(|entry| entry.operation() == operation);
                assert!(includes(Operation::LedgerSetupBookCreate));
                assert!(includes(Operation::TxnReverse));
                assert!(includes(Operation::AuditUndo));
                assert!(includes(Operation::TxnPost));
                assert!(!includes(Operation::CompanyCreate));
                for entry in LedgerAccess::ReadWrite.catalog() {
                    assert_eq!(
                        includes(entry.operation()),
                        identity.can_discover(entry.operation(), enforced)?
                    );
                }
                let read_only = LedgerAccess::ReadOnly.catalog_for(&identity, enforced)?;
                assert!(
                    read_only
                        .iter()
                        .all(|entry| entry.operation().is_read_only())
                );
            }
        }
        Ok(())
    }

    #[test]
    fn verification_preserves_multiple_explicit_leaf_grants() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let passphrase: SecretString = "ephemeral test ledger".into();
        let ledger = Db::open(&directory.path().join("books.db"), Some(&passphrase))?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&ledger, &owner, init_root)?;
        let capabilities = [
            "ledger/read".into(),
            "ledger/approve".into(),
            "ledger/post/scheduled".into(),
        ];
        let issued = Granted::<Admin>::with_db(&ledger, &owner, |grant| {
            issue_credential(
                grant,
                &IssueRequest {
                    name: "reviewer",
                    capabilities: &capabilities,
                    ttl: Duration::from_mins(15),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        let identity = verify(ledger.read(), issued.token().expose_secret())?;
        assert_eq!(identity.capabilities(), &capabilities);
        assert!(identity.allows(Capability::Read)?);
        assert!(identity.allows(Capability::Approve)?);
        assert!(!identity.allows(Capability::Post)?);
        assert!(!identity.allows(Capability::Navigate)?);
        for enforced in [false, true] {
            let catalog = crate::mcp::LedgerAccess::ReadWrite.catalog_for(&identity, enforced)?;
            let includes = |operation| catalog.iter().any(|entry| entry.operation() == operation);
            assert!(includes(Operation::AccountList));
            assert!(includes(Operation::TxnApprove));
            assert_eq!(includes(Operation::TxnPost), !enforced);
            assert!(!includes(Operation::LedgerSetupBookCreate));
            assert!(!includes(Operation::TxnReverse));
            assert!(!includes(Operation::AuditUndo));
            assert!(!includes(Operation::Restore));
        }
        Ok(())
    }

    #[test]
    fn approval_witness_rechecks_ledger_registration_and_revocation() -> Result<(), CliError> {
        let directory = tempfile::tempdir()?;
        let passphrase: secrecy::SecretString = "ephemeral test ledger".into();
        let ledger = Db::open(&directory.path().join("books.db"), Some(&passphrase))?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&ledger, &owner, init_root)?;
        let issued = Granted::<Admin>::with_db(&ledger, &owner, |grant| {
            issue(
                grant,
                &IssueRequest {
                    name: "reviewer",
                    capabilities: &["ledger/approve".into()],
                    ttl: Duration::from_mins(15),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        let token = issued["token"]
            .as_str()
            .ok_or_else(|| CliError::General("missing test token".into()))?;
        let identity = verify(ledger.read(), token)?;
        let actor = Actor::new("reviewer".parse()?).with_identity(identity.clone());

        assert!(Granted::<Approve>::with_db(&ledger, &actor, |_| Ok(())).is_ok());
        let foreign = Db::open_in_memory()?;
        assert!(matches!(
            Granted::<Approve>::with_db(&foreign, &actor, |_| Ok(())),
            Err(CliError::Unauthorized(_))
        ));
        Granted::<Admin>::with_db(&ledger, &owner, |grant| revoke(grant, identity.jti()))?;
        assert!(matches!(
            Granted::<Approve>::with_db(&ledger, &actor, |_| Ok(())),
            Err(CliError::Unauthorized(_))
        ));
        Ok(())
    }
}
