//! Pending card authorizations and their audited settlement.

use std::{cmp::Ordering, fmt, str::FromStr};

use beankeeper::{
    core::Transaction,
    types::{Currency, DebitOrCredit, Entry, Money},
};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    EntryRow, TransactionRow,
    amounts::PositiveMinorUnits,
    audit::{Actor, savepoint::AuditedTx},
    companies::{Company, CompanySlug},
    ids::TransactionId,
    proposals::{self, ProposalCreation},
    transactions::{
        self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams, PostingSource,
    },
};
use crate::{
    commands::{reference::TransactionReference, txn::MajorAmount},
    error::CliError,
    identity::{Granted, Post, Propose},
};

/// The posting status shared by the balanced journal's entries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// A card authorization that affects available funds.
    Pending,
    /// Actual movement recorded in the books.
    #[default]
    Posted,
}

impl EntryStatus {
    const fn code(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Posted => "posted",
        }
    }
}

impl fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// An unknown posting status.
#[derive(Debug)]
pub struct InvalidEntryStatus;

impl fmt::Display for InvalidEntryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("posting status must be 'pending' or 'posted'")
    }
}

impl std::error::Error for InvalidEntryStatus {}

impl FromStr for EntryStatus {
    type Err = InvalidEntryStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "posted" => Ok(Self::Posted),
            _ => Err(InvalidEntryStatus),
        }
    }
}

impl rusqlite::types::FromSql for EntryStatus {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}

impl rusqlite::types::ToSql for EntryStatus {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.code().into())
    }
}

/// The immutable intent of a proposed posting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostingPurpose {
    /// Ordinary posting.
    #[default]
    Posted,
    /// Record a pending card hold.
    Pending,
    /// Settle the specified hold.
    Settle(TransactionId),
}

impl PostingPurpose {
    pub(crate) fn is_posted(&self) -> bool {
        *self == Self::Posted
    }

    pub(super) const fn status(self) -> EntryStatus {
        match self {
            Self::Pending => EntryStatus::Pending,
            Self::Posted | Self::Settle(_) => EntryStatus::Posted,
        }
    }
}

impl From<EntryStatus> for PostingPurpose {
    fn from(status: EntryStatus) -> Self {
        match status {
            EntryStatus::Pending => Self::Pending,
            EntryStatus::Posted => Self::Posted,
        }
    }
}

/// Typed relationship between the actual posting and its original hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SettlementLink {
    hold_id: TransactionId,
    amount_delta: i64,
}

impl SettlementLink {
    /// Original pending transaction.
    #[must_use]
    pub const fn hold_id(self) -> TransactionId {
        self.hold_id
    }

    /// Posted amount minus authorized amount, in minor units.
    #[must_use]
    pub const fn amount_delta(self) -> i64 {
        self.amount_delta
    }
}

/// Checked lifecycle decoded from the transaction's durable columns.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostingState {
    #[default]
    Posted,
    Pending,
    Settlement(SettlementLink),
}

