//! Validated inputs for provisioning separate synthetic demo books.

use std::{
    ffi::OsStr,
    fmt,
    fs::File,
    io::{self, Read},
    str::FromStr,
};

use beankeeper::types::Currency;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{error::CliError, mcp::input_values::Text};

mod country;
pub use country::{CountryCode, CountryCodeError};

/// Owner's work shape. It selects a synthetic income scenario.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkShape {
    #[default]
    Employee,
    SelfEmployed,
    BusinessOwner,
    Retired,
    Other,
}

/// Legal shape of a synthetic book.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompanyKind {
    #[default]
    Personal,
    SoleProprietorship,
    Llc,
    Corporation,
}

/// Optional industry hint. Only predefined synthetic scenarios may be selected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DemoIndustry {
    #[default]
    General,
    ProfessionalServices,
    Retail,
    Trades,
}

/// Financial account shapes supported by the generator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DemoAccountKind {
    #[default]
    Checking,
    Savings,
    CreditCard,
    PaymentApp,
    Loan,
    Investment,
    Cash,
}

/// A requested account. An omitted currency inherits the primary currency.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DemoAccountProfile {
    pub(crate) kind: DemoAccountKind,
    currency: Option<Text<Currency>>,
}

impl DemoAccountProfile {
    /// Construct an account choice with an optional explicit currency.
    #[must_use]
    pub fn new(kind: DemoAccountKind, currency: Option<Currency>) -> Self {
        Self {
            kind,
            currency: currency.map(Text::from),
        }
    }

    /// The requested financial account shape.
    #[must_use]
    pub fn kind(&self) -> DemoAccountKind {
        self.kind
    }

    /// The explicitly requested currency, if any.
    #[must_use]
    pub fn currency(&self) -> Option<Currency> {
        self.currency.as_ref().map(Text::as_ref).copied()
    }

    pub(crate) fn currency_with_primary(&self, primary: Currency) -> Currency {
        self.currency().unwrap_or(primary)
    }
}

/// One requested book. Account order is preserved, including repeated kinds.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DemoBookProfile {
    pub(crate) kind: CompanyKind,
    pub(crate) industry: DemoIndustry,
    #[schemars(length(max = MAX_ACCOUNTS))]
    pub(crate) accounts: Vec<DemoAccountProfile>,
}

impl DemoBookProfile {
    /// Construct a book choice. The full profile validates its account count and currencies.
    #[must_use]
    pub fn new(
        kind: CompanyKind,
        industry: DemoIndustry,
        accounts: Vec<DemoAccountProfile>,
    ) -> Self {
        Self {
            kind,
            industry,
            accounts,
        }
    }
}

/// Currency selections always include a primary currency.
///
/// The primary currency defaults to one supported for the owner's country, or USD.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DemoCurrencyChoices {
    primary: Option<Text<Currency>>,
    #[schemars(length(max = MAX_SECONDARY))]
    secondary: Vec<Text<Currency>>,
}

impl DemoCurrencyChoices {
    /// Construct choices from existing ledger currencies.
    #[must_use]
    pub fn new(primary: Option<Currency>, secondary: Vec<Currency>) -> Self {
        Self {
            primary: primary.map(Text::from),
            secondary: secondary.into_iter().map(Text::from).collect(),
        }
    }

    fn primary_for(&self, country: CountryCode) -> Currency {
        self.primary
            .as_ref()
            .map_or_else(|| country.default_currency(), |value| *value.as_ref())
    }
}

/// Feature choices for generating transactions in the synthetic books.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DemoFeatures {
    pub(crate) include_investments: bool,
    pub(crate) include_owner_draws: bool,
}

impl DemoFeatures {
    /// Explicitly select investment activity and linked owner draws.
    #[must_use]
    pub const fn new(include_investments: bool, include_owner_draws: bool) -> Self {
        Self {
            include_investments,
            include_owner_draws,
        }
    }
}

impl Default for DemoFeatures {
    fn default() -> Self {
        Self::new(false, true)
    }
}

const MAX_BOOKS: usize = 16;
const MAX_ACCOUNTS: usize = 32;
const MAX_SECONDARY: usize = 9;

/// Interview choices before the profile checks relationships across fields.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DemoInterview {
    /// Owner's broad work situation.
    pub work_shape: WorkShape,
    /// ISO 3166-1 alpha-2 country.
    pub country: CountryCode,
    /// Primary and optional secondary currencies.
    pub currencies: DemoCurrencyChoices,
    /// One or more books, in the desired order.
    #[schemars(length(min = 1, max = MAX_BOOKS))]
    pub books: Vec<DemoBookProfile>,
    /// Optional synthetic activities.
    pub features: DemoFeatures,
}

impl Default for DemoInterview {
    fn default() -> Self {
        Self {
            work_shape: WorkShape::default(),
            country: CountryCode::default(),
            currencies: DemoCurrencyChoices::default(),
            books: vec![DemoBookProfile::default()],
            features: DemoFeatures::default(),
        }
    }
}

/// A validated interview for generating synthetic books. The generator never writes it.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "DemoInterview")]
pub struct DemoProfile {
    #[serde(flatten)]
    interview: DemoInterview,
    #[serde(skip)]
    primary: Currency,
}

impl fmt::Debug for DemoProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DemoProfile")
            .finish_non_exhaustive()
    }
}

