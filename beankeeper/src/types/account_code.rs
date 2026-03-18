use core::fmt;
use core::str::FromStr;

/// Error type for [`AccountCode`] validation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountCodeError {
    /// The account code string was empty.
    Empty,
    /// The account code contains an invalid character.
    InvalidCharacter {
        /// The invalid character encountered.
        char: char,
        /// Zero-based position of the invalid character.
        position: usize,
    },
}

impl fmt::Display for AccountCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "account code cannot be empty"),
            Self::InvalidCharacter { char, position } => {
                write!(
                    f,
                    "invalid character {char:?} at position {position} (expected digits, hyphens, or dots)"
                )
            }
        }
    }
}

impl std::error::Error for AccountCodeError {}

/// A validated account code for chart-of-accounts organization.
///
/// Account codes are non-empty strings containing digits, hyphens, or dots
/// (e.g., `"1000"`, `"1000.10"`, `"1-1000"`).
///
/// Codes are ordered lexicographically, which works naturally for
/// hierarchical account numbering schemes.
///
/// # Examples
///
/// ```
/// use beankeeper::types::AccountCode;
///
/// let code = AccountCode::new("1000").unwrap();
/// assert_eq!(code.as_str(), "1000");
///
/// let parent = AccountCode::new("1000").unwrap();
/// let child = AccountCode::new("1000.10").unwrap();
/// assert!(parent.is_parent_of(&child));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountCode(String);

impl AccountCode {
    /// Creates a new validated account code.
    ///
    /// The code must be non-empty and contain only digits (`0-9`),
    /// hyphens (`-`), or dots (`.`).
    ///
    /// # Errors
    ///
    /// Returns [`AccountCodeError::Empty`] if the string is empty, or
    /// [`AccountCodeError::InvalidCharacter`] if an invalid character is found.
    pub fn new(code: &str) -> Result<Self, AccountCodeError> {
        if code.is_empty() {
            return Err(AccountCodeError::Empty);
        }

        for (i, ch) in code.chars().enumerate() {
            if !ch.is_ascii_digit() && ch != '-' && ch != '.' {
                return Err(AccountCodeError::InvalidCharacter {
                    char: ch,
                    position: i,
                });
            }
        }

        Ok(Self(code.to_owned()))
    }

    /// Returns the account code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` if this code is a hierarchical parent of `other`.
    ///
    /// A code is considered a parent if `other` starts with this code
    /// followed by a separator (`.` or `-`).
    ///
    /// # Examples
    ///
    /// ```
    /// use beankeeper::types::AccountCode;
    ///
    /// let parent = AccountCode::new("1000").unwrap();
    /// let child = AccountCode::new("1000.10").unwrap();
    /// let sibling = AccountCode::new("1001").unwrap();
    ///
    /// assert!(parent.is_parent_of(&child));
    /// assert!(!parent.is_parent_of(&sibling));
    /// assert!(!parent.is_parent_of(&parent)); // not a parent of itself
    /// ```
    #[must_use]
    pub fn is_parent_of(&self, other: &Self) -> bool {
        if other.0.len() <= self.0.len() {
            return false;
        }

        if !other.0.starts_with(&self.0) {
            return false;
        }

        // The character immediately after the parent code must be a separator
        other
            .0
            .as_bytes()
            .get(self.0.len())
            .is_some_and(|&b| b == b'.' || b == b'-')
    }
}

impl fmt::Display for AccountCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for AccountCode {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountCode {
    type Err = AccountCodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_numeric_code() {
        let code = AccountCode::new("1000");
        assert!(code.is_ok());
        assert_eq!(
            code.unwrap_or_else(|_| AccountCode("".into())).as_str(),
            "1000"
        );
    }

    #[test]
    fn valid_code_with_dots() {
        assert!(AccountCode::new("1000.10").is_ok());
    }

    #[test]
    fn valid_code_with_hyphens() {
        assert!(AccountCode::new("1-1000").is_ok());
    }

    #[test]
    fn valid_code_mixed_separators() {
        assert!(AccountCode::new("1-1000.10").is_ok());
    }

    #[test]
    fn empty_code_rejected() {
        assert_eq!(AccountCode::new(""), Err(AccountCodeError::Empty));
    }

    #[test]
    fn invalid_character_rejected() {
        let result = AccountCode::new("100A");
        assert!(matches!(
            result,
            Err(AccountCodeError::InvalidCharacter {
                char: 'A',
                position: 3
            })
        ));
    }

    #[test]
    fn space_rejected() {
        assert!(AccountCode::new("10 00").is_err());
    }

    #[test]
    fn parent_of_with_dot_separator() {
        let parent = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        let child = AccountCode::new("1000.10").unwrap_or_else(|_| AccountCode("".into()));
        assert!(parent.is_parent_of(&child));
    }

    #[test]
    fn parent_of_with_hyphen_separator() {
        let parent = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        let child = AccountCode::new("1000-10").unwrap_or_else(|_| AccountCode("".into()));
        assert!(parent.is_parent_of(&child));
    }

    #[test]
    fn not_parent_of_self() {
        let code = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        assert!(!code.is_parent_of(&code));
    }

    #[test]
    fn not_parent_of_sibling() {
        let a = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        let b = AccountCode::new("1001").unwrap_or_else(|_| AccountCode("".into()));
        assert!(!a.is_parent_of(&b));
    }

    #[test]
    fn not_parent_without_separator() {
        let a = AccountCode::new("100").unwrap_or_else(|_| AccountCode("".into()));
        let b = AccountCode::new("10000").unwrap_or_else(|_| AccountCode("".into()));
        assert!(!a.is_parent_of(&b));
    }

    #[test]
    fn display() {
        let code = AccountCode::new("1000.10").unwrap_or_else(|_| AccountCode("".into()));
        assert_eq!(format!("{code}"), "1000.10");
    }

    #[test]
    fn as_ref_str() {
        let code = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        let s: &str = code.as_ref();
        assert_eq!(s, "1000");
    }

    #[test]
    fn from_str_works() {
        let code: Result<AccountCode, _> = "1000".parse();
        assert!(code.is_ok());
    }

    #[test]
    fn ordering_is_lexicographic() {
        let a = AccountCode::new("1000").unwrap_or_else(|_| AccountCode("".into()));
        let b = AccountCode::new("2000").unwrap_or_else(|_| AccountCode("".into()));
        assert!(a < b);
    }

    #[test]
    fn error_display_empty() {
        assert_eq!(
            format!("{}", AccountCodeError::Empty),
            "account code cannot be empty"
        );
    }

    #[test]
    fn error_display_invalid_char() {
        let err = AccountCodeError::InvalidCharacter {
            char: 'A',
            position: 3,
        };
        assert!(format!("{err}").contains("invalid character"));
    }
}
