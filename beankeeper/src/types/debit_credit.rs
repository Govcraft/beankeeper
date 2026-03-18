use core::fmt;
use core::ops::Not;
use core::str::FromStr;

/// Error type for parsing [`DebitOrCredit`] from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DebitCreditError {
    /// The provided string does not match "Debit" or "Credit".
    InvalidValue { value: String },
}

impl fmt::Display for DebitCreditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue { value } => {
                write!(
                    f,
                    "invalid debit/credit value: {value:?} (expected \"Debit\" or \"Credit\")"
                )
            }
        }
    }
}

impl std::error::Error for DebitCreditError {}

/// The direction of a ledger entry.
///
/// In double-entry bookkeeping, every transaction records equal
/// amounts of debits and credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DebitOrCredit {
    /// A debit entry.
    Debit,
    /// A credit entry.
    Credit,
}

impl DebitOrCredit {
    /// Returns the opposite direction.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Debit => Self::Credit,
            Self::Credit => Self::Debit,
        }
    }

    /// Returns `true` if this is a debit.
    #[must_use]
    pub const fn is_debit(self) -> bool {
        matches!(self, Self::Debit)
    }

    /// Returns `true` if this is a credit.
    #[must_use]
    pub const fn is_credit(self) -> bool {
        matches!(self, Self::Credit)
    }

    /// Returns `1` for debit, `-1` for credit.
    ///
    /// Useful for signed balance calculations.
    #[must_use]
    pub const fn sign(self) -> i8 {
        match self {
            Self::Debit => 1,
            Self::Credit => -1,
        }
    }
}

impl fmt::Display for DebitOrCredit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Debit => write!(f, "Debit"),
            Self::Credit => write!(f, "Credit"),
        }
    }
}

impl FromStr for DebitOrCredit {
    type Err = DebitCreditError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Debit" | "debit" | "DR" | "Dr" => Ok(Self::Debit),
            "Credit" | "credit" | "CR" | "Cr" => Ok(Self::Credit),
            _ => Err(DebitCreditError::InvalidValue {
                value: s.to_owned(),
            }),
        }
    }
}

impl Not for DebitOrCredit {
    type Output = Self;

    fn not(self) -> Self::Output {
        self.opposite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debit_opposite_is_credit() {
        assert_eq!(DebitOrCredit::Debit.opposite(), DebitOrCredit::Credit);
    }

    #[test]
    fn credit_opposite_is_debit() {
        assert_eq!(DebitOrCredit::Credit.opposite(), DebitOrCredit::Debit);
    }

    #[test]
    fn not_operator_works() {
        assert_eq!(!DebitOrCredit::Debit, DebitOrCredit::Credit);
        assert_eq!(!DebitOrCredit::Credit, DebitOrCredit::Debit);
    }

    #[test]
    fn display_format() {
        assert_eq!(format!("{}", DebitOrCredit::Debit), "Debit");
        assert_eq!(format!("{}", DebitOrCredit::Credit), "Credit");
    }

    #[test]
    fn from_str_variants() {
        assert_eq!(
            "Debit".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Debit)
        );
        assert_eq!(
            "debit".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Debit)
        );
        assert_eq!(
            "DR".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Debit)
        );
        assert_eq!(
            "Credit".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Credit)
        );
        assert_eq!(
            "credit".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Credit)
        );
        assert_eq!(
            "CR".parse::<DebitOrCredit>().ok(),
            Some(DebitOrCredit::Credit)
        );
    }

    #[test]
    fn from_str_invalid() {
        assert!("foo".parse::<DebitOrCredit>().is_err());
    }

    #[test]
    fn is_debit() {
        assert!(DebitOrCredit::Debit.is_debit());
        assert!(!DebitOrCredit::Credit.is_debit());
    }

    #[test]
    fn is_credit() {
        assert!(DebitOrCredit::Credit.is_credit());
        assert!(!DebitOrCredit::Debit.is_credit());
    }

    #[test]
    fn sign_values() {
        assert_eq!(DebitOrCredit::Debit.sign(), 1);
        assert_eq!(DebitOrCredit::Credit.sign(), -1);
    }

    #[test]
    fn error_display() {
        let err = DebitCreditError::InvalidValue {
            value: "foo".to_owned(),
        };
        assert!(format!("{err}").contains("invalid debit/credit"));
    }
}
