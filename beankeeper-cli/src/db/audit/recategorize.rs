//! Move one immutable posting line with an audited, balanced journal.

use std::fmt;

use beankeeper::{
    core::{JournalEntry, Transaction},
    types::{Account, AccountCode, Currency, DebitOrCredit, Entry, Money},
};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, ReversalBlock, has_reconciled_entries,
    is_investment_trade,
    receipts::{ReceiptId, desktop_receipt_path},
    record_with_links,
    savepoint::AuditedTx,
    snapshot,
};
use crate::{
    commands::{reference::TransactionReference, txn::PostingDate},
    db::{
        Actor, TransactionRow,
        amounts::PositiveMinorUnits,
        companies::{Company, CompanySlug},
        holds::EntryStatus,
        ids::{EntryId, TransactionId},
        periods,
        transactions::{
            self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams,
        },
    },
    error::CliError,
};

/// A condition that prevents a posting line from being moved.
#[derive(Debug)]
pub enum RecategorizeBlock {
    /// A request must select the transaction by ID or by reference.
    TransactionSelector,
    /// The source transaction does not exist in this book.
    TransactionMissing,
    /// No source line matches the explicit entry or account selector.
    EntryMissing,
    /// A single source line could not be inferred.
    AmbiguousEntry,
    /// Any line or partial clearing on the transaction is reconciled.
    Reconciled,
    /// The transaction was reversed or was created to correct another one.
    ReversedOrCorrection,
    /// Pending holds and settlements carry separate lifecycle links.
    HoldOrSettlement,
    /// Investment entries require lot-aware corrections.
    InvestmentTrade,
    /// Source and destination must identify different accounts.
    SameAccount,
    /// The destination account does not exist in this book.
    DestinationMissing(AccountCode),
    /// The destination account is archived.
    DestinationArchived(AccountCode),
    /// A date earlier than the original would distort historical balances.
    BeforeOriginal(NaiveDate),
    /// An active move already offsets the selected source line.
    AlreadyMoved,
    /// A previous move was undone after the requested posting date.
    BeforePriorUndo(NaiveDate),
    /// The idempotency reference belongs to a different change.
    ReferenceConflict,
}

impl fmt::Display for RecategorizeBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionSelector => f.write_str("provide exactly one of id or reference"),
            Self::TransactionMissing => f.write_str("source transaction not found in this company"),
            Self::EntryMissing => f.write_str("source entry not found on the selected transaction and account"),
            Self::AmbiguousEntry => f.write_str("could not identify one source entry. Specify its entry ID, or an account with exactly one line on the transaction"),
            Self::Reconciled => f.write_str("the source transaction has a reconciled entry or clearing"),
            Self::ReversedOrCorrection => f.write_str("the source transaction is reversed or is itself a correction"),
            Self::HoldOrSettlement => f.write_str("pending holds and hold settlements cannot be recategorized"),
            Self::InvestmentTrade => f.write_str("investment trades require a lot-aware correction"),
            Self::SameAccount => f.write_str("source and destination accounts must differ"),
            Self::DestinationMissing(code) => write!(f, "destination account '{code}' not found in this company"),
            Self::DestinationArchived(code) => write!(f, "destination account '{code}' is archived"),
            Self::BeforeOriginal(date) => write!(f, "recategorization date must be on or after the original date {date}"),
            Self::AlreadyMoved => f.write_str("this entry has an active recategorization. Undo its receipt before moving it again"),
            Self::BeforePriorUndo(date) => write!(f, "this entry was recategorized before. Choose a date on or after its last undo on {date}"),
            Self::ReferenceConflict => f.write_str("recategorization reference is already in use. For an exact retry, use the same request and conflict strategy 'skip'"),
        }
    }
}

impl std::error::Error for RecategorizeBlock {}

impl From<RecategorizeBlock> for CliError {
    fn from(block: RecategorizeBlock) -> Self {
        Self::RecategorizeBlocked(block)
    }
}

/// Typed transport values before the cross-field selector is validated.
pub(crate) struct Input {
    pub id: Option<TransactionId>,
    pub reference: Option<TransactionReference>,
    pub from_account: Option<AccountCode>,
    pub entry_id: Option<EntryId>,
    pub to_account: AccountCode,
    pub date: Option<PostingDate>,
    pub tax_category: Option<String>,
    pub description: Option<String>,
    pub posting_reference: Option<TransactionReference>,
    pub on_conflict: Option<ConflictStrategy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionSelector {
    Id(TransactionId),
    Reference(TransactionReference),
}

impl TryFrom<(Option<TransactionId>, Option<TransactionReference>)> for TransactionSelector {
    type Error = RecategorizeBlock;