impl PostingState {
    pub(super) fn read(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Self> {
        match (row.get(start)?, row.get(start + 1)?, row.get(start + 2)?) {
            (EntryStatus::Posted, None, None) => Ok(Self::Posted),
            (EntryStatus::Pending, None, None) => Ok(Self::Pending),
            (EntryStatus::Posted, Some(hold_id), Some(amount_delta)) => {
                Ok(Self::Settlement(SettlementLink {
                    hold_id,
                    amount_delta,
                }))
            }
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }

    pub(super) const fn status(self) -> EntryStatus {
        match self {
            Self::Pending => EntryStatus::Pending,
            Self::Posted | Self::Settlement(_) => EntryStatus::Posted,
        }
    }

    pub(super) const fn settlement(self) -> Option<SettlementLink> {
        match self {
            Self::Settlement(link) => Some(link),
            _ => None,
        }
    }
}

/// Identifies a hold in the selected company.
#[derive(Debug, Clone)]
pub enum HoldSelector {
    /// Numeric ledger transaction ID.
    Id(TransactionId),
    /// Stable reference supplied when recording the hold.
    Reference(TransactionReference),
}

impl TryFrom<(Option<TransactionId>, Option<TransactionReference>)> for HoldSelector {
    type Error = CliError;

    fn try_from(
        (id, reference): (Option<TransactionId>, Option<TransactionReference>),
    ) -> Result<Self, Self::Error> {
        match (id, reference) {
            (Some(id), None) => Ok(Self::Id(id)),
            (None, Some(reference)) => Ok(Self::Reference(reference)),
            _ => Err(CliError::Validation(
                "select exactly one hold ID or hold reference".into(),
            )),
        }
    }
}

/// A positive posted decimal amount, awaiting the hold's currency scale.
#[derive(Debug, Clone)]
pub struct SettlementAmount(MajorAmount);

impl FromStr for SettlementAmount {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let amount = value.parse::<MajorAmount>()?;
        if amount.sign() != Ordering::Greater {
            return Err(CliError::Validation(
                "posted amount must be positive".into(),
            ));
        }
        Ok(Self(amount))
    }
}

impl fmt::Display for SettlementAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<(&SettlementAmount, Currency)> for PositiveMinorUnits {
    type Error = CliError;

    fn try_from((amount, currency): (&SettlementAmount, Currency)) -> Result<Self, Self::Error> {
        amount.0.minor_units(currency)?.try_into()
    }
}

/// A decoded settlement request. The hold supplies its company-local accounts and currency.
#[derive(Debug)]
pub struct SettlementRequest {
    selector: HoldSelector,
    amount: SettlementAmount,
    date: NaiveDate,
    reference: TransactionReference,
}

impl
    From<(
        HoldSelector,
        SettlementAmount,
        NaiveDate,
        TransactionReference,
    )> for SettlementRequest
{
    fn from(
        (selector, amount, date, reference): (
            HoldSelector,
            SettlementAmount,
            NaiveDate,
            TransactionReference,
        ),
    ) -> Self {
        Self {
            selector,
            amount,
            date,
            reference,
        }
    }
}

#[derive(Debug)]
enum HoldError {
    RequiresPair,
    CannotCorrelate,
    NotHold(TransactionId),
    Closed(TransactionId, TransactionId),
    TooEarly(NaiveDate),
    ReferenceInUse(TransactionId),
    ReversalOfMirror(TransactionId),
    PostingMismatch(TransactionId),
}

impl fmt::Display for HoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiresPair => f.write_str(
                "a pending hold requires one debit and one credit on different accounts",
            ),
            Self::CannotCorrelate => {
                f.write_str("post the hold without an intercompany correlation")
            }
            Self::NotHold(id) => write!(
                f,
                "transaction #{id} is not a hold. Select the original pending transaction"
            ),
            Self::Closed(id, closing) => write!(
                f,
                "hold #{id} is already closed by transaction #{closing}. Use the original posted reference to retry a settlement"
            ),
            Self::TooEarly(earliest) => write!(
                f,
                "use an effective date on or after {earliest} for this hold"
            ),
            Self::ReferenceInUse(id) => write!(
                f,
                "posted reference belongs to transaction #{id}. Retry with the original settlement details or supply an unused reference"
            ),
            Self::ReversalOfMirror(id) => write!(
                f,
                "transaction #{id} is a hold-related reversal. Record a new hold or settle the restored hold"
            ),
            Self::PostingMismatch(id) => write!(
                f,
                "settlement for hold #{id} must use the hold's accounts and currency"
            ),
        }
    }
}

impl std::error::Error for HoldError {}

impl From<HoldError> for CliError {
    fn from(error: HoldError) -> Self {
        Self::Validation(error.to_string())
    }
}

struct HoldPair<T> {
    debit: T,
    credit: T,
}

