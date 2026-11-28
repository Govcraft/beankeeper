//! Compare posted bank lines with pending authorizations in the ledger snapshot.

use std::{fmt, str::FromStr};

use beankeeper::types::{Currency, DebitOrCredit};
use chrono::{Days, NaiveDate};
use clap::Args;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{OpenHold, SettlementResult, finish, post_ready, propose_ready};
use crate::{
    commands::reference::TransactionReference,
    db::{
        Actor, Db,
        amounts::PositiveMinorUnits,
        audit::savepoint::AuditedTx,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        ids::TransactionId,
        metadata,
        proposals::{self, ProposalCreation, ProposalId},
        transactions::{self, PostResult},
    },
    error::CliError,
    identity::{Granted, Post},
};

macro_rules! hold_setting {
    ($(#[$doc:meta])* $name:ident, $default:expr, $error:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "i64", into = "i64")]
        #[schemars(with = "u32")]
        pub struct $name(u32);

        impl Default for $name {
            fn default() -> Self {
                Self($default)
            }
        }

        impl TryFrom<i64> for $name {
            type Error = CliError;

            fn try_from(value: i64) -> Result<Self, Self::Error> {
                u32::try_from(value)
                    .ok()
                    .map(Self)
                    .ok_or_else(|| CliError::Validation($error.into()))
            }
        }

        impl From<$name> for i64 {
            fn from(value: $name) -> Self {
                Self::from(value.0)
            }
        }

        impl FromStr for $name {
            type Err = CliError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse::<i64>()
                    .map_err(|_| CliError::Validation($error.into()))?
                    .try_into()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

hold_setting!(
    /// Maximum absolute amount difference, in minor units of the statement currency.
    HoldAmountTolerance,
    0,
    "hold amount tolerance must be a nonnegative 32-bit count of minor units"
);
hold_setting!(
    /// Inclusive number of days a posting may follow its authorization.
    HoldPostingWindow,
    7,
    "hold posting window must be a nonnegative 32-bit count of days"
);
hold_setting!(
    /// An open hold must be strictly older than this age to need attention.
    StaleHoldAge,
    7,
    "stale hold age must be a nonnegative 32-bit count of days"
);

/// Matching policy shared by file imports and the bounded statement tool.
#[derive(Args, Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct HoldMatchOptions {
    /// Maximum amount difference in minor units of the statement currency.
    #[arg(long, default_value_t)]
    hold_amount_tolerance: HoldAmountTolerance,
    /// Maximum days between authorization and posting, inclusive.
    #[arg(long, default_value_t)]
    hold_posting_window_days: HoldPostingWindow,
}

impl From<(HoldAmountTolerance, HoldPostingWindow)> for HoldMatchOptions {
    fn from(
        (hold_amount_tolerance, hold_posting_window_days): (HoldAmountTolerance, HoldPostingWindow),
    ) -> Self {
        Self {
            hold_amount_tolerance,
            hold_posting_window_days,
        }
    }
}

impl HoldMatchOptions {
    pub(crate) fn is_default(self) -> bool {
        self == Self::default()
    }
}

impl StaleHoldAge {
    fn cutoff(self, as_of: NaiveDate) -> Option<NaiveDate> {
        as_of
            .checked_sub_days(Days::new(u64::from(self.0)))?
            .pred_opt()
    }
}

/// Identity and bank-side movement of one parsed statement row.
pub(crate) struct BankLine<'a> {
    pub(crate) reference: &'a TransactionReference,
    pub(crate) description: &'a str,
    pub(crate) payee: Option<&'a PayeeKey>,
    pub(crate) date: NaiveDate,
    pub(crate) currency: Currency,
    pub(crate) bank: &'a str,
    pub(crate) direction: DebitOrCredit,
    pub(crate) amount: PositiveMinorUnits,
    pub(crate) metadata: Option<&'a str>,
}

enum PostedReference {
    Unused,
    Ordinary,
    Settlement(SettlementResult),
}

pub(in crate::db) enum BankMatch<T> {
    Existing(SettlementResult),
    New(T),
}

pub(crate) fn match_bank_import(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<SettlementResult>, CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        grant.mutate("sp_match_hold", |tx, actor| {
            let matched = post(tx, actor, company, line, options)?;
            Ok(matched.map(|result| match result {
                BankMatch::Existing(result) | BankMatch::New(result) => result,
            }))
        })
    })
}

