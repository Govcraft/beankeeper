use core::fmt;
use std::str::FromStr;

/// Clearance status for a ledger entry.
///
/// This tracks whether an entry has been verified against an external
/// statement (e.g., bank reconciliation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ClearanceStatus {
    /// The entry has not been verified (default).
    #[default]
    Uncleared,
    /// The entry matches an external statement.
    Cleared,
    /// The entry has been finalized in a reconciliation workflow.
    Reconciled,
}

impl ClearanceStatus {
    /// Returns the string representation used in the database.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Uncleared => "uncleared",
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
        }
    }
}

impl fmt::Display for ClearanceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error type when parsing a clearance status fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseClearanceError(String);

impl fmt::Display for ParseClearanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid clearance status '{}'", self.0)
    }
}

impl std::error::Error for ParseClearanceError {}

impl FromStr for ClearanceStatus {
    type Err = ParseClearanceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "uncleared" => Ok(Self::Uncleared),
            "cleared" => Ok(Self::Cleared),
            "reconciled" => Ok(Self::Reconciled),
            _ => Err(ParseClearanceError(s.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_and_display() {
        assert_eq!(ClearanceStatus::Uncleared.as_str(), "uncleared");
        assert_eq!(format!("{}", ClearanceStatus::Cleared), "cleared");
    }

    #[test]
    fn from_str() {
        assert_eq!("uncleared".parse(), Ok(ClearanceStatus::Uncleared));
        assert_eq!("CLEARED".parse(), Ok(ClearanceStatus::Cleared));
        assert_eq!("Reconciled".parse(), Ok(ClearanceStatus::Reconciled));
        assert!("invalid".parse::<ClearanceStatus>().is_err());
    }
}
