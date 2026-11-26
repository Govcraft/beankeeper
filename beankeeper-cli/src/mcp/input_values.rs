//! Decode validated values from the standard MCP JSON wire types.

use std::{borrow::Cow, fmt, str::FromStr};

use agent_uri::AgentUri;
use beankeeper::{
    core::investments::PositiveQuantity,
    types::{
        AccountCode, AccountName, AccountType, Currency, DebitOrCredit, ExchangeRate, Sha256Hex,
    },
};
use chrono::NaiveDate;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::commands::{reference::TransactionReference, report_periods::CompareArg};
use crate::db::audit::setup::books::{
    BookPurpose, ChartTemplate, FiscalYearStart, ProfileRevision, SetupReference,
};
pub(crate) use crate::db::companies::CompanySlug;
use crate::db::{
    ActorLabel,
    investments::LotId,
    proposals::ProposalId,
    rules::{DescriptionPattern, RuleId},
};
use crate::{error::CliError, request::CalendarDateArg};

pub(crate) trait TextValue: fmt::Display + FromStr {
    const NAME: &'static str;

    fn schema() -> Schema;
}

#[derive(Debug)]
pub(crate) struct Text<T>(T);

impl<T> Text<T> {
    pub(crate) fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Text<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> AsRef<T> for Text<T> {
    fn as_ref(&self) -> &T {
        &self.0
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
    const NAME: &'static str = "SHA-256 digest";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[a-f0-9]{64}$"})
    }
}

impl TextValue for beankeeper::types::CashClass {
    const NAME: &'static str = "cash class";
    fn schema() -> Schema {
        json_schema!({"type": "string", "enum": [Self::NonCash.code(), Self::Cash.code(), Self::CashEquivalent.code()]})
    }
}

impl TextValue for AccountType {
    const NAME: &'static str = "account type";

    fn schema() -> Schema {
        json_schema!({"type": "string", "enum": ["asset", "liability", "equity", "revenue", "expense"]})
    }
}

impl TextValue for DebitOrCredit {
    const NAME: &'static str = "entry direction";

    fn schema() -> Schema {
        json_schema!({"type": "string", "enum": [Self::Debit.code(), Self::Credit.code()]})
    }
}

impl TextValue for Currency {
    const NAME: &'static str = "currency";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "description": "A three-letter ISO 4217 currency code with a defined decimal minor unit, such as USD, KRW, or TND. Fund and special-purpose codes are excluded."
        })
    }
}

impl TextValue for ExchangeRate {
    const NAME: &'static str = "ExchangeRate";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[0-9]+(\\.[0-9]+)?$",
            "description": "Positive decimal functional major units per native major unit, e.g. 0.85"
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

impl TextValue for DescriptionPattern {
    const NAME: &'static str = "description pattern";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "\\S"})
    }
}

impl TextValue for RuleId {
    const NAME: &'static str = "rule ID";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^rule_[0-7][0-9a-hjkmnp-tv-z]{25}$"})
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

impl TextValue for ActorLabel {
    const NAME: &'static str = "actor label";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "\\S"})
    }
}

impl TextValue for AgentUri {
    const NAME: &'static str = "agent URI";

    fn schema() -> Schema {
        json_schema!({"type": "string", "format": "uri", "pattern": "^agent://"})
    }
}

pub(crate) use crate::commands::investment::SecuritySymbol;

impl TextValue for SecuritySymbol {
    const NAME: &'static str = "security symbol";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[a-zA-Z0-9._:-]{1,32}$"})
    }
}

/// A date-only wire value with a resolved calendar date.
#[derive(Debug)]
pub(crate) struct CalendarDate(CalendarDateArg);

impl CalendarDate {
    pub(crate) fn date(&self) -> NaiveDate {
        self.0.into()
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
        input.parse().map(Self).map_err(Into::into)
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
pub(crate) struct Month(chrono::Month);

impl Month {
    pub(crate) fn into_inner(self) -> chrono::Month {
        self.0
    }

    pub(crate) fn get(&self) -> u32 {
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
pub(crate) struct PageLimit(i64);

impl PageLimit {
    pub(crate) fn get(&self) -> i64 {
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
pub(crate) struct PageOffset(i64);

impl PageOffset {
    pub(crate) fn get(&self) -> i64 {
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

pub(crate) use crate::commands::txn::MajorAmount as DecimalAmount;

impl TextValue for DecimalAmount {
    const NAME: &'static str = "amount";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[+-]?[0-9]+(\\.[0-9]*)?$"})
    }
}

impl TextValue for crate::db::holds::SettlementAmount {
    const NAME: &'static str = "posted amount";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[+]?[0-9]+(\\.[0-9]*)?$",
            "description": "Positive decimal in the hold's currency, in major units."
        })
    }
}

impl TextValue for crate::commands::txn_clear::StatementAmount {
    const NAME: &'static str = "statement amount";

    fn schema() -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[+-]?[0-9]+(\\.[0-9]*)?$",
            "description": "Nonzero decimal in major units. A minus sign denotes a withdrawal (credit), an explicit plus denotes a deposit (debit), and an unsigned amount clears either side."
        })
    }
}

impl TextValue for FiscalYearStart {
    const NAME: &'static str = "fiscal year start";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9]{2}-[0-9]{2}$",
            "description": "First fiscal day as MM-DD, valid in every year."})
    }
}

impl TextValue for AccountName {
    const NAME: &'static str = "account name";

    fn schema() -> Schema {
        json_schema!({"type": "string", "pattern": "\\S",
            "description": "Account display name, nonempty after trimming whitespace.",
            "examples": ["Operating Cash"]})
    }
}

impl TextValue for crate::commands::setup::OpeningDescription {
    const NAME: &'static str = "opening description";

    fn schema() -> Schema {
        json_schema!({"type": "string", "minLength": 1})
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

/// An array whose required first item survives decoding.
pub(crate) use crate::request::NonEmpty;

impl<T: Serialize> Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::deserialize(deserializer)?
            .try_into()
            .map_err(|_| de::Error::invalid_length(0, &"a non-empty array"))
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

impl TextValue for crate::db::statement::StatementMonth {
    const NAME: &'static str = "statement month-end";

    fn schema() -> Schema {
        json_schema!({"type": "string", "format": "date"})
    }
}

impl TextValue for crate::db::statement::StatementRef {
    const NAME: &'static str = "statement reference";

    fn schema() -> Schema {
        json_schema!({"type": "string", "minLength": 1})
    }
}