pub(in crate::db) fn post_onboarding(
    scope: &crate::db::audit::setup::direct::OnboardingWrite<
        '_,
        crate::db::audit::grant::OnboardingPost,
    >,
    company: &CompanySlug,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<BankMatch<SettlementResult>>, CliError> {
    post(scope.tx(), scope.actor(), company, line, options)
}

pub(in crate::db) fn propose_onboarding(
    scope: &crate::db::audit::setup::direct::OnboardingWrite<
        '_,
        crate::db::audit::grant::OnboardingPropose,
    >,
    company: &CompanySlug,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<BankMatch<(ProposalId, TransactionId)>>, CliError> {
    propose(scope.tx(), scope.actor(), company, line, options)
}

pub(in crate::db) fn post(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<BankMatch<SettlementResult>>, CliError> {
    apply(
        tx,
        actor,
        company,
        (line, options),
        |_| Ok(None),
        post_ready,
    )
}

pub(in crate::db) fn propose(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<BankMatch<(ProposalId, TransactionId)>>, CliError> {
    apply(
        tx,
        actor,
        company,
        (line, options),
        |scope| prior_proposal(scope, actor, line),
        |ready| {
            let hold = ready.link().hold_id();
            match propose_ready(&ready)? {
                ProposalCreation::Created(proposal) => Ok((proposal.id, hold)),
                ProposalCreation::Skipped(_) => Err(CliError::Database(
                    "matched hold proposal unexpectedly skipped".into(),
                )),
            }
        },
    )
}

/// The caller holds the authorized ledger writer for selection and posting.
fn apply<T>(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    (line, options): (&BankLine<'_>, HoldMatchOptions),
    prior: impl FnOnce(&Company<'_>) -> Result<Option<T>, CliError>,
    execute: impl FnOnce(super::ReadySettlement<'_>) -> Result<T, CliError>,
) -> Result<Option<BankMatch<T>>, CliError> {
    let scope = Company::bind(tx.read(), company.clone())?;
    match posted_reference(&scope, line)? {
        PostedReference::Settlement(result) => return Ok(Some(BankMatch::Existing(result))),
        PostedReference::Ordinary => return Ok(None),
        PostedReference::Unused => {}
    }
    if let Some(prior) = prior(&scope)? {
        return Ok(Some(BankMatch::New(prior)));
    }
    let Some(id) = select_open(&scope, line, options)? else {
        return Ok(None);
    };
    let (row, entries) = transactions::get_transaction(&scope, id)?;
    let hold = OpenHold::load(tx, row, &entries, line.date)?;
    let result = finish(
        hold,
        actor,
        line.amount,
        (line.date, line.reference),
        (line.description, line.metadata),
        execute,
    )?;
    Ok(Some(BankMatch::New(result)))
}

fn prior_proposal(
    scope: &Company<'_>,
    actor: &Actor,
    line: &BankLine<'_>,
) -> Result<Option<(ProposalId, TransactionId)>, CliError> {
    let Some(identity) = actor.identity() else {
        return Ok(None);
    };
    let id: Option<ProposalId> = scope
        .read()
        .query_row(
            "SELECT id FROM txn_proposals WHERE company_slug = ?1 AND reference = ?2
         AND status = 'pending' AND proposer_uri = ?3",
            params![scope.slug(), line.reference.as_str(), identity.uri()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(None) };
    let proposal = proposals::get(scope.read(), scope.slug(), &id)?;
    let super::PostingPurpose::Settle(hold) = proposal.payload.purpose else {
        return Ok(None);
    };
    if proposal.payload.currency == line.currency
        && proposal.payload.date == line.date
        && proposal.payload.description == line.description
        && proposal.payload.entries.iter().any(|entry| {
            entry.account_code == line.bank
                && entry.direction == line.direction
                && entry.amount == line.amount
        })
    {
        return Ok(Some((id, hold)));
    }
    Err(reference_conflict(line))
}

fn reference_conflict(line: &BankLine<'_>) -> CliError {
    CliError::Validation(format!(
        "bank reference '{}' already identifies a different hold settlement",
        line.reference
    ))
}

fn posted_reference(scope: &Company<'_>, line: &BankLine<'_>) -> Result<PostedReference, CliError> {
    let Some(id) =
        transactions::find_reference(scope.read(), scope.slug(), line.reference.as_str())?
    else {
        return Ok(PostedReference::Unused);
    };
    let (posted, entries) = transactions::get_transaction(scope, id)?;
    let Some(settlement) = posted.settlement() else {
        if posted.status() == super::EntryStatus::Pending {
            return Err(CliError::Validation(format!(
                "bank reference '{}' belongs to pending transaction #{id}. Use the posted bank reference",
                line.reference
            )));
        }
        return Ok(PostedReference::Ordinary);
    };
    if posted.currency != line.currency
        || posted.date != line.date
        || !entries.iter().any(|entry| {
            entry.account_code == line.bank
                && entry.direction == line.direction
                && entry.amount == line.amount
        })
    {
        return Err(reference_conflict(line));
    }
    Ok(PostedReference::Settlement(SettlementResult::new(
        PostResult::Skipped(id),
        settlement,
        posted.currency,
        posted.date,
    )))
}

fn select_open(
    scope: &Company<'_>,
    line: &BankLine<'_>,
    options: HoldMatchOptions,
) -> Result<Option<TransactionId>, CliError> {
    let Some(payee) = line.payee else {
        return Ok(None);
    };
    let earliest = options
        .hold_posting_window_days
        .earliest_authorization(line.date);
    let mut statement = scope.read().prepare(OPEN_HOLDS)?;
    let holds = statement.query_map(
        params![
            scope.slug(),
            line.currency.code(),
            line.date,
            false,
            line.bank,
            line.direction.code(),
            earliest,
            line.date
        ],
        read_candidate,
    )?;
    let mut matched = None;
    for hold in holds {
        let hold = hold?;
        if !options
            .hold_amount_tolerance
            .includes(line.amount, hold.amount)
            || hold.payee().as_ref() != Some(payee)
        {
            continue;
        }
        if let Some(first) = matched {
            return Err(CliError::Validation(format!(
                "bank line '{}' matches multiple open holds (#{first} and #{}). Settle the intended hold before retrying the import",
                line.reference, hold.id
            )));
        }
        matched = Some(hold.id);
    }
    Ok(matched)
}

impl HoldAmountTolerance {
    fn includes(self, actual: PositiveMinorUnits, authorized: PositiveMinorUnits) -> bool {
        actual.minor_units().abs_diff(authorized.minor_units()) <= u64::from(self.0)
    }
}

impl HoldPostingWindow {
    fn earliest_authorization(self, posting_date: NaiveDate) -> Option<NaiveDate> {
        posting_date.checked_sub_days(Days::new(u64::from(self.0)))
    }
}

/// A nonblank payee name normalized for an exact comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PayeeKey(Box<str>);

impl FromStr for PayeeKey {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(CliError::Validation("payee is blank".into()));
        }
        Ok(Self(value.to_lowercase().into_boxed_str()))
    }
}

struct Candidate {
    id: TransactionId,
    date: NaiveDate,
    amount: PositiveMinorUnits,
    description: String,
    metadata: Option<String>,
}

impl Candidate {
    fn payee(&self) -> Option<PayeeKey> {
        self.metadata
            .as_deref()
            .and_then(metadata::normalize)
            .and_then(|annotations| {
                let value = annotations.into_value();
                value.get("payee")?.as_str()?.parse().ok()
            })
            .or_else(|| self.description.parse().ok())
    }
}

fn read_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<Candidate> {
    Ok(Candidate {
        id: row.get(0)?,
        date: crate::db::calendar_date::read(row, 1)?,
        amount: row.get(2)?,
        description: row.get(3)?,
        metadata: row.get(4)?,
    })
}

// Matching needs a hold open for writing now and eligible on the bank date.
// Attention reconstructs the same lifecycle as of the report date.
const OPEN_HOLDS: &str = "
    SELECT h.id, h.date, e.amount, h.description, h.metadata
    FROM transactions h JOIN entries e ON e.transaction_id = h.id
    WHERE h.company_slug = ?1 AND h.currency = ?2 AND h.posting_status = 'pending'
      AND (?7 IS NULL OR h.date >= ?7) AND h.date <= ?8
      AND (?5 IS NULL OR e.account_code = ?5) AND e.direction = ?6
      AND NOT EXISTS (
          SELECT 1 FROM posting_reversals r JOIN transactions mirror ON mirror.id = r.reversal_id
          WHERE r.company_slug = h.company_slug AND
              (r.reversal_id = h.id OR (r.original_id = CAST(h.id AS TEXT) AND (NOT ?4 OR mirror.date <= ?3)))
      )
      AND NOT EXISTS (
          SELECT 1 FROM transactions s WHERE s.settles_hold = h.id AND (NOT ?4 OR s.date <= ?3)
            AND NOT EXISTS (
                SELECT 1 FROM posting_reversals r JOIN transactions undo ON undo.id = r.reversal_id
                WHERE r.company_slug = s.company_slug AND r.original_id = CAST(s.id AS TEXT) AND undo.date <= ?3
            )
      )
    ORDER BY h.id";

/// Authorized amount on one leg of a stale hold, as of the summary date.
#[derive(Serialize)]
pub(crate) struct StaleHold {
    pub(crate) hold_id: TransactionId,
    pub(crate) date: NaiveDate,
    pub(crate) amount: PositiveMinorUnits,
    pub(crate) description: String,
}

pub(crate) fn stale_holds(
    reader: ReadConn<'_>,
    company: &str,
    currency: Currency,
    as_of: NaiveDate,
    age: StaleHoldAge,
) -> Result<Vec<StaleHold>, CliError> {
    let Some(cutoff) = age.cutoff(as_of) else {
        return Ok(Vec::new());
    };
    let mut statement = reader.prepare(OPEN_HOLDS)?;
    let rows = statement.query_map(
        params![
            company,
            currency.code(),
            as_of,
            true,
            None::<&str>,
            "debit",
            None::<NaiveDate>,
            cutoff
        ],
        read_candidate,
    )?;
    rows.map(|row| {
        let hold = row?;
        Ok(StaleHold {
            hold_id: hold.id,
            date: hold.date,
            amount: hold.amount,
            description: hold.description,
        })
    })
    .collect()
}
