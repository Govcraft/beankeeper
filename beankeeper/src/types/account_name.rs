use core::{fmt, str::FromStr};

/// A nonempty account display name with surrounding whitespace removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountName(Box<str>);

/// An account display name was empty after trimming whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyAccountName;

impl fmt::Display for EmptyAccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("account name must not be empty after trimming whitespace")
    }
}

impl std::error::Error for EmptyAccountName {}

impl FromStr for AccountName {
    type Err = EmptyAccountName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let name = name.trim();
        if name.is_empty() {
            return Err(EmptyAccountName);
        }
        Ok(Self(name.into()))
    }
}

impl AsRef<str> for AccountName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_ref())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for AccountName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_ref())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AccountName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}
