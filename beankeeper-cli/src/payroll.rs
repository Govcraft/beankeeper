//! Exact journal planning for finalized, aggregate payroll runs.

use std::{collections::BTreeMap, error::Error, fmt, str::FromStr};

use beankeeper::types::{AccountCode, AccountType, Amount, Currency, DebitOrCredit};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::db::{amounts::PositiveMinorUnits, transactions::PostEntryParams};

/// A payroll provider's organization or run key. Keys are case sensitive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct ProviderKey(Box<str>);

impl FromStr for ProviderKey {
    type Err = PayrollError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 128
            || value.chars().any(char::is_control)
            || value.trim() != value
        {
            return Err(PayrollError::InvalidKey);
        }
        Ok(Self(value.into()))
    }
}

impl<'de> Deserialize<'de> for ProviderKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The sign of an aggregate payroll import relative to a regular run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunKind {
    Regular,
    OffCycle,
    Reversal,
}

/// How an aggregate amount is classified in a payroll journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Component {
    GrossWages,
    Reimbursements,
    EmployerTaxes,
    EmployerBenefits,
    ProviderFees,
    NetPay,
    TaxWithholdings,
    BenefitWithholdings,
    FundingClearing,
}

impl Component {
    #[must_use]
    pub(crate) const fn account_type(self) -> AccountType {
        match self {
            Self::GrossWages
            | Self::Reimbursements
            | Self::EmployerTaxes
            | Self::EmployerBenefits
            | Self::ProviderFees => AccountType::Expense,
            Self::NetPay
            | Self::TaxWithholdings
            | Self::BenefitWithholdings
            | Self::FundingClearing => AccountType::Liability,
        }
    }

    const fn direction(self, kind: RunKind) -> DebitOrCredit {
        match (self.account_type(), kind) {
            (AccountType::Expense, RunKind::Reversal) => DebitOrCredit::Credit,
            (AccountType::Expense, _) | (_, RunKind::Reversal) => DebitOrCredit::Debit,
            _ => DebitOrCredit::Credit,
        }
    }
}

impl fmt::Display for Component {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GrossWages => "Gross wages",
            Self::Reimbursements => "Reimbursements",
            Self::EmployerTaxes => "Employer taxes",
            Self::EmployerBenefits => "Employer benefits",
            Self::ProviderFees => "Provider fees",
            Self::NetPay => "Net pay",
            Self::TaxWithholdings => "Tax withholdings and employer taxes payable",
            Self::BenefitWithholdings => "Benefit withholdings and employer benefits payable",
            Self::FundingClearing => "Payroll provider clearing",
        })
    }
}

/// Nonempty inclusive earning dates for a payroll run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayPeriod {
    pub(crate) start: NaiveDate,
    pub(crate) end: NaiveDate,
}

/// The organization and stable run ID from the payroll system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunKey {
    pub(crate) provider: ProviderKey,
    pub(crate) organization: ProviderKey,
    pub(crate) run: ProviderKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FinalizedRun {
    pub(crate) key: RunKey,
    pub(crate) revision: std::num::NonZeroU32,
    pub(crate) kind: RunKind,
    pub(crate) period: PayPeriod,
    pub(crate) pay_date: NaiveDate,
    pub(crate) currency: Currency,
    pub(crate) total_minor: PositiveMinorUnits,
    pub(crate) components: Vec<AggregateComponent>,
    pub(crate) reverses: Option<RunKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregateComponent {
    pub(crate) component: Component,
    pub(crate) amount_minor: PositiveMinorUnits,
}

pub(crate) type AccountMap = BTreeMap<Component, AccountCode>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PayrollError {
    InvalidKey,
    InvalidPeriod,
    InvalidCorrection,
    MissingAccount(Component),
    DuplicateComponent(Component),
    Unbalanced {
        expected: Amount,
        debit: Amount,
        credit: Amount,
    },
    Overflow,
}

impl fmt::Display for PayrollError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey => formatter.write_str(
                "payroll source keys must contain 1 to 128 bytes without surrounding whitespace or control characters",
            ),
            Self::InvalidPeriod => formatter.write_str("payroll period starts after it ends"),
            Self::InvalidCorrection => formatter.write_str(
                "a reversal needs a distinct original run in the same provider organization",
            ),
            Self::MissingAccount(component) => write!(formatter, "map {component} to a ledger account"),
            Self::DuplicateComponent(component) => write!(formatter, "duplicate payroll component {component}"),
            Self::Unbalanced { expected, debit, credit } => write!(
                formatter,
                "payroll control total is {expected} minor units, but planned debits are {debit} and credits are {credit}",
            ),
            Self::Overflow => formatter.write_str("payroll totals exceed the supported amount range"),
        }
    }
}

impl Error for PayrollError {}

impl From<PayrollError> for crate::error::CliError {
    fn from(error: PayrollError) -> Self {
        Self::Validation(error.to_string())
    }
}

/// Plan the journal and verify both sides against the provider's control total.
pub(crate) fn plan(
    run: &FinalizedRun,
    accounts: &AccountMap,
) -> Result<Vec<PostEntryParams>, PayrollError> {
    if run.period.start > run.period.end {
        return Err(PayrollError::InvalidPeriod);
    }
    validate_correction(run)?;

    let mut debit = Amount::ZERO;
    let mut credit = Amount::ZERO;
    let mut components = BTreeMap::new();
    for row in &run.components {
        if components.insert(row.component, row.amount_minor).is_some() {
            return Err(PayrollError::DuplicateComponent(row.component));
        }
    }
    let entries = components
        .into_iter()
        .map(|(component, amount)| {
            let direction = component.direction(run.kind);
            let total = match direction {
                DebitOrCredit::Debit => &mut debit,
                DebitOrCredit::Credit => &mut credit,
            };
            *total = total
                .checked_add(amount.into())
                .ok_or(PayrollError::Overflow)?;

            Ok(PostEntryParams {
                account_code: accounts
                    .get(&component)
                    .ok_or(PayrollError::MissingAccount(component))?
                    .to_string(),
                direction,
                amount,
                memo: Some(component.to_string()),
                tax_category: None,
            })
        })
        .collect::<Result<Vec<_>, PayrollError>>()?;

    let expected = Amount::from(run.total_minor);
    if debit != expected || credit != expected {
        return Err(PayrollError::Unbalanced {
            expected,
            debit,
            credit,
        });
    }
    Ok(entries)
}

fn validate_correction(run: &FinalizedRun) -> Result<(), PayrollError> {
    match (run.kind, &run.reverses) {
        (RunKind::Reversal, Some(original))
            if original.provider == run.key.provider
                && original.organization == run.key.organization
                && original.run != run.key.run =>
        {
            Ok(())
        }
        (RunKind::Regular | RunKind::OffCycle, None) => Ok(()),
        _ => Err(PayrollError::InvalidCorrection),
    }
}
