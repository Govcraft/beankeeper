//! Validated domain inputs and the functional accounting projection.

use std::{collections::BTreeMap, fmt, str::FromStr};

use beankeeper::types::{AccountType, Currency, DebitOrCredit};
use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        amounts::PositiveMinorUnits,
        counterparties::CounterpartyId,
        ids::{EntryId, TransactionId},
    },
    error::CliError,
};

use super::{ProjectId, ProjectSplitId, TimeEntryId};

/// A rejected project or work operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectError {
    /// The supplied ID had the wrong prefix.
    InvalidId(&'static str),
    /// The project code is blank or contains unsupported characters.
    InvalidCode,
    /// A label is blank, too long or contains control characters.
    InvalidLabel,
    /// Daily work must be positive and fit in one calendar day.
    InvalidMinutes,
    /// Project cannot accept new work.
    Archived,
    /// Project accounting requires an expense or revenue posting.
    WrongEntryType,
    /// An attributed split exceeds its journal entry or repeats a project.
    InvalidSplit,
    /// The proposed effective date precedes the underlying accounting event.
    InvalidDate,
    /// The amount overflowed its checked range or rounded to zero.
    AmountOutOfRange,
    /// Work has an active bill or changed after draft generation.
    SourceUnavailable,
    /// Date window is reversed or too large.
    InvalidWindow,
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidId(prefix) => return write!(f, "expected a valid {prefix} TypeID"),
            Self::InvalidCode => {
                "project code must contain 1 to 64 ASCII letters, digits, periods, underscores or hyphens"
            }
            Self::InvalidLabel => "project label must contain 1 to 200 bytes of printable text",
            Self::InvalidMinutes => "work minutes must be between 1 and 1440",
            Self::Archived => "archived projects cannot receive new work",
            Self::WrongEntryType => {
                "only revenue and expense journal entries can be assigned to projects"
            }
            Self::InvalidSplit => {
                "project splits must name distinct projects and may not exceed the journal entry"
            }
            Self::InvalidDate => "effective date precedes the accounting event",
            Self::AmountOutOfRange => "project amount is outside the supported range",
            Self::SourceUnavailable => {
                "billing source was claimed, changed, or is no longer available"
            }
            Self::InvalidWindow => "project report requires an ordered date window",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ProjectError {}

impl From<ProjectError> for CliError {
    fn from(error: ProjectError) -> Self {
        Self::Validation(error.to_string())
    }
}

macro_rules! checked_text {
    ($(#[$doc:meta])* $name:ident, $error:ident, $check:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from="String", into="String")]
        #[schemars(with="String")]
        pub struct $name(Box<str>);

        impl $name {
            /// Returns the validated text.
            #[must_use]
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl FromStr for $name {
            type Err = ProjectError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let value = raw.trim();
                if ($check)(value) { Ok(Self(value.into())) } else { Err(ProjectError::$error) }
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProjectError;
            fn try_from(raw: String) -> Result<Self, Self::Error> { raw.parse() }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self { value.0.into() }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

checked_text!(
    /// User-selected book-unique project code.
    ProjectCode,
    InvalidCode,
    |value: &str| !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
);

checked_text!(
    /// A bounded project or work description.
    ProjectLabel,
    InvalidLabel,
    |value: &str| !value.is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
);

/// Minutes worked on one dated record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u16", into = "u16")]
pub struct WorkMinutes(#[schemars(range(min = 1, max = 1440))] u16);

impl WorkMinutes {
    /// Returns the recorded whole minutes.
    #[must_use]
    pub const fn minutes(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for WorkMinutes {
    type Error = ProjectError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if (1..=1440).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ProjectError::InvalidMinutes)
        }
    }
}

impl From<WorkMinutes> for u16 {
    fn from(value: WorkMinutes) -> Self {
        value.minutes()
    }
}

impl FromStr for WorkMinutes {
    type Err = ProjectError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<u16>()
            .map_err(|_| ProjectError::InvalidMinutes)?
            .try_into()
    }
}

/// A positive per-hour rate in minor units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct BillingRate(#[schemars(with = "u64")] PositiveMinorUnits);

impl BillingRate {
    /// Calculate a positive invoice line, rounding half up at one minor unit.
    ///
    /// # Errors
    /// Returns a range error for a zero or overflowing billed amount.
    pub fn priced_minutes(self, minutes: WorkMinutes) -> Result<PositiveMinorUnits, ProjectError> {
        let numerator = i128::from(self.0) * i128::from(minutes.minutes());
        i64::try_from((numerator + 30) / 60)
            .map_err(|_| ProjectError::AmountOutOfRange)?
            .try_into()
            .map_err(|_| ProjectError::AmountOutOfRange)
    }

    /// Stored hourly rate in the project's currency.
    #[must_use]
    pub const fn minor_units(self) -> PositiveMinorUnits {
        self.0
    }
}

impl From<PositiveMinorUnits> for BillingRate {
    fn from(value: PositiveMinorUnits) -> Self {
        Self(value)
    }
}

/// Whether this cost or work is intended for a customer invoice.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum BillableState {
    /// Counts as work or cost without entering a customer invoice.
    #[default]
    Internal,
    /// May be included in an invoice to the project customer.
    Billable,
}

impl BillableState {
    pub(in crate::db) const fn storage_flag(self) -> bool {
        matches!(self, Self::Billable)
    }
}

impl From<bool> for BillableState {
    fn from(value: bool) -> Self {
        if value {
            Self::Billable
        } else {
            Self::Internal
        }
    }
}

/// Validated company-contained project terms.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectTerms {
    /// Code unique within the company.
    pub code: ProjectCode,
    /// Human-readable project name.
    pub name: ProjectLabel,
    /// Active customer used for project invoices.
    pub customer: CounterpartyId,
    /// Currency for every amount in the project.
    #[schemars(with = "String")]
    pub currency: Currency,
}

/// Current project record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Stable ID for splits, work and invoices.
    pub id: ProjectId,
    /// Project definition.
    #[serde(flatten)]
    pub terms: ProjectTerms,
    /// Archived projects remain available to reports.
    pub archived: bool,
}

impl Project {
    pub(in crate::db) fn require_active(&self) -> Result<(), ProjectError> {
        if self.archived {
            Err(ProjectError::Archived)
        } else {
            Ok(())
        }
    }
}

/// A dated piece of work submitted to a project.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkInput {
    /// Project that received the work.
    pub project: ProjectId,
    /// Calendar day on which it occurred.
    #[schemars(with = "String")]
    pub date: NaiveDate,
    /// Bounded integer work minutes.
    pub minutes: WorkMinutes,
    /// What was done.
    pub description: ProjectLabel,
    /// Optional billable hourly rate. Without a rate the work is internal.
    pub rate_minor: Option<BillingRate>,
}

/// A time record and its optional sign-off date.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeEntry {
    /// Stable source ID for invoicing.
    pub id: TimeEntryId,
    /// Frozen work facts.
    #[serde(flatten)]
    pub work: WorkInput,
    /// First day on which approval can appear in project reports.
    pub approved_on: Option<NaiveDate>,
}

