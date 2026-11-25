//! Typed inputs and read models for monthly statement reconciliation.

use std::{fmt, str::FromStr};

use beankeeper::types::{AccountCode, Currency, Money};
use chrono::{Datelike, NaiveDate};
use rusqlite::{
    Row, params,
    types::{ToSql, ToSqlOutput},
};
use serde::{Deserialize, Serialize};

use super::{companies::CompanySlug, connection::ReadConn, ids::EntryId, periods::PeriodDate};
use crate::{
    commands::{reference::TransactionReference, txn::MajorAmount},
    error::CliError,
};

/// An inclusive calendar month-end in the ledger's date format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementMonth(PeriodDate);

impl TryFrom<PeriodDate> for StatementMonth {
    type Error = CliError;

    fn try_from(end: PeriodDate) -> Result<Self, Self::Error> {
        if end.date().succ_opt().is_some_and(|date| date.day() == 1) {
            Ok(Self(end))
        } else {
            Err(CliError::Validation(
                "period_end must be the last day of a calendar month".into(),
            ))
        }
    }
}

impl FromStr for StatementMonth {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<PeriodDate>()?.try_into()
    }
}

impl fmt::Display for StatementMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<StatementMonth> for PeriodDate {
    fn from(month: StatementMonth) -> Self {
        month.0
    }
}

impl StatementMonth {
    pub(super) fn start(self) -> NaiveDate {
        self.end() - chrono::Days::new(u64::from(self.end().day0()))
    }

    pub(super) const fn end(self) -> NaiveDate {
        self.0.date()
    }
}

/// Company-local account, currency and calendar month in the audit history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementKey(AccountCode, Currency, StatementMonth);

impl From<(AccountCode, Currency, StatementMonth)> for StatementKey {
    fn from((account, currency, month): (AccountCode, Currency, StatementMonth)) -> Self {
        Self(account, currency, month)
    }
}

impl FromStr for StatementKey {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (account, rest) = raw.split_once('/').ok_or_else(invalid_key)?;
        let (currency, month) = rest.split_once('/').ok_or_else(invalid_key)?;
        Ok(Self(
            account.parse().map_err(|_| invalid_key())?,
            currency.parse().map_err(|_| invalid_key())?,
            month.parse()?,
        ))
    }
}

impl fmt::Display for StatementKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.0, self.1, self.2)
    }
}

impl Serialize for StatementKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn invalid_key() -> CliError {
    CliError::Database("invalid statement reconciliation key".into())
}

/// A nonblank identifier from a bank or card statement.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct StatementRef(Box<str>);

impl FromStr for StatementRef {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.trim().is_empty() {
            return Err(CliError::Validation(
                "statement_ref must not be blank".into(),
            ));
        }
        Ok(Self(raw.into()))
    }
}

impl TryFrom<String> for StatementRef {
    type Error = CliError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl AsRef<str> for StatementRef {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StatementRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

/// Which matched clearings to count on a statement.
#[derive(Debug, Default)]
pub enum StatementSelection {
    /// Include every eligible clearing dated in the statement month.
    #[default]
    Month,
    /// Include clearings on these entry IDs.
    Entries(crate::request::NonEmpty<EntryId>),
    /// Include clearings on the account from these transaction references.
    References(crate::request::NonEmpty<TransactionReference>),
}

impl TryFrom<(Option<Vec<EntryId>>, Option<Vec<TransactionReference>>)> for StatementSelection {
    type Error = CliError;

    fn try_from(
        (entries, references): (Option<Vec<EntryId>>, Option<Vec<TransactionReference>>),
    ) -> Result<Self, Self::Error> {
        match (entries, references) {
            (None, None) => Ok(Self::default()),
            (Some(entries), None) => Ok(Self::Entries(entries.try_into()?)),
            (None, Some(references)) => Ok(Self::References(references.try_into()?)),
            (Some(_), Some(_)) => Err(CliError::Validation(
                "choose either entry_ids or references".into(),
            )),
        }
    }
}

/// Whether the completed statement should also advance the company posting cutoff.
#[derive(Clone, Copy, Debug, Default)]
pub enum StatementClose {
    /// Complete only the account's statement.
    #[default]
    LeaveOpen,
    /// Close company posting through the statement month-end.
    ClosePeriod,
}

impl From<bool> for StatementClose {
    fn from(close: bool) -> Self {
        if close {
            Self::ClosePeriod
        } else {
            Self::LeaveOpen
        }
    }
}

impl From<Option<bool>> for StatementClose {
    fn from(close: Option<bool>) -> Self {
        close.map(Self::from).unwrap_or_default()
    }
}

/// A signed statement balance in the ledger's integer range, with its currency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StatementBalance(i64, Currency);

impl TryFrom<(MajorAmount, Currency)> for StatementBalance {
    type Error = CliError;

    fn try_from((amount, currency): (MajorAmount, Currency)) -> Result<Self, Self::Error> {
        Ok(Self(amount.minor_units(currency)?, currency))
    }
}

impl TryFrom<Money> for StatementBalance {
    type Error = CliError;

    fn try_from(money: Money) -> Result<Self, Self::Error> {
        let minor = i64::try_from(money.amount().minor_units()).map_err(|_| Self::overflow())?;
        Ok(Self(minor, money.currency()))
    }
}

impl From<StatementBalance> for Money {
    fn from(balance: StatementBalance) -> Self {
        Self::from_minor(balance.0.into(), balance.1)
    }
}

impl ToSql for StatementBalance {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

// The receipt wire format uses a single statement currency and integer amounts.
impl Serialize for StatementBalance {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.0)
    }
}

impl StatementBalance {
    pub(super) fn read(row: &Row<'_>, index: usize) -> rusqlite::Result<Self> {
        Ok(Self(row.get(index)?, row.get("currency")?))
    }

