//! Decoded currency selections and the environment default shared by adapters.

use beankeeper::types::{Currency, CurrencyError};

use crate::error::CliError;

/// A decoded currency flag with any validation error retained for the command envelope.
#[derive(Debug, Clone)]
pub struct CurrencyArg(Result<Currency, CurrencyError>);

impl CurrencyArg {
    pub(crate) fn configured() -> Option<Self> {
        std::env::var("BEANKEEPER_CURRENCY")
            .ok()
            .map(|value| Self::decode(&value))
    }

    pub(crate) fn decode(raw: &str) -> Self {
        Self(parse_currency(raw))
    }

    pub(crate) fn resolve_optional(value: Option<&Self>) -> Result<Option<Currency>, CliError> {
        value.map(Self::resolve).transpose()
    }

    /// Returns the validated currency.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Validation`] for an unsupported currency code.
    pub fn resolve(&self) -> Result<Currency, CliError> {
        self.0
            .as_ref()
            .copied()
            .map_err(|error| CliError::Validation(error.to_string()))
    }
}

impl Default for CurrencyArg {
    fn default() -> Self {
        Self::configured().unwrap_or_else(|| Currency::USD.into())
    }
}

impl TryFrom<CurrencyArg> for Currency {
    type Error = CliError;

    fn try_from(value: CurrencyArg) -> Result<Self, Self::Error> {
        value
            .0
            .map_err(|error| CliError::Validation(error.to_string()))
    }
}

impl From<Currency> for CurrencyArg {
    fn from(currency: Currency) -> Self {
        Self(Ok(currency))
    }
}

impl std::str::FromStr for CurrencyArg {
    type Err = std::convert::Infallible;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self::decode(raw))
    }
}

impl serde::Serialize for CurrencyArg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Ok(currency) => serializer.collect_str(currency),
            Err(error) => Err(serde::ser::Error::custom(error)),
        }
    }
}

impl<'de> serde::Deserialize<'de> for CurrencyArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = <String as serde::Deserialize>::deserialize(deserializer)?;
        parse_currency(&code)
            .map(Self::from)
            .map_err(serde::de::Error::custom)
    }
}

/// Parse a currency flag or its environment default.
pub(crate) fn parse_currency(raw: &str) -> Result<Currency, CurrencyError> {
    let raw = raw.trim();
    if raw.bytes().any(|byte| byte.is_ascii_lowercase()) {
        raw.to_ascii_uppercase().parse()
    } else {
        raw.parse()
    }
}