/// Requested portion of an expense or revenue entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SplitInput {
    /// Project to attribute.
    pub project: ProjectId,
    /// Exact amount in the journal currency's minor units.
    #[schemars(with = "u64")]
    pub amount_minor: PositiveMinorUnits,
    /// Mark only positive expenses for rebilling.
    #[serde(default)]
    pub billable: BillableState,
}

/// Version-specific attribution that can be traced to its journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSplit {
    /// Stable source ID for this expense portion.
    pub id: ProjectSplitId,
    /// Exact amount and project.
    #[serde(flatten)]
    pub assignment: SplitInput,
}

/// Dated attribution of a ledger line, including the unallocated remainder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDistribution {
    /// Immutable journal entry.
    pub entry_id: EntryId,
    /// Journal posting containing the entry.
    pub transaction_id: TransactionId,
    /// First day this distribution appears in reports.
    pub effective_on: NaiveDate,
    /// Exact positive shares of the underlying line.
    pub splits: Vec<ProjectSplit>,
    /// Amount of the journal line not assigned to a project.
    pub unallocated_minor: i64,
}

/// Recognized revenue and expense, keeping the sign of credits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ProjectOperatingAmounts {
    /// Net booked revenue in minor units, credit-positive.
    pub revenue_minor: i64,
    /// Net booked expense in minor units, debit-positive.
    pub expense_minor: i64,
}

