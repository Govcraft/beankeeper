//! Exact configured UK invoice VAT in pounds sterling.

use std::{fmt, str::FromStr};

use beankeeper::types::{AccountCode, Currency};
use chrono::{Datelike, NaiveDate};
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{amounts::PositiveMinorUnits, documents::DocumentKind},
    error::CliError,
};

macro_rules! tax_id {
    ($name:ident, $prefix:literal, $description:literal) => {
        #[doc = $description]
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(MagicTypeId);

        impl $name {
            pub(in crate::db) fn generate() -> Self {
                Self($prefix.create_type_id::<V7>())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = TaxError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let id = raw
                    .parse::<MagicTypeId>()
                    .map_err(|_| TaxError::Identifier($prefix))?;
                if id.prefix_str().as_deref() == Ok($prefix) {
                    Ok(Self(id))
                } else {
                    Err(TaxError::Identifier($prefix))
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = TaxError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                raw.parse()
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.to_string()
            }
        }

        impl FromSql for $name {
            fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
                raw.as_str()?
                    .parse()
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(self.to_string().into())
            }
        }
    };
}

tax_id!(TaxCodeId, "taxcode", "Stable company tax code identity.");
tax_id!(
    TaxRateRevisionId,
    "taxrate",
    "Identity of one effective-dated immutable tax revision."
);

/// Human search code distinct from the system identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct TaxCodeName(Box<str>);

impl TaxCodeName {
    /// The canonical upper-case code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaxCodeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TaxCodeName {
    type Err = TaxError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let code = raw.trim();
        if !code.is_empty()
            && code.len() <= 32
            && code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            Ok(Self(code.to_ascii_uppercase().into()))
        } else {
            Err(TaxError::InvalidCode)
        }
    }
}

impl TryFrom<String> for TaxCodeName {
    type Error = TaxError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<TaxCodeName> for String {
    fn from(code: TaxCodeName) -> Self {
        code.0.into()
    }
}

const RATE_UNIT: i64 = 1_000_000;

/// A percentage from 0 through 100 with up to four exact decimal places.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct TaxRate(i64);

impl TryFrom<i64> for TaxRate {
    type Error = TaxError;

    fn try_from(raw: i64) -> Result<Self, Self::Error> {
        if (0..=RATE_UNIT).contains(&raw) {
            Ok(Self(raw))
        } else {
            Err(TaxError::InvalidRate)
        }
    }
}

impl FromStr for TaxRate {
    type Err = TaxError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let decimal = Decimal::from_str_exact(raw)
            .map_err(|_| TaxError::InvalidRate)?
            .normalize();
        let place = 4_u32
            .checked_sub(decimal.scale())
            .ok_or(TaxError::InvalidRate)?;
        let ppm = decimal
            .mantissa()
            .checked_mul(10_i128.pow(place))
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(TaxError::InvalidRate)?;
        ppm.try_into()
    }
}

impl TryFrom<String> for TaxRate {
    type Error = TaxError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<TaxRate> for String {
    fn from(rate: TaxRate) -> Self {
        rate.to_string()
    }
}

impl fmt::Display for TaxRate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Decimal::new(self.0, 4).normalize().fmt(formatter)
    }
}

/// The applicable VAT treatment supplied by the ledger owner.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum TaxTreatment {
    /// A positive configured percentage is charged.
    Taxable,
    /// A zero-rated supply recorded separately from exemptions.
    ZeroRated,
    /// An exempt supply with no VAT charged.
    Exempt,
}

impl TaxTreatment {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Taxable => "taxable",
            Self::ZeroRated => "zero_rated",
            Self::Exempt => "exempt",
        }
    }
}

/// Whether the supplied line price already contains VAT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriceTaxMode {
    #[default]
    Exclusive,
    Inclusive,
}

/// The VAT code and price treatment selected on one draft line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LineTax {
    pub code: TaxCodeId,
    #[serde(default)]
    pub price_mode: PriceTaxMode,
}

