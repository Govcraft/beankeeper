use std::{fmt, str::FromStr};

use beankeeper::types::Currency;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// An assigned ISO 3166-1 alpha-2 country code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CountryCode([u8; 2]);

/// The country code was not an assigned uppercase ISO 3166-1 alpha-2 code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountryCodeError;

impl fmt::Display for CountryCodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected an assigned uppercase ISO 3166-1 alpha-2 country code")
    }
}

impl std::error::Error for CountryCodeError {}

impl Default for CountryCode {
    fn default() -> Self {
        Self(*b"US")
    }
}

impl FromStr for CountryCode {
    type Err = CountryCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let code = <[u8; 2]>::try_from(value.as_bytes()).map_err(|_| CountryCodeError)?;
        if !COUNTRIES
            .split_ascii_whitespace()
            .any(|country| country == value)
        {
            return Err(CountryCodeError);
        }
        Ok(Self(code))
    }
}

impl TryFrom<String> for CountryCode {
    type Error = CountryCodeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<CountryCode> for String {
    fn from(value: CountryCode) -> Self {
        value.to_string()
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [first, second] = self.0.map(char::from);
        write!(formatter, "{first}{second}")
    }
}

impl JsonSchema for CountryCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CountryCode".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "enum": COUNTRIES.split_ascii_whitespace().collect::<Vec<_>>()
        })
    }
}

impl CountryCode {
    pub(super) fn default_currency(self) -> Currency {
        match &self.0 {
            b"AU" => Currency::AUD,
            b"BH" => Currency::BHD,
            b"CA" => Currency::CAD,
            b"CH" | b"LI" => Currency::CHF,
            b"GB" | b"GG" | b"IM" | b"JE" => Currency::GBP,
            b"JP" => Currency::JPY,
            b"KW" => Currency::KWD,
            b"MX" => Currency::MXN,
            b"AD" | b"AT" | b"BE" | b"BG" | b"CY" | b"DE" | b"EE" | b"ES" | b"FI" | b"FR"
            | b"GR" | b"HR" | b"IE" | b"IT" | b"LT" | b"LU" | b"LV" | b"MC" | b"MT" | b"NL"
            | b"PT" | b"SI" | b"SK" | b"SM" | b"VA" => Currency::EUR,
            _ => Currency::USD,
        }
    }
}

// Assigned alpha-2 codes from the ISO 3166-1 list.
const COUNTRIES: &str = "
AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE
BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ CA CC CD
CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM
DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF
GG GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM HN HR HT HU
ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN
KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME
MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA
NC NE NF NG NI NL NO NP NR NU NZ OM PA PE PF PG PH PK PL PM
PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI
SJ SK SL SM SN SO SR SS ST SV SX SY SZ TC TD TF TG TH TJ TK
TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA VC VE VG VI
VN VU WF WS YE YT ZA ZM ZW
";