impl ProjectOperatingAmounts {
    /// Computes exact margin from booked activity.
    ///
    /// # Errors
    /// Returns an overflow error if the result exceeds the stored integer range.
    pub fn profit(self) -> Result<i64, ProjectError> {
        self.revenue_minor
            .checked_sub(self.expense_minor)
            .ok_or(ProjectError::AmountOutOfRange)
    }

    pub(in crate::db) fn add_entry(
        &mut self,
        kind: AccountType,
        direction: DebitOrCredit,
        amount: i64,
    ) -> Result<(), ProjectError> {
        let (bucket, positive) = match kind {
            AccountType::Revenue => (&mut self.revenue_minor, direction == DebitOrCredit::Credit),
            AccountType::Expense => (&mut self.expense_minor, direction == DebitOrCredit::Debit),
            _ => return Err(ProjectError::WrongEntryType),
        };
        let signed = if positive { amount } else { -amount };
        *bucket = bucket
            .checked_add(signed)
            .ok_or(ProjectError::AmountOutOfRange)?;
        Ok(())
    }
}

pub(in crate::db) type ProjectTotals = BTreeMap<ProjectId, ProjectOperatingAmounts>;

pub(in crate::db) fn check_splits(
    amount: PositiveMinorUnits,
    splits: &[SplitInput],
) -> Result<i64, ProjectError> {
    if splits.len() > 1000 {
        return Err(ProjectError::InvalidSplit);
    }
    let mut projects = std::collections::BTreeSet::new();
    let mut remainder = amount.minor_units();
    for split in splits {
        if !projects.insert(&split.project) {
            return Err(ProjectError::InvalidSplit);
        }
        remainder = remainder
            .checked_sub(split.amount_minor.minor_units())
            .filter(|value| *value >= 0)
            .ok_or(ProjectError::InvalidSplit)?;
    }
    Ok(remainder)
}

/// A whole calendar month used for an expense budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct ProjectMonth(NaiveDate);

impl ProjectMonth {
    /// First day of the month.
    #[must_use]
    pub const fn start(self) -> NaiveDate {
        self.0
    }

    /// Last calendar day of the month.
    ///
    /// # Errors
    /// Returns a range error when the following month cannot be represented.
    pub fn end(self) -> Result<NaiveDate, ProjectError> {
        self.0
            .checked_add_months(chrono::Months::new(1))
            .and_then(|date| date.pred_opt())
            .ok_or(ProjectError::InvalidWindow)
    }
}

impl FromStr for ProjectMonth {
    type Err = ProjectError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let valid_shape = raw.as_bytes().split_first_chunk::<4>().is_some_and(|(year, month)| {
            year.iter().all(u8::is_ascii_digit)
                && matches!(month, [b'-', tens, ones] if tens.is_ascii_digit() && ones.is_ascii_digit())
        });
        if !valid_shape {
            return Err(ProjectError::InvalidWindow);
        }
        format!("{raw}-01")
            .parse::<NaiveDate>()
            .map(Self)
            .map_err(|_| ProjectError::InvalidWindow)
    }
}

impl TryFrom<String> for ProjectMonth {
    type Error = ProjectError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ProjectMonth> for String {
    fn from(value: ProjectMonth) -> Self {
        value.to_string()
    }
}

impl fmt::Display for ProjectMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.format("%Y-%m").fmt(f)
    }
}

/// Input for a monthly project expense budget.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectBudgetInput {
    /// Project being planned.
    pub project: ProjectId,
    /// Whole calendar month.
    pub month: ProjectMonth,
    /// Expense target in integer minor units.
    pub amount_minor: u64,
}

/// A billable item eligible to be used exactly once per active invoice line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum BillingSource {
    /// An approved time entry.
    Time(TimeEntryId),
    /// A positive assigned expense.
    Expense(ProjectSplitId),
}