/// Effective dated record of owner-selected tax treatment and controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateTerms {
    #[schemars(with = "String")]
    pub effective_from: NaiveDate,
    pub treatment: TaxTreatment,
    pub rate: TaxRate,
    #[schemars(with = "Option<String>")]
    pub output_account: Option<AccountCode>,
    #[schemars(with = "Option<String>")]
    pub input_account: Option<AccountCode>,
}

impl RateTerms {
    pub(in crate::db) fn validate(&self) -> Result<(), TaxError> {
        if !(0..=9999).contains(&self.effective_from.year()) {
            return Err(TaxError::InvalidDate);
        }
        let allowed = match self.treatment {
            TaxTreatment::Taxable => self.rate.0 > 0 && self.output_account.is_some(),
            TaxTreatment::ZeroRated | TaxTreatment::Exempt => {
                self.rate.0 == 0 && self.output_account.is_none() && self.input_account.is_none()
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(TaxError::TreatmentAccounts)
        }
    }
}

/// Canonical historical rule applied to a document line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateRevision {
    pub code: TaxCodeId,
    pub name: TaxCodeName,
    pub id: TaxRateRevisionId,
    pub terms: RateTerms,
}

/// Exact total used on a line of an issued document or its credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineAmounts {
    pub net_minor: i64,
    pub vat_minor: i64,
    pub gross_minor: i64,
}

/// Applied VAT amounts and settings frozen with a document journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxedLine {
    pub number: u32,
    pub revision: RateRevision,
    pub price_mode: PriceTaxMode,
    #[serde(flatten)]
    pub amounts: LineAmounts,
}

impl TaxedLine {
    pub(in crate::db) fn control(&self, kind: DocumentKind) -> Option<&AccountCode> {
        match kind {
            DocumentKind::Invoice => self.revision.terms.output_account.as_ref(),
            DocumentKind::Bill => self.revision.terms.input_account.as_ref(),
        }
    }

    pub(in crate::db) fn credit(
        &self,
        credited: i64,
        amount: PositiveMinorUnits,
    ) -> Result<Self, TaxError> {
        let gross_minor = amount.minor_units();
        let prior = self.rounded_credit_tax(credited)?;
        let total = credited.checked_add(gross_minor).ok_or(TaxError::Amount)?;
        let vat_minor = self
            .rounded_credit_tax(total)?
            .checked_sub(prior)
            .ok_or(TaxError::Amount)?;
        Ok(Self {
            amounts: LineAmounts {
                vat_minor,
                gross_minor,
                net_minor: gross_minor - vat_minor,
            },
            ..self.clone()
        })
    }

    fn rounded_credit_tax(&self, gross: i64) -> Result<i64, TaxError> {
        if !(0..=self.amounts.gross_minor).contains(&gross) || self.amounts.gross_minor == 0 {
            return Err(TaxError::Amount);
        }
        round_ratio(gross, self.amounts.vat_minor, self.amounts.gross_minor)
    }
}

/// Calculate nearest-penny VAT for one GBP invoice line.
///
/// Amounts are rounded only after quantity pricing establishes the line price.
/// An inclusive price is preserved exactly and net is the gross remainder.
///
/// # Errors
/// Returns an error for a non-GBP currency or an unrepresentable total.
pub fn calculate_document_tax(
    price: PositiveMinorUnits,
    currency: Currency,
    rate: TaxRate,
    mode: PriceTaxMode,
) -> Result<LineAmounts, TaxError> {
    if currency != Currency::GBP {
        return Err(TaxError::Currency);
    }
    let price = price.minor_units();
    let (net_minor, vat_minor, gross_minor) = match mode {
        PriceTaxMode::Exclusive => {
            let vat = round_ratio(price, rate.0, RATE_UNIT)?;
            (price, vat, price.checked_add(vat).ok_or(TaxError::Amount)?)
        }
        PriceTaxMode::Inclusive => {
            let vat = round_ratio(price, rate.0, RATE_UNIT + rate.0)?;
            (price - vat, vat, price)
        }
    };
    Ok(LineAmounts {
        net_minor,
        vat_minor,
        gross_minor,
    })
}