    fn try_from(
        selectors: (Option<TransactionId>, Option<TransactionReference>),
    ) -> Result<Self, Self::Error> {
        match selectors {
            (Some(id), None) => Ok(Self::Id(id)),
            (None, Some(reference)) => Ok(Self::Reference(reference)),
            _ => Err(RecategorizeBlock::TransactionSelector),
        }
    }
}

impl TransactionSelector {
    fn load(&self, company: &Company<'_>) -> Result<TransactionRow, CliError> {
        let id = match self {
            Self::Id(id) => *id,
            Self::Reference(reference) => {
                transactions::find_reference(company.read(), company.slug(), reference.as_str())?
                    .ok_or(RecategorizeBlock::TransactionMissing)?
            }
        };
        let row = transactions::find_transaction_row(company, id)?
            .ok_or(RecategorizeBlock::TransactionMissing)?;
        super::documents::guard_linked_posting(company, id)?;
        Ok(row)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Details {
    transaction: TransactionSelector,
    from_account: Option<AccountCode>,
    entry_id: Option<EntryId>,
    to_account: AccountCode,
    date: Option<NaiveDate>,
    tax_category: Option<String>,
    description: Option<String>,
    reference: Option<TransactionReference>,
}

/// A validated request to recategorize one immutable posting line.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recategorization {
    details: Details,
    conflict: ConflictStrategy,
}

impl TryFrom<Input> for Recategorization {
    type Error = CliError;

    fn try_from(input: Input) -> Result<Self, Self::Error> {
        Ok(Self {
            details: Details {
                transaction: (input.id, input.reference).try_into()?,
                from_account: input.from_account,
                entry_id: input.entry_id,
                to_account: input.to_account,
                date: input.date.map(Into::into),
                tax_category: input.tax_category,
                description: input.description,
                reference: input.posting_reference,
            },
            conflict: input.on_conflict.unwrap_or_default(),
        })
    }
}

struct EffectiveDate(NaiveDate);

impl From<EffectiveDate> for NaiveDate {
    fn from(date: EffectiveDate) -> Self {
        date.0
    }
}

impl TryFrom<(Option<NaiveDate>, NaiveDate, MoveHistory)> for EffectiveDate {
    type Error = RecategorizeBlock;