impl<T> HoldPair<T> {
    fn read(
        entries: &[T],
        direction: impl Fn(&T) -> DebitOrCredit,
        account: impl Fn(&T) -> &str,
    ) -> Result<HoldPair<&T>, HoldError> {
        let [first, second] = entries else {
            return Err(HoldError::RequiresPair);
        };
        let pair = match (direction(first), direction(second)) {
            (DebitOrCredit::Debit, DebitOrCredit::Credit) => HoldPair {
                debit: first,
                credit: second,
            },
            (DebitOrCredit::Credit, DebitOrCredit::Debit) => HoldPair {
                debit: second,
                credit: first,
            },
            _ => return Err(HoldError::RequiresPair),
        };
        if account(pair.debit) == account(pair.credit) {
            return Err(HoldError::RequiresPair);
        }
        Ok(pair)
    }
}

/// A balanced journal with exactly one debit and one credit on different accounts.
pub(super) struct HoldJournal<'a>(HoldPair<&'a Entry>);

impl<'a> TryFrom<&'a Transaction> for HoldJournal<'a> {
    type Error = CliError;

    fn try_from(journal: &'a Transaction) -> Result<Self, Self::Error> {
        HoldPair::read(journal.entries(), Entry::direction, |entry| {
            entry.account().code().as_str()
        })
        .map(Self)
        .map_err(Into::into)
    }
}

impl<'a> TryFrom<(&'a Transaction, Option<super::ids::PartnerTransactionId>)> for HoldJournal<'a> {
    type Error = CliError;

    fn try_from(
        (journal, partner): (&'a Transaction, Option<super::ids::PartnerTransactionId>),
    ) -> Result<Self, Self::Error> {
        if partner.is_some() {
            return Err(HoldError::CannotCorrelate.into());
        }
        journal.try_into()
    }
}

impl From<HoldJournal<'_>> for PostingPurpose {
    fn from(_: HoldJournal<'_>) -> Self {
        Self::Pending
    }
}

impl From<HoldJournal<'_>> for PostingState {
    fn from(_: HoldJournal<'_>) -> Self {
        Self::Pending
    }
}

/// Proof that the original hold is still open, bound to the writer that read it.
struct OpenHold<'a> {
    tx: &'a AuditedTx<'a>,
    row: TransactionRow,
    pair: HoldPair<EntryRow>,
}

impl<'a> OpenHold<'a> {
    fn load(
        tx: &'a AuditedTx<'a>,
        row: TransactionRow,
        entries: &[EntryRow],
        date: NaiveDate,
    ) -> Result<Self, CliError> {
        check_open(tx, &row, date)?;
        let HoldPair { debit, credit } = HoldPair::read(
            entries,
            |entry| entry.direction,
            |entry| &entry.account_code,
        )?;
        if debit.amount != credit.amount {
            return Err(CliError::Database(format!(
                "hold #{} is unbalanced",
                row.id
            )));
        }
        Ok(Self {
            tx,
            row,
            pair: HoldPair {
                debit: debit.clone(),
                credit: credit.clone(),
            },
        })
    }

    fn lines(&self, amount: PositiveMinorUnits) -> [PostEntryParams; 2] {
        [&self.pair.debit, &self.pair.credit].map(|entry| PostEntryParams {
            account_code: entry.account_code.clone(),
            direction: entry.direction,
            amount,
            memo: entry.memo.clone(),
            tax_category: entry.tax_category.clone(),
        })
    }

    fn settlement(self, amount: PositiveMinorUnits) -> ReadySettlement<'a> {
        ReadySettlement {
            tx: self.tx,
            link: SettlementLink {
                hold_id: self.row.id,
                amount_delta: amount.minor_units() - self.pair.debit.amount.minor_units(),
            },
        }
    }
}

/// A single-use transition from an open hold into its linked posting.
pub(super) struct ReadySettlement<'a> {
    tx: &'a AuditedTx<'a>,
    link: SettlementLink,
}

impl<'a> ReadySettlement<'a> {
    pub(super) const fn transaction(&self) -> &'a AuditedTx<'a> {
        self.tx
    }

