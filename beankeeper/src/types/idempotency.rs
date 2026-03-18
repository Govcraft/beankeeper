//! Idempotency key for deduplicating transaction posts.
//!
//! An [`IdempotencyKey`] produces a deterministic, URL-safe reference string
//! from a human-readable input such as a bank reference number.

use core::fmt;

use data_encoding::BASE32_NOPAD;
use sha2::{Digest, Sha256};

/// Prefix applied to every idempotency key.
const PREFIX: &str = "txnref_";

/// A deterministic idempotency key derived from a human-readable reference.
///
/// # Examples
///
/// ```
/// use beankeeper::types::IdempotencyKey;
///
/// let key = IdempotencyKey::from_reference("chase-2026-03-15-001").unwrap();
/// assert!(key.as_str().starts_with("txnref_"));
///
/// // Same input always produces the same key.
/// let key2 = IdempotencyKey::from_reference("chase-2026-03-15-001").unwrap();
/// assert_eq!(key.as_str(), key2.as_str());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    value: String,
}

impl IdempotencyKey {
    /// Create an idempotency key from a human-readable reference string.
    ///
    /// The reference is hashed with SHA-256 and the first 16 bytes are
    /// base32-encoded (no padding, lowercase) to produce a deterministic
    /// `txnref_<encoded>` string.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyKeyError::Empty`] if the input is empty or
    /// contains only whitespace.
    pub fn from_reference(input: &str) -> Result<Self, IdempotencyKeyError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(IdempotencyKeyError::Empty);
        }

        let hash = Sha256::digest(trimmed.as_bytes());
        // Take first 16 bytes (128 bits) for a UUID-sized identifier.
        let encoded = BASE32_NOPAD.encode(&hash[..16]).to_lowercase();

        Ok(Self {
            value: format!("{PREFIX}{encoded}"),
        })
    }

    /// Returns the key as a string slice, suitable for database storage.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

/// Errors that can occur when creating an [`IdempotencyKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdempotencyKeyError {
    /// The reference string was empty or contained only whitespace.
    Empty,
}

impl fmt::Display for IdempotencyKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "idempotency key reference must not be empty"),
        }
    }
}

impl std::error::Error for IdempotencyKeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_reference_produces_prefixed_key() {
        let key = IdempotencyKey::from_reference("chase-2026-03-15-001")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        assert!(key.as_str().starts_with("txnref_"));
    }

    #[test]
    fn from_reference_is_deterministic() {
        let key1 = IdempotencyKey::from_reference("chase-2026-03-15-001")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        let key2 = IdempotencyKey::from_reference("chase-2026-03-15-001")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        assert_eq!(key1.as_str(), key2.as_str());
    }

    #[test]
    fn different_inputs_produce_different_keys() {
        let key1 = IdempotencyKey::from_reference("ref-001")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        let key2 = IdempotencyKey::from_reference("ref-002")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        assert_ne!(key1.as_str(), key2.as_str());
    }

    #[test]
    fn empty_reference_is_error() {
        let result = IdempotencyKey::from_reference("");
        assert!(matches!(result, Err(IdempotencyKeyError::Empty)));
    }

    #[test]
    fn whitespace_only_reference_is_error() {
        let result = IdempotencyKey::from_reference("   ");
        assert!(matches!(result, Err(IdempotencyKeyError::Empty)));
    }

    #[test]
    fn display_matches_as_str() {
        let key = IdempotencyKey::from_reference("test-ref")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        assert_eq!(key.to_string(), key.as_str());
    }

    #[test]
    fn trimmed_input_matches() {
        let key1 = IdempotencyKey::from_reference("  ref-001  ")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        let key2 = IdempotencyKey::from_reference("ref-001")
            .unwrap_or_else(|e| panic!("unexpected error: {e}"));
        assert_eq!(key1.as_str(), key2.as_str());
    }
}
