//! Decode validated values from the standard MCP JSON wire types.

use std::{borrow::Cow, fmt, str::FromStr};

use agent_uri::AgentUri;
use beankeeper::{
    core::investments::PositiveQuantity,
    types::{AccountCode, AccountType, Currency, Sha256Hex},
};
use chrono::NaiveDate;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::commands::{reference::TransactionReference, report_periods::CompareArg};
use crate::db::audit::setup::books::{
    BookPurpose, ChartTemplate, FiscalYearStart, ProfileRevision, SetupReference,
};
pub(super) use crate::db::companies::CompanySlug;
use crate::db::{
    investments::{LotId, valid_symbol},
    proposals::ProposalId,
};
use crate::error::CliError;

pub(super) trait TextValue: fmt::Display + FromStr {
    const NAME: &'static str;

    fn schema() -> Schema;
}

#[derive(Debug)]
pub(super) struct Text<T>(T);

impl<T> Text<T> {
    pub(super) fn into_inner(self) -> T {
        self.0
    }
}

impl<T: TextValue> fmt::Display for Text<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de, T: TextValue> Deserialize<'de> for Text<T>
where
    T::Err: fmt::Display,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse()
            .map(Self)
            .map_err(|error| de::Error::custom(format_args!("invalid {}: {error}", T::NAME)))
    }
}

impl<T: TextValue> Serialize for Text<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<T: TextValue> JsonSchema for Text<T> {
    fn schema_name() -> Cow<'static, str> {
        T::NAME.into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        T::schema()
    }
}

impl TextValue for AccountCode {
    const NAME: &'static str = "account code";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9.-]+$"})
    }
}

impl TextValue for Sha256Hex {
    const NAME: &'static str = "opening preview fingerprint";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[a-f0-9]{64}$"})
    }
}

impl TextValue for AccountType {
    const NAME: &'static str = "account type";

    fn schema() -> Schema {
        json_schema!({"type": "string", "enum": ["asset", "liability", "equity", "revenue", "expense"]})
    }
}

impl TextValue for Currency {
    const NAME: &'static str = "currency";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "enum": ["AUD", "BHD", "CAD", "CHF", "EUR", "GBP", "JPY", "KWD", "MXN", "USD"],
            "description": "A supported ISO 4217 currency code."
        })
    }
}

impl TextValue for TransactionReference {
    const NAME: &'static str = "transaction reference";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "\\S"})
    }
}

impl TextValue for CompareArg {
    const NAME: &'static str = "comparison window";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(previous|year-ago|prior-period|prior-year|[0-9]{4}-[0-9]{2}-[0-9]{2}\\.\\.[0-9]{4}-[0-9]{2}-[0-9]{2})$",
            "description": "A relative period or inclusive YYYY-MM-DD..YYYY-MM-DD range."
        })
    }
}

impl TextValue for ProposalId {
    const NAME: &'static str = "proposal ID";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^proposal_[0-7][0-9a-hjkmnp-tv-z]{25}$"})
    }
}

impl TextValue for PositiveQuantity {
    const NAME: &'static str = "share quantity";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(?=.*[1-9])[0-9]+(\\.[0-9]{1,8})?$",
            "description": "Positive quantity with up to eight decimal places."
        })
    }
}

impl TextValue for AgentUri {
    const NAME: &'static str = "agent URI";

    fn schema() -> Schema {
        json_schema!({"type": "string", "format": "uri", "pattern": "^agent://"})
    }
}

#[derive(Debug)]
pub(super) struct SecuritySymbol(Box<str>);

impl fmt::Display for SecuritySymbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SecuritySymbol {
    type Err = CliError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if valid_symbol(input) {
            Ok(Self(input.into()))
        } else {
            Err(CliError::Validation(
                "expected 1-32 ASCII letters, digits, '.', '-', '_' or ':'".into(),
            ))
        }
    }
}

impl TextValue for SecuritySymbol {
    const NAME: &'static str = "security symbol";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[a-zA-Z0-9._:-]{1,32}$"})
    }
}

/// A date-only wire value with a resolved calendar date.
#[derive(Debug)]
pub(super) struct CalendarDate(NaiveDate);

impl CalendarDate {
    pub(super) fn date(&self) -> NaiveDate {
        self.0
    }
}

impl fmt::Display for CalendarDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.date().fmt(formatter)
    }
}