fn round_ratio(value: i64, numerator: i64, denominator: i64) -> Result<i64, TaxError> {
    let numerator = i128::from(value) * i128::from(numerator);
    let denominator = i128::from(denominator);
    i64::try_from((numerator + denominator / 2) / denominator).map_err(|_| TaxError::Amount)
}

/// Errors specific to configuration and calculation of this VAT profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaxError {
    Identifier(&'static str),
    InvalidCode,
    InvalidRate,
    InvalidDate,
    TreatmentAccounts,
    Currency,
    Amount,
    MissingRate(TaxCodeId),
    RateConflict,
    InvalidControl(AccountCode),
    StaleReview,
}

impl fmt::Display for TaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identifier(prefix) => write!(formatter, "expected a valid {prefix} TypeID"),
            Self::InvalidCode => formatter.write_str("VAT code must have 1 to 32 letters, digits, underscores or hyphens"),
            Self::InvalidDate => formatter.write_str("VAT effective dates require a four-digit year"),
            Self::InvalidRate => formatter.write_str("VAT percentage must be between 0 and 100 with at most four decimal places"),
            Self::TreatmentAccounts => formatter.write_str("taxable VAT requires a positive rate and an output liability account; zero-rated and exempt lines require a zero rate and no VAT accounts"),
            Self::Currency => formatter.write_str("the configured UK invoice VAT profile supports GBP only"),
            Self::Amount => formatter.write_str("VAT amount is outside the supported line or currency range"),
            Self::MissingRate(code) => write!(formatter, "VAT code '{code}' has no effective revision in this company on the document date"),
            Self::RateConflict => formatter.write_str("this VAT code already has a different revision on that date; choose a different effective date"),
            Self::InvalidControl(code) => write!(formatter, "VAT control account '{code}' is invalid for this document"),
            Self::StaleReview => formatter.write_str("VAT or preceding credit terms changed after review; issue a new proposal"),
        }
    }
}

impl std::error::Error for TaxError {}

impl From<TaxError> for CliError {
    fn from(value: TaxError) -> Self {
        Self::Validation(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_penny_and_partial_credit_amounts_conserve_each_line() -> Result<(), TaxError> {
        let rate = "20".parse()?;
        let exclusive = calculate_document_tax(
            127_i64.try_into().map_err(|_| TaxError::Amount)?,
            Currency::GBP,
            rate,
            PriceTaxMode::Exclusive,
        )?;
        assert_eq!(
            exclusive,
            LineAmounts {
                net_minor: 127,
                vat_minor: 25,
                gross_minor: 152
            }
        );
        let inclusive = calculate_document_tax(
            123_i64.try_into().map_err(|_| TaxError::Amount)?,
            Currency::GBP,
            rate,
            PriceTaxMode::Inclusive,
        )?;
        assert_eq!(
            inclusive,
            LineAmounts {
                net_minor: 102,
                vat_minor: 21,
                gross_minor: 123
            }
        );
        let line = TaxedLine {
            number: 1,
            revision: RateRevision {
                code: TaxCodeId::generate(),
                id: TaxRateRevisionId::generate(),
                name: "UK".parse()?,
                terms: RateTerms {
                    effective_from: NaiveDate::from_ymd_opt(2026, 1, 1).ok_or(TaxError::Amount)?,
                    treatment: TaxTreatment::Taxable,
                    rate,
                    output_account: None,
                    input_account: None,
                },
            },
            price_mode: PriceTaxMode::Inclusive,
            amounts: inclusive,
        };
        let mut cumulative = 0;
        let penny = 1_i64.try_into().map_err(|_| TaxError::Amount)?;
        for previous in 0..inclusive.gross_minor {
            let partial = line.credit(previous, penny)?;
            assert_eq!(partial.amounts.net_minor + partial.amounts.vat_minor, 1);
            cumulative += partial.amounts.vat_minor;
        }
        assert_eq!(cumulative, inclusive.vat_minor);
        assert!(matches!(
            calculate_document_tax(penny, Currency::JPY, rate, PriceTaxMode::Inclusive),
            Err(TaxError::Currency)
        ));
        Ok(())
    }
}
