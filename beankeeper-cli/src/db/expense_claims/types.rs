//! Validated claim inputs and pure state transitions.

use std::{fmt, num::NonZeroU16, str::FromStr};

use beankeeper::types::{AccountCode, AccountName, Currency};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::db::{amounts::PositiveMinorUnits, counterparties::CounterpartyId, ids::TransactionId};
use crate::error::CliError;
use crate::mcp::input_values::Text;

macro_rules! expense_id {
    ($id:ident, $prefix:literal, $documentation:literal) => {
        #[doc = $documentation]
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $id(MagicTypeId);

        impl $id {
            pub(super) fn generate() -> Self {
                Self($prefix.create_type_id::<V7>())
            }
        }

        impl fmt::Display for $id {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $id {
            type Err = CliError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let id = value.parse::<MagicTypeId>()
                    .map_err(|error| CliError::Validation(format!("invalid {} ID: {error}",$prefix)))?;
                if id.prefix_str().as_deref() != Ok($prefix) {
                    return Err(CliError::Validation(concat!("expected an ID with prefix ", $prefix).into()));
                }
                Ok(Self(id))
            }
        }

        impl TryFrom<String> for $id {
            type Error = CliError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$id> for String {
            fn from(value: $id) -> Self {
                value.to_string()
            }
        }

        impl JsonSchema for $id {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($id).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type":"string", "pattern": concat!("^", $prefix, "_[0-9a-z]{26}$")})
            }
        }

        impl ToSql for $id {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.to_string()))
            }
        }

        impl FromSql for $id {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

expense_id!(
    ReceiptIntakeId,
    "receiptin",
    "Company-owned intake of a receipt file."
);
expense_id!(
    ExpenseClaimId,
    "claim",
    "Persistent identity of one expense claim."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaimError {
    InvalidState,
    InvalidSplits,
    InvalidCurrency,
    InvalidPayment,
    RevisionOverflow,
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidState => {
                "the expense claim cannot perform this action in its current state"
            }
            Self::InvalidSplits => {
                "expense splits must use distinct accounts and sum to the confirmed total"
            }
            Self::InvalidCurrency => "the expense must use the functional currency",
            Self::InvalidPayment => "reimbursement exceeds the remaining confirmed liability",
            Self::RevisionOverflow => "claim revision limit reached",
        })
    }
}

impl std::error::Error for ClaimError {}

impl From<ClaimError> for CliError {
    fn from(err: ClaimError) -> Self {
        Self::Validation(err.to_string())
    }
}

/// One validated positive allocation to a distinct expense account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseSplit {
    #[schemars(with = "Text<AccountCode>")]
    pub(crate) account: AccountCode,
    #[schemars(with = "i64", range(min = 1))]
    pub(crate) amount_minor: PositiveMinorUnits,
}

