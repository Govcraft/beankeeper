use core::fmt;
use core::str::FromStr;

/// Error type for currency operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CurrencyError {
    /// The provided string is not exactly 3 uppercase ASCII letters.
    InvalidCode { value: String },
    /// The code has valid format but is not a recognized ISO 4217 currency.
    UnknownCode { code: String },
}

impl fmt::Display for CurrencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCode { value } => {
                write!(
                    f,
                    "invalid currency code: {value:?} (expected 3 uppercase ASCII letters)"
                )
            }
            Self::UnknownCode { code } => {
                write!(f, "unknown currency code: {code}")
            }
        }
    }
}

impl std::error::Error for CurrencyError {}

/// A supported ISO 4217 currency with fixed minor unit precision.
///
/// Use the named variants or parse a currency code with [`Currency::from_code`].
///
/// # Examples
///
/// ```
/// use beankeeper::types::Currency;
///
/// let usd = Currency::USD;
/// assert_eq!(usd.code(), "USD");
/// assert_eq!(usd.minor_units(), 2);
///
/// let jpy = Currency::JPY;
/// assert_eq!(jpy.minor_units(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Currency {
    /// US Dollar.
    USD,
    /// Euro.
    EUR,
    /// British Pound Sterling.
    GBP,
    /// Japanese Yen.
    JPY,
    /// Swiss Franc.
    CHF,
    /// Canadian Dollar.
    CAD,
    /// Australian Dollar.
    AUD,
    /// Bahraini Dinar.
    BHD,
    /// Kuwaiti Dinar.
    KWD,
    /// Mexican Peso.
    MXN,
}

impl Currency {
    /// Creates a `Currency` from a string code, validating format.
    ///
    /// The code must be exactly 3 uppercase ASCII letters.
    ///
    /// # Errors
    ///
    /// Returns [`CurrencyError::InvalidCode`] for malformed text and
    /// [`CurrencyError::UnknownCode`] for an unsupported code.
    pub fn from_code(code: &str) -> Result<Self, CurrencyError> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(CurrencyError::InvalidCode {
                value: code.to_owned(),
            });
        }

        match bytes {
            b"USD" => Ok(Self::USD),
            b"EUR" => Ok(Self::EUR),
            b"GBP" => Ok(Self::GBP),
            b"JPY" => Ok(Self::JPY),
            b"CHF" => Ok(Self::CHF),
            b"CAD" => Ok(Self::CAD),
            b"AUD" => Ok(Self::AUD),
            b"BHD" => Ok(Self::BHD),
            b"KWD" => Ok(Self::KWD),
            b"MXN" => Ok(Self::MXN),
            _ => Err(CurrencyError::UnknownCode {
                code: code.to_owned(),
            }),
        }
    }

    /// Returns the 3-byte currency code.
    #[must_use]
    pub const fn code_bytes(&self) -> [u8; 3] {
        let bytes = self.code().as_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    /// Returns the currency code as a string slice.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::USD => "USD",
            Self::EUR => "EUR",
            Self::GBP => "GBP",
            Self::JPY => "JPY",
            Self::CHF => "CHF",
            Self::CAD => "CAD",
            Self::AUD => "AUD",
            Self::BHD => "BHD",
            Self::KWD => "KWD",
            Self::MXN => "MXN",
        }
    }

    /// Returns the number of minor unit decimal places for this currency.
    ///
    /// For example, USD has 2 (cents), JPY has 0, BHD has 3.
    #[must_use]
    pub const fn minor_units(&self) -> u8 {
        match self {
            Self::JPY => 0,
            Self::BHD | Self::KWD => 3,
            Self::USD | Self::EUR | Self::GBP | Self::CHF | Self::CAD | Self::AUD | Self::MXN => 2,
        }
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

// Human-facing parsing accepts whitespace and ASCII case differences.
impl FromStr for Currency {
    type Err = CurrencyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let code = s.trim();
        if code.bytes().any(|byte| byte.is_ascii_lowercase()) {
            Self::from_code(&code.to_ascii_uppercase())
        } else {
            Self::from_code(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_has_two_minor_units() {
        assert_eq!(Currency::USD.minor_units(), 2);
    }

    #[test]
    fn jpy_has_zero_minor_units() {
        assert_eq!(Currency::JPY.minor_units(), 0);
    }

    #[test]
    fn bhd_has_three_minor_units() {
        assert_eq!(Currency::BHD.minor_units(), 3);
    }

    #[test]
    fn from_code_valid_uppercase() {
        let usd = Currency::from_code("USD");
        assert!(usd.is_ok());
        assert_eq!(usd.ok(), Some(Currency::USD));
    }

    #[test]
    fn from_code_rejects_lowercase() {
        assert!(Currency::from_code("usd").is_err());
    }

    #[test]
    fn from_code_rejects_wrong_length() {
        assert!(Currency::from_code("US").is_err());
        assert!(Currency::from_code("USDD").is_err());
    }

    #[test]
    fn from_code_rejects_digits() {
        assert!(Currency::from_code("U1D").is_err());
    }

    #[test]
    fn display_shows_code() {
        assert_eq!(format!("{}", Currency::USD), "USD");
        assert_eq!(format!("{}", Currency::JPY), "JPY");
    }

    #[test]
    fn from_str_round_trips() {
        assert_eq!("USD".parse(), Ok(Currency::USD));
    }

    #[test]
    fn code_returns_str() {
        assert_eq!(Currency::EUR.code(), "EUR");
    }

    #[test]
    fn code_bytes_returns_array() {
        assert_eq!(Currency::GBP.code_bytes(), *b"GBP");
    }

    #[test]
    fn unknown_code_returns_error() {
        let result = Currency::from_code("XYZ");
        assert!(matches!(result, Err(CurrencyError::UnknownCode { .. })));
    }

    #[test]
    fn equality() {
        assert_eq!(Currency::USD, Currency::USD);
        assert_ne!(Currency::USD, Currency::EUR);
    }

    #[test]
    fn kwd_has_three_minor_units() {
        assert_eq!(Currency::KWD.minor_units(), 3);
    }

    #[test]
    fn mxn_has_two_minor_units() {
        assert_eq!(Currency::MXN.minor_units(), 2);
        assert_eq!(Currency::MXN.code(), "MXN");
    }

    #[test]
    fn mxn_from_code() {
        let mxn = Currency::from_code("MXN");
        assert!(mxn.is_ok());
        assert_eq!(mxn.ok(), Some(Currency::MXN));
    }

    #[test]
    fn error_display() {
        let err = CurrencyError::InvalidCode {
            value: "usd".to_owned(),
        };
        assert!(format!("{err}").contains("invalid currency code"));
    }
}