impl FromStr for CalendarDate {
    type Err = CliError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.len() != 10
            || !input.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                _ => byte.is_ascii_digit(),
            })
        {
            return Err(CliError::Validation("expected YYYY-MM-DD".into()));
        }
        NaiveDate::parse_from_str(input, "%Y-%m-%d")
            .map(Self)
            .map_err(|error| CliError::Validation(format!("expected YYYY-MM-DD: {error}")))
    }
}

impl TextValue for CalendarDate {
    const NAME: &'static str = "calendar date";

    fn schema() -> Schema {
        json_schema!({"type": "string", "format": "date", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"})
    }
}

impl TextValue for LotId {
    const NAME: &'static str = "lot ID";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^lot_[0-7][0-9a-hjkmnp-tv-z]{25}$"})
    }
}

#[derive(Debug)]
pub(super) struct Month(chrono::Month);

impl Month {
    pub(super) fn get(&self) -> u32 {
        self.0.number_from_month()
    }
}

impl<'de> Deserialize<'de> for Month {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let month = u8::deserialize(deserializer)?;
        chrono::Month::try_from(month)
            .map(Self)
            .map_err(|_| de::Error::custom("month must be an integer from 1 to 12"))
    }
}

impl Serialize for Month {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.get())
    }
}

impl JsonSchema for Month {
    fn schema_name() -> Cow<'static, str> {
        "month".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "minimum": 1, "maximum": 12})
    }
}

/// Page size. SQLite treats negative limits as unlimited.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(transparent)]
pub(super) struct PageLimit(i64);

impl PageLimit {
    pub(super) fn get(&self) -> i64 {
        self.0
    }
}

impl Serialize for PageLimit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.get())
    }
}

/// Rows to skip before returning a page.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(transparent)]
pub(super) struct PageOffset(i64);

impl PageOffset {
    pub(super) fn get(&self) -> i64 {
        self.0
    }
}

impl Serialize for PageOffset {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.get())
    }
}

impl TextValue for CompanySlug {
    const NAME: &'static str = "company slug";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[a-z0-9][a-z0-9-]{0,63}$"})
    }
}

/// A plain decimal in major currency units, before applying a currency scale.
#[derive(Debug)]
pub(super) struct DecimalAmount(Box<str>);

impl DecimalAmount {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DecimalAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DecimalAmount {
    type Err = CliError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let unsigned = input.strip_prefix(['+', '-']).unwrap_or(input);
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let digits = |value: &str| value.bytes().all(|digit| digit.is_ascii_digit());
        if whole.is_empty() || !digits(whole) || !digits(fraction) {
            return Err(CliError::Validation(
                "expected a plain decimal in major units".into(),
            ));
        }
        Ok(Self(input.into()))
    }
}

impl TextValue for DecimalAmount {
    const NAME: &'static str = "amount";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[+-]?[0-9]+(\\.[0-9]*)?$"})
    }
}

impl TextValue for FiscalYearStart {
    const NAME: &'static str = "fiscal year start";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9]{2}-[0-9]{2}$",
            "description": "First fiscal day as MM-DD, valid in every year."})
    }
}

impl TextValue for SetupReference {
    const NAME: &'static str = "setup reference";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "\\S", "maxLength": 128})
    }
}

impl JsonSchema for BookPurpose {
    fn schema_name() -> Cow<'static, str> {
        "book purpose".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "enum": ["personal", "business"]})
    }
}

impl JsonSchema for ChartTemplate {
    fn schema_name() -> Cow<'static, str> {
        "chart template".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "enum": ["personal-v1", "business-v1"]})
    }
}

impl JsonSchema for ProfileRevision {
    fn schema_name() -> Cow<'static, str> {
        "profile revision".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "format": "int64", "minimum": 1})
    }
}

impl JsonSchema for crate::db::audit::setup::Generation {
    fn schema_name() -> Cow<'static, str> {
        "setup generation".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "format": "int64", "minimum": 1})
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    pub(super) fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = Vec::deserialize(deserializer)?;
        if values.is_empty() {
            return Err(de::Error::invalid_length(0, &"a non-empty array"));
        }
        Ok(Self(values))
    }
}

impl<T: JsonSchema> JsonSchema for NonEmpty<T> {
    fn schema_name() -> Cow<'static, str> {
        format!("NonEmpty_{}", T::schema_name()).into()
    }

    fn schema_id() -> Cow<'static, str> {
        format!("NonEmpty<{}>", T::schema_id()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "array", "minItems": 1, "items": generator.subschema_for::<T>()})
    }
}