/// Payment type chosen by the claimant and frozen for review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClaimFunding {
    Reimbursable {
        #[schemars(with = "Text<AccountCode>")]
        payable: AccountCode,
    },
    CompanyCard {
        transaction: TransactionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimTerms {
    pub(crate) claimant: CounterpartyId,
    pub(crate) receipt: ReceiptIntakeId,
    #[schemars(with = "Text<AccountName>")]
    pub(crate) merchant: AccountName,
    #[schemars(with = "crate::request::CalendarDateArg")]
    pub(crate) incurred_on: NaiveDate,
    #[schemars(with = "Text<Currency>")]
    pub(crate) currency: Currency,
    #[schemars(with = "i64", range(min = 1))]
    pub(crate) amount_minor: PositiveMinorUnits,
    #[schemars(length(min = 1, max = 20))]
    pub(crate) splits: Vec<ExpenseSplit>,
    pub(crate) funding: ClaimFunding,
}

impl ClaimTerms {
    pub(super) fn validate(&self, functional: Option<Currency>) -> Result<(), ClaimError> {
        if functional.is_some_and(|currency| currency != self.currency) {
            return Err(ClaimError::InvalidCurrency);
        }
        let mut used = std::collections::BTreeSet::new();
        if self.splits.is_empty() || self.splits.len() > 20 {
            return Err(ClaimError::InvalidSplits);
        }
        let mut total = 0_i128;
        for split in &self.splits {
            if !used.insert(&split.account) {
                return Err(ClaimError::InvalidSplits);
            }
            total += i128::from(split.amount_minor);
        }
        if total == i128::from(self.amount_minor) {
            Ok(())
        } else {
            Err(ClaimError::InvalidSplits)
        }
    }
}

/// Revision count of the claim's append-only state history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClaimRevision(NonZeroU16);

impl ClaimRevision {
    pub(super) const FIRST: Self = Self(NonZeroU16::MIN);

    pub(in crate::db) const fn get(self) -> u16 {
        self.0.get()
    }

    pub(super) fn next(self) -> Result<Self, ClaimError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(ClaimError::RevisionOverflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::db) enum ClaimStatus {
    Draft,
    Submitted,
    Approved,
    Rejected,
    PartiallyReimbursed,
    Reimbursed,
    CardMatched,
    Voided,
}

impl ClaimStatus {
    pub(in crate::db) const fn code(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Submitted => "submitted",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::PartiallyReimbursed => "partially_reimbursed",
            Self::Reimbursed => "reimbursed",
            Self::CardMatched => "card_matched",
            Self::Voided => "voided",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ReviewerIdentity {
    Agent(String),
    LocalActor(String),
}

impl From<&crate::db::Actor> for ReviewerIdentity {
    fn from(actor: &crate::db::Actor) -> Self {
        actor.agent_uri().map_or_else(
            || Self::LocalActor(actor.name().to_owned()),
            |uri| Self::Agent(uri.to_owned()),
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ClaimTransition {
    Amend,
    Submit,
    Approve,
    MatchCard,
    Reject,
    RecordPayment(PositiveMinorUnits),
    Void,
}

pub(super) fn transition(
    current: ClaimStatus,
    outstanding: i64,
    action: ClaimTransition,
) -> Result<ClaimStatus, ClaimError> {
    use ClaimStatus as S;
    use ClaimTransition as A;
    match (current, action) {
        (S::Draft | S::Rejected, A::Amend) => Ok(S::Draft),
        (S::Draft, A::Submit) => Ok(S::Submitted),
        (S::Submitted, A::Approve) => Ok(S::Approved),
        (S::Submitted, A::MatchCard) => Ok(S::CardMatched),
        (S::Submitted, A::Reject) => Ok(S::Rejected),
        (S::Approved, A::Void) => Ok(S::Voided),
        (S::Approved | S::PartiallyReimbursed, A::RecordPayment(amount)) => {
            match amount.minor_units().cmp(&outstanding) {
                std::cmp::Ordering::Less => Ok(S::PartiallyReimbursed),
                std::cmp::Ordering::Equal => Ok(S::Reimbursed),
                std::cmp::Ordering::Greater => Err(ClaimError::InvalidPayment),
            }
        }
        _ => Err(ClaimError::InvalidState),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overpayment_and_approved_state_changes() -> Result<(), CliError> {
        let three_hundred = PositiveMinorUnits::try_from(300)?;
        let payment = ClaimTransition::RecordPayment(three_hundred);
        assert_eq!(
            transition(ClaimStatus::Approved, 301, payment),
            Ok(ClaimStatus::PartiallyReimbursed)
        );
        assert_eq!(
            transition(ClaimStatus::PartiallyReimbursed, 300, payment),
            Ok(ClaimStatus::Reimbursed)
        );
        assert_eq!(
            transition(ClaimStatus::PartiallyReimbursed, 299, payment),
            Err(ClaimError::InvalidPayment)
        );
        assert_eq!(
            transition(ClaimStatus::Approved, 0, ClaimTransition::Amend),
            Err(ClaimError::InvalidState)
        );
        Ok(())
    }
}
