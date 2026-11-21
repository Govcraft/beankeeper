//! Validation for the ledger's user-supplied idempotency reference.

use std::{fmt, str::FromStr};

use crate::error::CliError;

/// The original nonempty reference, trimmed once for storage and lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransactionReference(Box<str>);

impl TransactionReference {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0.into_string()
    }
}

impl fmt::Display for TransactionReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EmptyReference;

impl fmt::Display for EmptyReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("reference must not be empty")
    }
}

impl std::error::Error for EmptyReference {}

impl From<EmptyReference> for CliError {
    fn from(error: EmptyReference) -> Self {
        Self::Validation(error.to_string())
    }
}

impl FromStr for TransactionReference {
    type Err = EmptyReference;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(EmptyReference);
        }
        // Existing ledger references are part of the public wire format.
        // Retaining the original key preserves retries and reference lookups.
        Ok(Self(value.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_references_round_trip_after_trimming() -> Result<(), EmptyReference> {
        let reference: TransactionReference = "  RENT-2026-03  ".parse()?;
        assert_eq!(reference.as_str(), "RENT-2026-03");
        Ok(())
    }

    #[test]
    fn empty_and_whitespace_only_references_are_invalid() {
        for reference in ["", "   "] {
            assert_eq!(
                reference.parse::<TransactionReference>(),
                Err(EmptyReference)
            );
        }
    }
}
