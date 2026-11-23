//! Resolve the process currency setting at the request boundary.

use std::ffi::OsString;

use beankeeper::types::Currency;

use crate::error::CliError;

/// Currency selection at the boundary, before any environment text is decoded.
/// The resulting request carries only `Currency`.
pub(crate) struct CurrencySetting(Source);

enum Source {
    Explicit(Currency),
    Environment {
        value: Option<OsString>,
        fallback: Currency,
    },
}

impl Default for CurrencySetting {
    fn default() -> Self {
        Self(Source::Environment {
            value: std::env::var_os("BEANKEEPER_CURRENCY"),
            fallback: Currency::USD,
        })
    }
}

impl From<Option<Currency>> for CurrencySetting {
    fn from(currency: Option<Currency>) -> Self {
        currency.map_or_else(Self::default, |currency| Self(Source::Explicit(currency)))
    }
}

impl TryFrom<CurrencySetting> for Option<Currency> {
    type Error = CliError;

    fn try_from(setting: CurrencySetting) -> Result<Self, Self::Error> {
        match setting.0 {
            Source::Explicit(currency) => Ok(Some(currency)),
            Source::Environment { value, .. } => value
                .map(|raw| {
                    let raw = raw.into_string().map_err(|_| {
                        CliError::Validation("BEANKEEPER_CURRENCY must be valid UTF-8".into())
                    })?;
                    raw.parse().map_err(|error| {
                        CliError::Validation(format!("BEANKEEPER_CURRENCY: {error}"))
                    })
                })
                .transpose(),
        }
    }
}

impl TryFrom<CurrencySetting> for Currency {
    type Error = CliError;

    fn try_from(setting: CurrencySetting) -> Result<Self, Self::Error> {
        let fallback = match setting.0 {
            Source::Explicit(currency)
            | Source::Environment {
                fallback: currency, ..
            } => currency,
        };
        Ok(Option::<Self>::try_from(setting)?.unwrap_or(fallback))
    }
}