    fn try_from(
        (requested, original, history): (Option<NaiveDate>, NaiveDate, MoveHistory),
    ) -> Result<Self, Self::Error> {
        let date = requested.unwrap_or(original);
        if date < original {
            return Err(RecategorizeBlock::BeforeOriginal(original));
        }
        match history {
            MoveHistory::Active => Err(RecategorizeBlock::AlreadyMoved),
            MoveHistory::UndoneOn(undo) if date < undo => {
                Err(RecategorizeBlock::BeforePriorUndo(undo))
            }
            _ => Ok(Self(date)),
        }
    }
}

struct SourceLine {
    id: EntryId,
    account: Account,
    direction: DebitOrCredit,
    amount: PositiveMinorUnits,
    memo: Option<String>,
    tax_category: Option<String>,
}

impl SourceLine {
    fn load(
        company: &Company<'_>,
        original: TransactionId,
        request: &Details,
    ) -> Result<Self, CliError> {
        let explicit = request.from_account.is_some() || request.entry_id.is_some();
        let mut statement = company.read().prepare(
            "SELECT e.id, a.code, a.name, a.type, e.direction, e.amount, e.memo, e.tax_category
             FROM entries e JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code
             WHERE e.company_slug = ?1 AND e.transaction_id = ?2
               AND (?3 IS NULL OR e.id = ?3) AND (?4 IS NULL OR a.code = ?4)
               AND (?5 OR (a.type NOT IN ('asset', 'liability') AND a.code <> ?6))
             ORDER BY e.id LIMIT 2",
        )?;
        let mut rows = statement.query(params![
            company.slug(),
            original,
            request.entry_id,
            request.from_account,
            explicit,
            request.to_account,
        ])?;
        let Some(row) = rows.next()? else {
            return Err(if explicit {
                RecategorizeBlock::EntryMissing
            } else {
                RecategorizeBlock::AmbiguousEntry
            }
            .into());
        };
        let source = Self {
            id: row.get(0)?,
            account: Account::new(row.get(1)?, row.get::<_, String>(2)?, row.get(3)?),
            direction: row.get(4)?,
            amount: row.get(5)?,
            memo: row.get(6)?,
            tax_category: row.get(7)?,
        };
        if rows.next()?.is_some() {
            return Err(RecategorizeBlock::AmbiguousEntry.into());
        }
        Ok(source)
    }
}

/// The immutable line movement recorded on the receipt and available to agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movement {
    entry_id: EntryId,
    from_account: AccountCode,
    to_account: AccountCode,
    direction: DebitOrCredit,
    amount: PositiveMinorUnits,
    currency: Currency,
    date: NaiveDate,
}

impl Movement {
    fn summary(&self, original: TransactionId) -> String {
        let amount = Money::new(self.amount.into(), self.currency);
        format!(
            "Moved {amount} on transaction #{original} from account {} to account {}.",
            self.from_account, self.to_account
        )
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RecordedMove {
    movement: Movement,
    request: Details,
    source_reference: Option<String>,
}

impl RecordedMove {
    pub(super) fn into_receipt(self) -> (Movement, Option<String>) {
        (self.movement, self.source_reference)
    }
}

struct ReadyMove {
    original: TransactionId,
    source_reference: Option<String>,
    movement: Movement,
    entries: [PostEntryParams; 2],
    journal: Transaction,
}

impl ReadyMove {
    fn load(
        tx: &AuditedTx<'_>,
        company: &Company<'_>,
        request: &Details,
    ) -> Result<Self, CliError> {
        let original = request.transaction.load(company)?;
        if crate::db::fixed_assets::protects(tx.read(), company.slug(), original.id)? {
            return Err(CliError::Validation(
                "fixed asset movements cannot be recategorized as standalone transactions".into(),
            ));
        }
        if crate::db::expense_claims::protects(tx.read(), company.slug(), original.id)? {
            return Err(CliError::Validation(
                "expense claim journals require a claim correction".into(),
            ));
        }
        check_original(tx, company, &original)?;
        let source = SourceLine::load(company, original.id, request)?;
        let accounts = MoveAccounts::try_from((company, source, &request.to_account))?;
        let history = MoveHistory::load(company, original.id, Some(accounts.source.id))?;
        let date = EffectiveDate::try_from((request.date, original.date, history))?;
        Self::build(original, accounts, date, request)
    }

    fn build(
        original: TransactionRow,
        accounts: MoveAccounts,
        date: EffectiveDate,
        request: &Details,
    ) -> Result<Self, CliError> {
        let MoveAccounts {
            source,
            destination,
        } = accounts;
        let date = NaiveDate::from(date);
        let movement = Movement {
            entry_id: source.id,
            from_account: source.account.code().clone(),
            to_account: destination.code().clone(),
            direction: source.direction,
            amount: source.amount,
            currency: original.currency,
            date,
        };
        let description =
            MoveDescription::from((request.description.as_deref(), original.id, &movement));
        let amount = Money::new(source.amount.into(), original.currency);
        let debit_or_credit = source.direction;
        let offset = make_entry(
            source.account,
            amount,
            debit_or_credit.opposite(),
            source.memo.as_deref(),
            source.tax_category.as_deref(),
        )?;
        let replacement = make_entry(
            destination,
            amount,
            debit_or_credit,
            source.memo.as_deref(),
            request
                .tax_category
                .as_deref()
                .or(source.tax_category.as_deref()),
        )?;
        let entries = [&offset, &replacement].map(|entry| PostEntryParams {
            account_code: entry.account().code().to_string(),
            direction: entry.direction(),
            amount: source.amount,
            memo: entry.memo().map(str::to_owned),
            tax_category: entry.tax_category().map(str::to_owned),
        });
        let journal = JournalEntry::new(date, description.0, original.currency)
            .entry(offset)?
            .entry(replacement)?
            .post()?;
        Ok(Self {
            original: original.id,
            source_reference: original.reference,
            movement,
            entries,
            journal,
        })
    }

    fn post(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        company: &CompanySlug,
        request: &Recategorization,
    ) -> Result<Applied, CliError> {
        let reference = request
            .details
            .reference
            .as_ref()
            .map(TransactionReference::as_str);
        let params = PostTransactionParams {
            company_slug: company,
            description: self.journal.description(),
            metadata: None,
            currency: self.movement.currency,
            date: self.movement.date,
            entries: &self.entries,
            correlate: None,
            reference,
            on_conflict: ConflictStrategy::Error,
            actor,
        };
        let posting = RecategorizationPosting {
            tx,
            params,
            journal: &self.journal,
        };
        let PostResult::Created(id) = transactions::post_recategorization(&posting)? else {
            return Err(CliError::Database(
                "recategorization posting unexpectedly skipped".into(),
            ));
        };
        let recorded = RecordedMove {
            movement: self.movement,
            request: request.details.clone(),
            source_reference: self.source_reference,
        };
        record_with_links(
            tx.connection(),
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Transaction,
                entity_id: &self.original.to_string(),
                action: AuditAction::Recategorize,
                snapshots: (None, Some(&snapshot(&recorded))),
            },
            AuditLinks::Posting(id),
        )?;
        let receipt_id = tx.connection().last_insert_rowid().try_into()?;
        Ok(Applied::new(
            self.original,
            id,
            receipt_id,
            recorded.movement,
        ))
    }
}

struct MoveDescription(String);

impl From<(Option<&str>, TransactionId, &Movement)> for MoveDescription {
    fn from((override_text, original, movement): (Option<&str>, TransactionId, &Movement)) -> Self {
        Self(override_text.map_or_else(
            || {
                format!(
                    "Recategorize #{original}: {} -> {}",
                    movement.from_account, movement.to_account
                )
            },
            str::to_owned,
        ))
    }
}

fn make_entry(
    account: Account,
    amount: Money,
    direction: DebitOrCredit,
    memo: Option<&str>,
    tax: Option<&str>,
) -> Result<Entry, CliError> {
    let entry = match memo {
        Some(memo) => Entry::with_memo(account, amount, direction, memo)?,
        None => Entry::new(account, amount, direction)?,
    };
    Ok(match tax {
        Some(tax) => entry.with_tax_category(tax),
        None => entry,
    })
}

struct MoveAccounts {
    source: SourceLine,
    destination: Account,
}

impl TryFrom<(&Company<'_>, SourceLine, &AccountCode)> for MoveAccounts {
    type Error = CliError;

    fn try_from(
        (company, source, code): (&Company<'_>, SourceLine, &AccountCode),
    ) -> Result<Self, Self::Error> {
        if source.account.code() == code {
            return Err(RecategorizeBlock::SameAccount.into());
        }
        let (name, account_type, archived) = company
            .read()
            .query_row(
                "SELECT name, type, archived FROM accounts WHERE company_slug = ?1 AND code = ?2",
                params![company.slug(), code],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| RecategorizeBlock::DestinationMissing(code.clone()))?;
        if archived {
            return Err(RecategorizeBlock::DestinationArchived(code.clone()).into());
        }
        Ok(Self {
            source,
            destination: Account::new(code.clone(), name, account_type),
        })
    }
}

/// A balanced move and its posting metadata sealed inside the authorized write scope.
pub(in crate::db) struct RecategorizationPosting<'a> {
    tx: &'a AuditedTx<'a>,
    params: PostTransactionParams<'a>,
    journal: &'a Transaction,
}

impl RecategorizationPosting<'_> {
    pub(in crate::db) fn parts(
        &self,
    ) -> (&AuditedTx<'_>, &PostTransactionParams<'_>, &Transaction) {
        (self.tx, &self.params, self.journal)
    }
}

fn check_original(
    tx: &AuditedTx<'_>,
    company: &Company<'_>,
    original: &TransactionRow,
) -> Result<(), CliError> {
    let id = original.id;
    if original.status() == EntryStatus::Pending || original.settlement().is_some() {
        return Err(RecategorizeBlock::HoldOrSettlement.into());
    }
    if is_investment_trade(tx.connection(), id)? {
        return Err(RecategorizeBlock::InvestmentTrade.into());
    }
    if has_reconciled_entries(tx.connection(), id)? {
        return Err(RecategorizeBlock::Reconciled.into());
    }
    let corrected: bool = company.read().query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM posting_reversals
         WHERE company_slug = ?1 AND (original_id = CAST(?2 AS TEXT) OR reversal_id = ?2))
         OR EXISTS(SELECT 1 FROM {audit} WHERE company_slug = ?1 AND action = 'recategorize'
                   AND related_posting = ?2)",
            audit = company.read().bookkeeping_audit()
        ),
        params![company.slug(), id],
        |row| row.get(0),
    )?;
    if corrected {
        return Err(RecategorizeBlock::ReversedOrCorrection.into());
    }
    Ok(())
}

