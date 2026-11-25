use core::fmt;
use core::str::FromStr;

mod catalog;

use catalog::{CURRENCIES, UNSUPPORTED_CODES};

/// Error type for currency operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CurrencyError {
    /// The provided string is not exactly 3 uppercase ASCII letters.
    InvalidCode { value: String },
    /// The code has valid format but is absent from the bundled catalog.
    UnknownCode { code: String },
    /// The code is registered but is a fund or has no defined decimal minor unit.
    UnsupportedCode { code: String },
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
            Self::UnsupportedCode { code } => {
                write!(
                    f,
                    "unsupported currency code: {code} (fund or no defined minor unit)"
                )
            }
        }
    }
}

impl std::error::Error for CurrencyError {}

/// An ISO 4217 currency with a defined decimal minor unit.
///
/// Parse human input with [`FromStr`] or use [`TryFrom<&str>`] for a canonical
/// uppercase code. The code, numeric identifier, and precision come from the
/// same offline registry entry.
///
/// # Examples
/// ```
/// use beankeeper::types::{Currency, CurrencyError};
///
/// let won: Currency = "krw".parse()?;
/// assert_eq!(won.code(), "KRW");
/// assert_eq!(won.numeric_code(), 410);
/// assert_eq!(won.minor_units(), 0);
/// # Ok::<(), CurrencyError>(())
/// ```
///
/// ```compile_fail
/// use beankeeper::types::Currency;
///
/// let invalid = Currency("ZZZ");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Currency(&'static Definition);

#[derive(PartialEq, Eq, Hash)]
struct Definition {
    code: &'static str,
    numeric_code: u16,
    minor_units: u8,
}

impl Definition {
    const fn registered(code: &'static str, numeric_code: u16, minor_units: u8) -> Self {
        // This constructor is only called from the static catalog.
        let [a, b, c] = code.as_bytes() else {
            panic!("registered currency code must be three letters");
        };
        assert!(a.is_ascii_uppercase() && b.is_ascii_uppercase() && c.is_ascii_uppercase());
        assert!(numeric_code <= 999 && minor_units <= 4);
        Self {
            code,
            numeric_code,
            minor_units,
        }
    }
}

impl Currency {
    /// US Dollar.
    pub const USD: Self = Self::registered_code(*b"USD");
    /// Euro.
    pub const EUR: Self = Self::registered_code(*b"EUR");
    /// British Pound Sterling.
    pub const GBP: Self = Self::registered_code(*b"GBP");
    /// Japanese Yen.
    pub const JPY: Self = Self::registered_code(*b"JPY");
    /// Swiss Franc.
    pub const CHF: Self = Self::registered_code(*b"CHF");
    /// Canadian Dollar.
    pub const CAD: Self = Self::registered_code(*b"CAD");
    /// Australian Dollar.
    pub const AUD: Self = Self::registered_code(*b"AUD");
    /// Bahraini Dinar.
    pub const BHD: Self = Self::registered_code(*b"BHD");
    /// Kuwaiti Dinar.
    pub const KWD: Self = Self::registered_code(*b"KWD");
    /// Mexican Peso.
    pub const MXN: Self = Self::registered_code(*b"MXN");

    const fn registered_code(code: [u8; 3]) -> Self {
        match Self::lookup(code) {
            Some(currency) => currency,
            None => panic!("named currency must be in the ISO 4217 catalog"),
        }
    }

    const fn lookup(code: [u8; 3]) -> Option<Self> {
        const fn key([a, b, c]: [u8; 3]) -> u32 {
            u32::from_be_bytes([0, a, b, c])
        }

        let target = key(code);
        let mut start = 0;
        let mut end = CURRENCIES.len();
        while start < end {
            let mid = start + (end - start) / 2;
            let currency = CURRENCIES[mid];
            let current = key(currency.code_bytes());
            if target < current {
                end = mid;
            } else if target > current {
                start = mid + 1;
            } else {
                return Some(currency);
            }
        }
        None
    }

    /// Parses an exact, uppercase currency code.
    ///
    /// # Errors
    /// Returns the same errors as [`TryFrom<&str>`].
    pub fn from_code(code: &str) -> Result<Self, CurrencyError> {
        Self::try_from(code)
    }

    /// Returns the 3-byte currency code.
    #[must_use]
    pub const fn code_bytes(&self) -> [u8; 3] {
        let bytes = self.code().as_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    /// Returns the uppercase ISO 4217 alphabetic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.0.code
    }

    /// Returns the ISO 4217 numeric code. Display with three digits, e.g. 036 for AUD.
    #[must_use]
    pub const fn numeric_code(&self) -> u16 {
        self.0.numeric_code
    }

    /// Returns the number of decimal places in a major unit of this currency.
    #[must_use]
    pub const fn minor_units(&self) -> u8 {
        self.0.minor_units
    }
}

impl Default for Currency {
    fn default() -> Self {
        Self::USD
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl TryFrom<&str> for Currency {
    type Error = CurrencyError;

    fn try_from(code: &str) -> Result<Self, Self::Error> {
        let invalid = || CurrencyError::InvalidCode {
            value: code.to_owned(),
        };
        let bytes: [u8; 3] = code.as_bytes().try_into().map_err(|_| invalid())?;
        if !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(invalid());
        }

        Self::lookup(bytes).ok_or_else(|| {
            if UNSUPPORTED_CODES.binary_search(&code).is_ok() {
                CurrencyError::UnsupportedCode {
                    code: code.to_owned(),
                }
            } else {
                CurrencyError::UnknownCode {
                    code: code.to_owned(),
                }
            }
        })
    }
}

// Human-facing parsing accepts whitespace and ASCII case differences.
impl FromStr for Currency {
    type Err = CurrencyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let code = s.trim();
        if code.bytes().any(|byte| byte.is_ascii_lowercase()) {
            Self::try_from(code.to_ascii_uppercase().as_str())
        } else {
            Self::try_from(code)
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
    fn every_catalog_currency_round_trips() {
        let mut numeric_codes = std::collections::BTreeSet::new();
        for &currency in CURRENCIES {
            let code = currency.code();
            assert_eq!(Currency::try_from(code), Ok(currency));
            assert_eq!(currency.to_string(), code);
            assert_eq!(code.to_ascii_lowercase().parse(), Ok(currency));
            assert!(numeric_codes.insert(currency.numeric_code()), "{code}");
        }
    }

    #[test]
    fn newly_supported_currencies_have_iso_numbers_and_scales() -> Result<(), CurrencyError> {
        for (code, number, scale) in [("KRW", 410, 0), ("INR", 356, 2), ("TND", 788, 3)] {
            let currency: Currency = code.parse()?;
            assert_eq!(currency.numeric_code(), number, "{code}");
            assert_eq!(currency.minor_units(), scale, "{code}");
        }
        assert_eq!(Currency::AUD.numeric_code(), 36);
        assert_eq!(" usd ".parse(), Ok(Currency::default()));
        Ok(())
    }

    #[test]
    fn excluded_iso_codes_return_a_distinct_error() {
        for &code in UNSUPPORTED_CODES {
            assert_eq!(
                Currency::try_from(code),
                Err(CurrencyError::UnsupportedCode { code: code.into() })
            );
        }
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
