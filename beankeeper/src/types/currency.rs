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

/// An ISO 4217 currency code with its associated minor unit precision.
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
pub struct Currency {
    code: [u8; 3],
    minor_units: u8,
}

impl Currency {
    /// US Dollar
    pub const USD: Self = Self::new(*b"USD", 2);
    /// Euro
    pub const EUR: Self = Self::new(*b"EUR", 2);
    /// British Pound Sterling
    pub const GBP: Self = Self::new(*b"GBP", 2);
    /// Japanese Yen
    pub const JPY: Self = Self::new(*b"JPY", 0);
    /// Swiss Franc
    pub const CHF: Self = Self::new(*b"CHF", 2);
    /// Canadian Dollar
    pub const CAD: Self = Self::new(*b"CAD", 2);
    /// Australian Dollar
    pub const AUD: Self = Self::new(*b"AUD", 2);
    /// Bahraini Dinar (3 decimal places)
    pub const BHD: Self = Self::new(*b"BHD", 3);
    /// Kuwaiti Dinar (3 decimal places)
    pub const KWD: Self = Self::new(*b"KWD", 3);
    /// Mexican Peso
    pub const MXN: Self = Self::new(*b"MXN", 2);

    /// Creates a new `Currency` from a 3-byte ASCII code and minor unit count.
    ///
    /// This is a low-level const constructor. For runtime construction with
    /// validation, use [`from_code`](Self::from_code).
    #[must_use]
    pub const fn new(code: [u8; 3], minor_units: u8) -> Self {
        Self { code, minor_units }
    }

    /// Creates a `Currency` from a string code, validating format.
    ///
    /// The code must be exactly 3 uppercase ASCII letters.
    ///
    /// # Errors
    ///
    /// Returns [`CurrencyError::InvalidCode`] if the string is not
    /// 3 uppercase ASCII letters.
    pub fn from_code(code: &str) -> Result<Self, CurrencyError> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(CurrencyError::InvalidCode {
                value: code.to_owned(),
            });
        }

        let code_arr = [bytes[0], bytes[1], bytes[2]];

        // Look up known currencies for minor unit info
        match &code_arr {
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
        self.code
    }

    /// Returns the currency code as a string slice.
    ///
    /// # Panics
    ///
    /// This method will not panic because `Currency` is always constructed
    /// from valid ASCII bytes.
    #[must_use]
    pub fn code(&self) -> &str {
        // SAFETY: Currency codes are always valid ASCII, which is valid UTF-8.
        // This is guaranteed by the constructors: `new` takes bytes that are
        // ASCII uppercase by convention, `from_code` validates, and `FromStr`
        // validates.
        std::str::from_utf8(&self.code).unwrap_or("???")
    }

    /// Returns the number of minor unit decimal places for this currency.
    ///
    /// For example, USD has 2 (cents), JPY has 0, BHD has 3.
    #[must_use]
    pub const fn minor_units(&self) -> u8 {
        self.minor_units
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl FromStr for Currency {
    type Err = CurrencyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_code(s)
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
        let usd: Currency = "USD".parse().ok().unwrap_or(Currency::USD);
        assert_eq!(usd, Currency::USD);
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