pub(super) enum MoveHistory {
    None,
    Active,
    UndoneOn(NaiveDate),
}

impl MoveHistory {
    pub(super) fn check_reversal(
        self,
        original: TransactionId,
        date: NaiveDate,
    ) -> Result<(), CliError> {
        let block = match self {
            Self::Active => ReversalBlock::HasRecategorization(original),
            Self::UndoneOn(undo) if date < undo => ReversalBlock::BeforeRecategorizationUndo(undo),
            _ => return Ok(()),
        };
        Err(CliError::ReversalBlocked(block))
    }

    pub(super) fn load(
        company: &Company<'_>,
        original: TransactionId,
        entry: Option<EntryId>,
    ) -> Result<Self, CliError> {
        let latest = company
            .read()
            .query_row(
                &format!(
                    "SELECT reversal.date FROM {audit} event
             LEFT JOIN posting_reversals r ON r.company_slug = event.company_slug
                AND r.original_id = CAST(event.related_posting AS TEXT)
             LEFT JOIN transactions reversal ON reversal.id = r.reversal_id
             WHERE event.company_slug = ?1 AND event.entity_id = CAST(?2 AS TEXT)
               AND event.action = 'recategorize'
               AND (?3 IS NULL OR json_extract(event.after, '$.movement.entry_id') = ?3)
             ORDER BY reversal.date IS NOT NULL, reversal.date DESC LIMIT 1",
                    audit = company.read().bookkeeping_audit()
                ),
                params![company.slug(), original, entry],
                |row| row.get::<_, Option<NaiveDate>>(0),
            )
            .optional()?;
        Ok(match latest {
            None => Self::None,
            Some(None) => Self::Active,
            Some(Some(date)) => Self::UndoneOn(date),
        })
    }
}