    pub(super) fn into_state(self) -> PostingState {
        PostingState::Settlement(self.link)
    }
}

fn check_open(tx: &AuditedTx<'_>, hold: &TransactionRow, date: NaiveDate) -> Result<(), CliError> {
    let reader = tx.read();
    if hold.status() != EntryStatus::Pending || reverses_hold_event(reader, hold)? {
        return Err(HoldError::NotHold(hold.id).into());
    }
    let closed: Option<TransactionId> = reader.query_row(
        "SELECT reversal_id FROM posting_reversals WHERE company_slug = ?1 AND original_id = CAST(?2 AS TEXT)
         UNION ALL
         SELECT s.id FROM transactions s WHERE s.settles_hold = ?2
           AND NOT EXISTS (SELECT 1 FROM posting_reversals r
                           WHERE r.company_slug = s.company_slug AND r.original_id = CAST(s.id AS TEXT))
         LIMIT 1",
        params![hold.company_slug, hold.id],
        |row| row.get(0),
    ).optional()?;
    if let Some(closing) = closed {
        return Err(HoldError::Closed(hold.id, closing).into());
    }
    let latest_undo: Option<NaiveDate> = reader.query_row(
        "SELECT MAX(undo.date) FROM transactions s JOIN posting_reversals r
           ON r.company_slug = s.company_slug AND r.original_id = CAST(s.id AS TEXT)
         JOIN transactions undo ON undo.id = r.reversal_id WHERE s.settles_hold = ?1",
        [hold.id],
        |row| row.get(0),
    )?;
    let earliest = latest_undo.map_or(hold.date, |undo| undo.max(hold.date));
    if date < earliest {
        return Err(HoldError::TooEarly(earliest).into());
    }
    Ok(())
}

fn reverses_hold_event(
    reader: super::connection::ReadConn<'_>,
    row: &TransactionRow,
) -> Result<bool, CliError> {
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM posting_reversals r JOIN transactions original
           ON r.original_id = CAST(original.id AS TEXT) AND r.company_slug = original.company_slug
         WHERE r.company_slug = ?1 AND r.reversal_id = ?2
           AND (original.posting_status = 'pending' OR original.settles_hold IS NOT NULL))",
        params![row.company_slug, row.id],
        |row| row.get(0),
    )?)
}

pub(super) fn check_reversal(
    tx: &AuditedTx<'_>,
    row: &TransactionRow,
    date: NaiveDate,
) -> Result<(), CliError> {
    if row.status() == EntryStatus::Pending {
        check_open(tx, row, date)?;
    } else if reverses_hold_event(tx.read(), row)? {
        return Err(HoldError::ReversalOfMirror(row.id).into());
    }
    Ok(())
}

/// Recheck the mutable hold lifecycle when executing an already frozen proposal.
pub(super) fn approve<'a>(
    tx: &'a AuditedTx<'a>,
    params: &PostTransactionParams<'_>,
    journal: &Transaction,
    id: TransactionId,
) -> Result<ReadySettlement<'a>, CliError> {
    let company = Company::bind(tx.read(), params.company_slug.clone())?;
    let (row, entries) = transactions::get_transaction(&company, id)?;
    let hold = OpenHold::load(tx, row, &entries, params.date)?;
    let HoldJournal(pair) = HoldJournal::try_from(journal)?;
    if params.correlate.is_some()
        || journal.currency() != hold.row.currency
        || pair.debit.account().code().as_str() != hold.pair.debit.account_code
        || pair.credit.account().code().as_str() != hold.pair.credit.account_code
    {
        return Err(HoldError::PostingMismatch(id).into());
    }
    Ok(hold.settlement(pair.debit.amount().amount().try_into()?))
}