    pub(super) fn sum(
        currency: Currency,
        values: impl IntoIterator<Item = Result<Self, CliError>>,
    ) -> Result<Self, CliError> {
        let total =
            values
                .into_iter()
                .try_fold(Money::from_minor(0, currency), |total, value| {
                    total
                        .checked_add(value?.into())
                        .map_err(|error| Self::arithmetic_error(&error))
                })?;
        total.try_into()
    }

    pub(super) fn check_ending(self, actual: Self) -> Result<(), CliError> {
        if self == actual {
            return Ok(());
        }
        let delta = Money::from(self)
            .checked_sub(actual.into())
            .map_err(|error| Self::arithmetic_error(&error))?;
        Err(CliError::StatementDiscrepancy(StatementDiscrepancy {
            expected: self,
            actual,
            delta,
        }))
    }

    fn arithmetic_error(error: &beankeeper::types::MoneyError) -> CliError {
        match error {
            beankeeper::types::MoneyError::Overflow => Self::overflow(),
            _ => CliError::Validation(error.to_string()),
        }
    }

    fn overflow() -> CliError {
        CliError::Validation("statement balance exceeds the ledger's minor-unit range".into())
    }
}

/// An exact balance discrepancy in the statement's minor units.
#[derive(Clone, Copy, Debug)]
pub struct StatementDiscrepancy {
    expected: StatementBalance,
    actual: StatementBalance,
    delta: Money,
}

impl StatementDiscrepancy {
    /// Expected minus the actual ending balance.
    #[must_use]
    pub fn delta(self) -> i128 {
        self.delta.amount().minor_units()
    }

    pub(crate) fn details(self) -> serde_json::Value {
        let delta = i64::try_from(self.delta()).map_or_else(
            |_| serde_json::Value::String(self.delta().to_string()),
            serde_json::Value::from,
        );
        serde_json::json!({
            "currency": self.delta.currency(),
            "expected_ending_balance": self.expected,
            "actual_ending_balance": self.actual,
            "delta": delta,
        })
    }
}

impl fmt::Display for StatementDiscrepancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "statement ending balance differs by {} {} minor units (expected {}, actual {})",
            self.delta(),
            self.delta.currency(),
            self.expected.0,
            self.actual.0,
        )
    }
}

impl std::error::Error for StatementDiscrepancy {}

/// An account and calendar month with posted activity and no completed statement.
#[derive(Debug, Serialize, Deserialize)]
pub struct UnreconciledMonth {
    account: AccountCode,
    currency: Currency,
    period_start: NaiveDate,
    period_end: NaiveDate,
}

impl UnreconciledMonth {
    pub(crate) fn account(&self) -> &str {
        self.account.as_str()
    }

    pub(crate) const fn currency(&self) -> Currency {
        self.currency
    }

    pub(crate) const fn period_start(&self) -> NaiveDate {
        self.period_start
    }

    pub(crate) const fn period_end(&self) -> NaiveDate {
        self.period_end
    }
}

pub(crate) fn unreconciled_months(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    account: Option<&AccountCode>,
    currency: Option<Currency>,
    as_of: NaiveDate,
) -> Result<Vec<UnreconciledMonth>, CliError> {
    let completion_filter = if super::get_schema_version(reader)?
        >= super::schema::STATEMENT_RECONCILIATION_SCHEMA_VERSION
    {
        "AND NOT EXISTS (
             SELECT 1 FROM statement_reconciliations s WHERE s.company_slug = ?1
               AND s.account = m.account AND s.currency = m.currency AND s.period_end = m.period_end
               AND s.undo_receipt IS NULL
         )"
    } else {
        ""
    };
    let mut statement = reader.prepare(
        &format!("WITH active_months AS (
             SELECT DISTINCT e.account_code AS account, t.currency AS currency,
                    date(substr(t.date, 1, 10), 'start of month') AS period_start,
                    date(substr(t.date, 1, 10), 'start of month', '+1 month', '-1 day') AS period_end
             FROM posted_transactions t JOIN entries e
               ON e.transaction_id = t.id AND e.company_slug = t.company_slug
             JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code
             WHERE t.company_slug = ?1 AND a.type IN ('asset', 'liability')
               AND (?2 IS NULL OR e.account_code = ?2) AND (?3 IS NULL OR t.currency = ?3)
               AND substr(t.date, 1, 10) <= ?4
         )
         SELECT m.account, m.currency, m.period_start, m.period_end
         FROM active_months m
         WHERE m.period_end < ?4 {completion_filter}
         ORDER BY m.period_end DESC, m.account, m.currency"),
    )?;
    statement
        .query_map(params![company, account, currency, as_of], |row| {
            Ok(UnreconciledMonth {
                account: row.get(0)?,
                currency: row.get(1)?,
                period_start: row.get(2)?,
                period_end: row.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}