impl Default for DemoProfile {
    fn default() -> Self {
        let interview = DemoInterview::default();
        Self {
            primary: interview.currencies.primary_for(interview.country),
            interview,
        }
    }
}

impl TryFrom<DemoInterview> for DemoProfile {
    type Error = InvalidDemoProfile;

    fn try_from(interview: DemoInterview) -> Result<Self, Self::Error> {
        if !(1..=MAX_BOOKS).contains(&interview.books.len()) {
            return Err(InvalidDemoProfile::BookCount);
        }
        let primary = interview.currencies.primary_for(interview.country);
        if interview.currencies.secondary.len() > MAX_SECONDARY {
            return Err(InvalidDemoProfile::CurrencyCount);
        }
        let mut currencies = std::collections::HashSet::from([primary]);
        for currency in &interview.currencies.secondary {
            if !currencies.insert(*currency.as_ref()) {
                return Err(InvalidDemoProfile::DuplicateCurrency);
            }
        }
        for book in &interview.books {
            if book.accounts.len() > MAX_ACCOUNTS {
                return Err(InvalidDemoProfile::AccountCount);
            }
            if book
                .accounts
                .iter()
                .filter_map(DemoAccountProfile::currency)
                .any(|currency| !currencies.contains(&currency))
            {
                return Err(InvalidDemoProfile::UnlistedCurrency);
            }
        }
        Ok(Self { interview, primary })
    }
}

/// A relationship between interview choices could not be satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidDemoProfile {
    /// At least one book is required and the input is bounded.
    BookCount,
    /// Each book has a bounded account list.
    AccountCount,
    /// The list of secondary currencies is bounded.
    CurrencyCount,
    /// A currency can appear only once in the selection.
    DuplicateCurrency,
    /// An account currency must be in the selection.
    UnlistedCurrency,
}

impl fmt::Display for InvalidDemoProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BookCount => write!(formatter, "demo profile requires 1 to {MAX_BOOKS} books"),
            Self::AccountCount => write!(
                formatter,
                "each demo book supports at most {MAX_ACCOUNTS} requested accounts"
            ),
            Self::CurrencyCount => write!(
                formatter,
                "demo profile supports at most {MAX_SECONDARY} secondary currencies"
            ),
            Self::DuplicateCurrency => {
                formatter.write_str("demo profile currencies must be distinct")
            }
            Self::UnlistedCurrency => {
                formatter.write_str("demo account currency must be listed as primary or secondary")
            }
        }
    }
}

impl std::error::Error for InvalidDemoProfile {}

impl FromStr for DemoProfile {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_bytes(value.as_bytes())
    }
}

impl DemoProfile {
    fn invalid_json(error: &serde_json::Error) -> CliError {
        // Serde's messages can contain input values. Report only the location.
        CliError::Validation(format!(
            "invalid demo profile at line {}, column {}; check the documented fields and currency choices",
            error.line(),
            error.column()
        ))
    }

    /// The resolved primary currency.
    #[must_use]
    pub fn primary_currency(&self) -> Currency {
        self.primary
    }

    /// The resolved currencies, starting with the primary currency.
    pub fn currencies(&self) -> impl Iterator<Item = Currency> + '_ {
        std::iter::once(self.primary).chain(
            self.interview
                .currencies
                .secondary
                .iter()
                .map(|value| *value.as_ref()),
        )
    }

    pub(crate) fn books(&self) -> &[DemoBookProfile] {
        &self.interview.books
    }

    pub(crate) fn work_shape(&self) -> WorkShape {
        self.interview.work_shape
    }

    pub(crate) fn features(&self) -> DemoFeatures {
        self.interview.features
    }
}

impl TryFrom<&OsStr> for DemoProfile {
    type Error = CliError;

    fn try_from(input: &OsStr) -> Result<Self, Self::Error> {
        if input == "-" {
            return Self::read(io::stdin().lock());
        }
        if let Some(json) = input
            .to_str()
            .filter(|value| value.trim_start().starts_with(['{', '[']))
        {
            return json.parse();
        }
        Self::read(File::open(input)?)
    }
}

impl DemoProfile {
    fn read(input: impl Read) -> Result<Self, CliError> {
        let mut bytes = Vec::new();
        input
            .take((MAX_PROFILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Self::parse_bytes(&bytes)
    }

    fn parse_bytes(bytes: &[u8]) -> Result<Self, CliError> {
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(CliError::Validation(format!(
                "demo profile exceeds {MAX_PROFILE_BYTES} bytes"
            )));
        }
        let interview: DemoInterview =
            serde_json::from_slice(bytes).map_err(|error| Self::invalid_json(&error))?;
        interview.try_into().map_err(CliError::from)
    }
}

const MAX_PROFILE_BYTES: usize = 65_536;

impl TryFrom<serde_json::Map<String, serde_json::Value>> for DemoProfile {
    type Error = CliError;

    fn try_from(fields: serde_json::Map<String, serde_json::Value>) -> Result<Self, Self::Error> {
        let interview: DemoInterview = serde_json::from_value(serde_json::Value::Object(fields))
            .map_err(|error| Self::invalid_json(&error))?;
        interview.try_into().map_err(CliError::from)
    }
}

impl From<InvalidDemoProfile> for CliError {
    fn from(error: InvalidDemoProfile) -> Self {
        Self::Validation(error.to_string())
    }
}