#[derive(Serialize)]
pub(crate) struct SettlementResult {
    pub(crate) transaction_id: TransactionId,
    pub(crate) settlement: SettlementLink,
    #[serde(rename = "status")]
    disposition: SettlementDisposition,
    pub(crate) currency: Currency,
    pub(crate) date: NaiveDate,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SettlementDisposition {
    Created,
    Skipped,
}

impl SettlementResult {
    fn new(
        result: PostResult,
        settlement: SettlementLink,
        currency: Currency,
        date: NaiveDate,
    ) -> Self {
        let (transaction_id, disposition) = match result {
            PostResult::Created(id) => (id, SettlementDisposition::Created),
            PostResult::Skipped(id) => (id, SettlementDisposition::Skipped),
        };
        Self {
            transaction_id,
            settlement,
            disposition,
            currency,
            date,
        }
    }

    pub(crate) fn delta(&self) -> Money {
        Money::from_minor(i128::from(self.settlement.amount_delta), self.currency)
    }
}

pub(crate) enum SettlementOutcome {
    Posted(SettlementResult),
    Proposed(ProposalCreation),
}

pub(crate) fn settle_direct(
    grant: Granted<'_, Post>,
    company: &CompanySlug,
    request: &SettlementRequest,
) -> Result<SettlementOutcome, CliError> {
    grant.mutate("sp_settle_hold", |tx, actor| {
        settle(tx, actor, company, request, |params, ready| {
            let link = ready.link;
            let posting = transactions::post_settlement(params, ready)?;
            Ok(SettlementOutcome::Posted(SettlementResult::new(
                posting,
                link,
                params.currency,
                params.date,
            )))
        })
    })
}

pub(crate) fn propose_settlement(
    grant: Granted<'_, Propose>,
    company: &CompanySlug,
    request: &SettlementRequest,
) -> Result<SettlementOutcome, CliError> {
    grant.mutate("sp_propose_settlement", |tx, actor| {
        settle(tx, actor, company, request, |params, ready| {
            proposals::reserve_in_scope(
                ready.transaction(),
                PostingPurpose::Settle(ready.link.hold_id),
                actor,
                params,
                PostingSource::Proposal,
            )
            .map(SettlementOutcome::Proposed)
        })
    })
}

fn settle(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    request: &SettlementRequest,
    execute: impl FnOnce(
        &PostTransactionParams<'_>,
        ReadySettlement<'_>,
    ) -> Result<SettlementOutcome, CliError>,
) -> Result<SettlementOutcome, CliError> {
    let scope = Company::bind(tx.read(), company.clone())?;
    let id = match &request.selector {
        HoldSelector::Id(id) => *id,
        HoldSelector::Reference(reference) => {
            transactions::find_reference(scope.read(), company, reference.as_str())?.ok_or_else(
                || {
                    CliError::NotFound(format!(
                        "no hold has reference '{reference}' in '{company}'"
                    ))
                },
            )?
        }
    };
    let (row, entries) = transactions::get_transaction(&scope, id)?;
    let amount = (&request.amount, row.currency).try_into()?;
    if let Some(existing) =
        transactions::find_reference(scope.read(), company, request.reference.as_str())?
    {
        let (posted, lines) = transactions::get_transaction(&scope, existing)?;
        if let Some(link) = posted.settlement()
            && link.hold_id == id
            && posted.date == request.date
            && lines.first().is_some_and(|line| line.amount == amount)
        {
            return Ok(SettlementOutcome::Posted(SettlementResult::new(
                PostResult::Skipped(existing),
                link,
                posted.currency,
                posted.date,
            )));
        }
        return Err(HoldError::ReferenceInUse(existing).into());
    }
    let hold = OpenHold::load(tx, row, &entries, request.date)?;
    let lines = hold.lines(amount);
    let description = hold.row.description.clone();
    let params = PostTransactionParams {
        company_slug: company,
        description: &description,
        metadata: None,
        currency: hold.row.currency,
        date: request.date,
        entries: &lines,
        correlate: None,
        reference: Some(request.reference.as_str()),
        on_conflict: ConflictStrategy::Error,
        actor,
    };
    execute(&params, hold.settlement(amount))
}
