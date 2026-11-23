//! Private staging and public recovery state for a credential rotation.

use std::{fmt, str::FromStr, time::Duration};

use agent_uri_attestation::{AcceptAll, Verifier, VerifyingKey};
use mti::prelude::MagicTypeId;
use rusqlite::OptionalExtension;
use secrecy::{ExposeSecret, SecretString};

use super::{IssueRequest, IssuedCredential, WriteMode, prepare_credential};
use crate::{db::connection::ReadConn, error::CliError};

/// The `TypeID` of a single agent token in the ledger registry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TokenId(MagicTypeId);

impl TokenId {
    pub(super) const PREFIX: &str = "token";

    /// The canonical token identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn invalid() -> CliError {
        CliError::Validation("expected a token TypeID".into())
    }
}

impl FromStr for TokenId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        MagicTypeId::from_str(value)
            .map_err(|_| Self::invalid())?
            .try_into()
    }
}

impl TryFrom<MagicTypeId> for TokenId {
    type Error = CliError;

    fn try_from(id: MagicTypeId) -> Result<Self, Self::Error> {
        if id.prefix().as_str() != Self::PREFIX {
            return Err(Self::invalid());
        }
        Ok(Self(id))
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A signed replacement bound to one exact prior token ID.
///
/// Preparing it writes nothing to the registry or audit log. The host can
/// durably stage the replacement ID and secret before committing the rotation.
#[derive(Debug)]
pub struct PreparedRotation {
    prior: TokenId,
    replacement_id: TokenId,
    replacement: IssuedCredential,
}

impl PreparedRotation {
    pub(crate) fn prepare(
        conn: ReadConn<'_>,
        prior: TokenId,
        request: &IssueRequest<'_>,
        write_mode: WriteMode,
    ) -> Result<Self, CliError> {
        let replacement = prepare_credential(conn, request, write_mode)?;
        let replacement_id = TokenId(replacement.token_jti.clone());
        if prior == replacement_id {
            return Err(CliError::Validation(
                "replacement and prior token IDs must differ".into(),
            ));
        }
        Ok(Self {
            prior,
            replacement_id,
            replacement,
        })
    }

    /// The exact prior token to revoke at commit.
    #[must_use]
    pub const fn prior_id(&self) -> &TokenId {
        &self.prior
    }

    /// The replacement token ID to save in the host's recovery journal.
    #[must_use]
    pub const fn replacement_id(&self) -> &TokenId {
        &self.replacement_id
    }

    /// The replacement bearer. Use `ExposeSecret` only for secure storage.
    #[must_use]
    pub fn token(&self) -> &SecretString {
        self.replacement.token()
    }

    /// The generated agent private key, if the issuance request asked for one.
    #[must_use]
    pub fn generated_private_key(&self) -> Option<&SecretString> {
        self.replacement.generated_private_key()
    }

    pub(crate) fn replacement_for(
        &self,
        conn: ReadConn<'_>,
    ) -> Result<&IssuedCredential, CliError> {
        let invalid = || {
            CliError::Validation(
                "prepared replacement is not currently valid for this ledger".into(),
            )
        };
        let (root, public_key): (String, String) = conn
            .query_row(
                "SELECT trust_root, public_key FROM identity_root WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(invalid)?;
        let key = VerifyingKey::from_base64(&public_key)
            .map_err(|_| CliError::Database("invalid ledger root public key".into()))?;
        // The staged bearer only needs signature and clock checks here.
        // The write transaction checks the registry separately.
        let mut verifier = Verifier::with_leeway(Duration::ZERO).with_revocation(AcceptAll);
        verifier.add_trusted_root(root, key);
        verifier
            .verify(self.token().expose_secret())
            .map_err(|_| invalid())?;
        Ok(&self.replacement)
    }
}

/// Registry state of a token ID, independent of its expiration time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialStatus {
    /// The token has no committed registry entry.
    Unregistered,
    /// The token is registered and has no revocation.
    Registered,
    /// The token is registered and has been revoked.
    Revoked,
}

/// Read a token's committed registry state by ID to reconcile private host storage.
///
/// For an ID obtained from [`PreparedRotation::replacement_id`], a registry
/// entry means the rotation committed, even if the token was later revoked.
/// Verify the staged bearer before use to account for expiration.
///
/// # Errors
/// Returns database errors if the registry cannot be read.
pub fn credential_status(conn: ReadConn<'_>, id: &TokenId) -> Result<CredentialStatus, CliError> {
    let revoked = conn
        .query_row(
            "SELECT r.jti IS NOT NULL FROM agent_tokens t
             LEFT JOIN agent_revocations r ON r.jti = t.jti
             WHERE t.jti = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match revoked {
        None => CredentialStatus::Unregistered,
        Some(false) => CredentialStatus::Registered,
        Some(true) => CredentialStatus::Revoked,
    })
}