/// The posted movement, its audit receipt, and the Desktop receipt path.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Applied {
    original_id: TransactionId,
    id: TransactionId,
    receipt_id: ReceiptId,
    skipped: bool,
    #[serde(flatten)]
    movement: Movement,
    summary: String,
}

impl Applied {
    fn new(
        original: TransactionId,
        posting: TransactionId,
        receipt: ReceiptId,
        movement: Movement,
    ) -> Self {
        Self {
            original_id: original,
            id: posting,
            receipt_id: receipt,
            skipped: false,
            summary: movement.summary(original),
            movement,
        }
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }

    pub(crate) const fn receipt(&self) -> ReceiptId {
        self.receipt_id
    }

    pub(crate) fn desktop_path(&self, company: &CompanySlug) -> String {
        desktop_receipt_path(company, self.receipt_id)
    }

    fn retry(mut self) -> Self {
        self.skipped = true;
        self
    }
}

impl Recategorization {
    pub(super) fn same_intent(&self, other: &Self) -> bool {
        self.details == other.details
    }

    fn existing(&self, company: &Company<'_>) -> Result<Option<Applied>, CliError> {
        let Some(reference) = &self.details.reference else {
            return Ok(None);
        };
        let Some(posting) =
            transactions::find_reference(company.read(), company.slug(), reference.as_str())?
        else {
            return Ok(None);
        };
        if self.conflict == ConflictStrategy::Error {
            return Err(RecategorizeBlock::ReferenceConflict.into());
        }
        let (receipt, original, raw): (ReceiptId, TransactionId, String) = company
            .read()
            .query_row(
                &format!(
                    "SELECT id, CAST(entity_id AS INTEGER), after FROM {audit}
             WHERE company_slug = ?1 AND action = 'recategorize' AND related_posting = ?2",
                    audit = company.read().bookkeeping_audit()
                ),
                params![company.slug(), posting],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(RecategorizeBlock::ReferenceConflict)?;
        let recorded: RecordedMove = serde_json::from_str(&raw).map_err(|error| {
            CliError::Database(format!(
                "invalid recategorization receipt #{receipt}: {error}"
            ))
        })?;
        if recorded.request != self.details {
            return Err(RecategorizeBlock::ReferenceConflict.into());
        }
        Ok(Some(
            Applied::new(original, posting, receipt, recorded.movement).retry(),
        ))
    }

    pub(super) fn apply(
        &self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        company: &Company<'_>,
    ) -> Result<Applied, CliError> {
        if let Some(applied) = self.existing(company)? {
            return Ok(applied);
        }
        ReadyMove::load(tx, company, &self.details)?.post(tx, actor, company.slug(), self)
    }

    pub(super) fn preview(
        &self,
        tx: &AuditedTx<'_>,
        company: &Company<'_>,
    ) -> Result<Option<Applied>, CliError> {
        if let Some(applied) = self.existing(company)? {
            return Ok(Some(applied));
        }
        let ready = ReadyMove::load(tx, company, &self.details)?;
        periods::check_open_date(tx.read(), company.slug(), ready.movement.date)?;
        Ok(None)
    }
}
