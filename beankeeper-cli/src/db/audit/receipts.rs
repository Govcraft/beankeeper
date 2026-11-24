//! Company-scoped audit receipts and immediate owner undo.

use std::{borrow::Cow, fmt, num::NonZeroU8, str::FromStr};

use rusqlite::{OptionalExtension, params};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{AuditAction, audit_columns, read_audit_row};
use crate::db::Db;
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::ReadConn;
use crate::db::ids::{AuditRowId, TransactionId};
use crate::error::CliError;

pub use super::account_rename::AccountLabel;
pub use crate::db::ids::ReceiptId;

mod event;
mod links;
pub use event::{
    AccountSnapshot, PostingEntrySnapshot, PostingSnapshot, ProposalSnapshot, ProposalStatus,
    ReceiptChange, ReceiptEvent, ReceiptTarget,
};
mod undo;
pub(in crate::db) use links::{backfill_legacy_links, seal_legacy_links};
pub(super) use links::{verify_legacy_links, verify_links};
pub(crate) use undo::undo_as_agent;
pub(in crate::db) use undo::verify_archives;
pub(super) use undo::{AgentReceiptUndo, UndoPostingDate};
pub use undo::{
    OnboardingUndoOutcome, OwnerUndoKind, ReceiptUndoError, ReceiptUndoOutcome, RuleUndoOutcome,
    UndoIneligibility, UndoOutcome, undo_change, undo_receipt, undo_rule_creation,
};

/// An exclusive position in the ledger's audit order. Zero starts at the beginning.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct AuditCursor(Option<AuditRowId>);

impl TryFrom<i64> for AuditCursor {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value == 0 {
            Ok(Self::default())
        } else {
            AuditRowId::try_from(value).map(|id| Self(Some(id)))
        }
    }
}

impl FromStr for AuditCursor {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|error| CliError::Validation(format!("invalid audit cursor: {error}")))?
            .try_into()
    }
}

impl From<AuditCursor> for i64 {
    fn from(value: AuditCursor) -> Self {
        value.0.map_or(0, AuditRowId::get)
    }
}

impl From<ReceiptId> for AuditCursor {
    fn from(id: ReceiptId) -> Self {
        Self(Some(id.into()))
    }
}

impl AuditCursor {
    /// The exclusive cursor immediately before a receipt ID.
    #[must_use]
    pub fn before(receipt: ReceiptId) -> Self {
        Self(receipt.predecessor())
    }
}

impl JsonSchema for AuditCursor {
    fn schema_name() -> Cow<'static, str> {
        "AuditCursor".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "format": "int64", "minimum": 0, "default": i64::from(Self::default())})
    }
}

impl fmt::Display for AuditCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        i64::from(*self).fmt(formatter)
    }
}

pub(crate) fn desktop_receipt_path(company: &CompanySlug, id: ReceiptId) -> String {
    let path = changes_path(company, AuditCursor::before(id), id.into());
    format!("{path}#receipt-{id}")
}

pub(crate) fn changes_path(
    company: &CompanySlug,
    after: AuditCursor,
    through: AuditCursor,
) -> String {
    // CompanySlug contains only characters safe in a query value.
    format!("/changes?company={company}&after={after}&through={through}")
}

/// A positive number of receipts per page, up to the audit page maximum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct ReceiptPageLimit(NonZeroU8);

impl Default for ReceiptPageLimit {
    fn default() -> Self {
        Self(NonZeroU8::new(super::AuditPageLimit::default().get()).unwrap_or(NonZeroU8::MIN))
    }
}

impl TryFrom<u8> for ReceiptPageLimit {
    type Error = CliError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        NonZeroU8::new(value)
            .filter(|value| value.get() <= super::AuditPageLimit::MAX)
            .map(Self)
            .ok_or_else(Self::invalid)
    }
}

impl ReceiptPageLimit {
    fn invalid() -> CliError {
        CliError::Validation(format!(
            "receipt page limit must be between 1 and {}",
            super::AuditPageLimit::MAX
        ))
    }
}

impl FromStr for ReceiptPageLimit {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<u8>().map_err(|_| Self::invalid())?.try_into()
    }
}

impl JsonSchema for ReceiptPageLimit {
    fn schema_name() -> Cow<'static, str> {
        "ReceiptPageLimit".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer", "minimum": 1, "maximum": super::AuditPageLimit::MAX,
            "default": u8::from(Self::default())
        })
    }
}

impl From<ReceiptPageLimit> for u8 {
    fn from(limit: ReceiptPageLimit) -> Self {
        limit.0.get()
    }
}

/// A continuation for the same company and bounded range, on the same ledger.
#[derive(Debug, Clone)]
pub struct ReceiptContinuation {
    company: CompanySlug,
    last: ReceiptId,
    through: AuditCursor,
}

impl ReceiptContinuation {
    /// The company for which this continuation was issued.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        &self.company
    }

    /// The exclusive cursor for the next page.
    #[must_use]
    pub fn after(&self) -> AuditCursor {
        self.last.into()
    }
}

impl TryFrom<(CompanySlug, ReceiptId, AuditCursor)> for ReceiptContinuation {
    type Error = CliError;

    fn try_from(
        (company, last, through): (CompanySlug, ReceiptId, AuditCursor),
    ) -> Result<Self, Self::Error> {
        if AuditCursor::from(last) >= through {
            return Err(CliError::Validation(
                "receipt continuation must advance within its audit range".into(),
            ));
        }
        Ok(Self {
            company,
            last,
            through,
        })
    }
}

impl FromStr for ReceiptContinuation {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(company), Some(last), Some(through), None) => {
                (company.parse()?, last.parse()?, through.parse()?).try_into()
            }
            _ => Err(CliError::Validation("invalid receipt continuation".into())),
        }
    }
}

impl fmt::Display for ReceiptContinuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.company, self.last, self.through)
    }
}

impl Serialize for ReceiptContinuation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReceiptContinuation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ReceiptContinuation {
    fn schema_name() -> Cow<'static, str> {
        "ReceiptContinuation".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "description": "Copy the continuation from the previous receipt page."})
    }
}

/// One page in an ascending, bounded receipt range.
#[derive(Debug, Serialize)]
pub struct ReceiptPage {
    receipts: Vec<AuditReceipt>,
    after: AuditCursor,
    through: AuditCursor,
    continuation: Option<ReceiptContinuation>,
}

impl ReceiptPage {
    /// Receipts in ascending order of their anchor audit row.
    #[must_use]
    pub fn receipts(&self) -> &[AuditReceipt] {
        &self.receipts
    }

    /// The exclusive lower bound of this page.
    #[must_use]
    pub const fn after(&self) -> AuditCursor {
        self.after
    }

    /// The last cursor covered by this page, without including later pages.
    #[must_use]
    pub fn page_through(&self) -> AuditCursor {
        self.continuation
            .as_ref()
            .map_or(self.through, ReceiptContinuation::after)
    }

    /// The fixed upper bound. Save this as the next range's cursor when the
    /// whole range has been consumed, including when the range is empty.
    #[must_use]
    pub const fn through(&self) -> AuditCursor {
        self.through
    }

    /// Present when the current range has another page.
    #[must_use]
    pub const fn continuation(&self) -> Option<&ReceiptContinuation> {
        self.continuation.as_ref()
    }
}

#[derive(Debug)]
enum ReceiptStart {
    At(CompanySlug, AuditCursor),
    Continue(ReceiptContinuation),
}

/// A company-scoped request to start or continue a bounded receipt range.
#[derive(Debug)]
pub struct ReceiptQuery {
    start: ReceiptStart,
    limit: ReceiptPageLimit,
}

impl
    TryFrom<(
        CompanySlug,
        Option<AuditCursor>,
        Option<ReceiptContinuation>,
        Option<ReceiptPageLimit>,
    )> for ReceiptQuery
{
    type Error = CliError;

    fn try_from(
        (company, cursor, continuation, limit): (
            CompanySlug,
            Option<AuditCursor>,
            Option<ReceiptContinuation>,
            Option<ReceiptPageLimit>,
        ),
    ) -> Result<Self, Self::Error> {
        let start = match (cursor, continuation) {
            (Some(_), Some(_)) => {
                return Err(CliError::Validation(
                    "choose either a receipt cursor or a continuation".into(),
                ));
            }
            (None, Some(next)) if next.company() != &company => {
                return Err(CliError::Validation(
                    "receipt continuation belongs to a different company".into(),
                ));
            }
            (None, Some(next)) => ReceiptStart::Continue(next),
            (cursor, None) => ReceiptStart::At(company, cursor.unwrap_or_default()),
        };
        Ok(Self {
            start,
            limit: limit.unwrap_or_default(),
        })
    }
}

impl ReceiptQuery {
    /// The company whose receipt history is requested.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        match &self.start {
            ReceiptStart::At(company, _) => company,
            ReceiptStart::Continue(next) => next.company(),
        }
    }

    /// Read the requested page in a consistent ledger snapshot.
    ///
    /// # Errors
    /// Returns scope, invalid cursor or database errors.
    pub fn read(&self, db: &Db) -> Result<ReceiptPage, CliError> {
        match &self.start {
            ReceiptStart::At(company, cursor) => list_receipts(db, company, *cursor, self.limit),
            ReceiptStart::Continue(next) => continue_receipts(db, next, self.limit),
        }
    }
}

/// A recorded action and the posting made as part of that action, if any.
#[derive(Debug, Serialize)]
pub struct AuditReceipt {
    id: ReceiptId,
    change: ReceiptEvent,
    posting: Option<ReceiptEvent>,
}

impl AuditReceipt {
    /// ID of the receipt's primary audit change.
    #[must_use]
    pub const fn id(&self) -> ReceiptId {
        self.id
    }

    /// The primary change. Its audit ID sets the receipt's order.
    #[must_use]
    pub const fn change(&self) -> &ReceiptEvent {
        &self.change
    }

    /// The explicitly linked posting creation, if made in the same company
    /// as part of this reversal or proposal approval.
    #[must_use]
    pub const fn posting(&self) -> Option<&ReceiptEvent> {
        self.posting.as_ref()
    }

    /// Reference recorded on the primary change or its linked posting.
    #[must_use]
    pub fn source_reference(&self) -> Option<&str> {
        self.change.source_reference().or_else(|| {
            self.posting
                .as_ref()
                .and_then(ReceiptEvent::source_reference)
        })
    }
}

/// Start a receipt range for one company, capturing the committed audit head.
/// Use the returned continuation to finish it before starting another range.
///
/// # Errors
/// Returns a scope, invalid cursor, or storage error.
pub fn list_receipts(
    db: &Db,
    company: &CompanySlug,
    after: AuditCursor,
    limit: ReceiptPageLimit,
) -> Result<ReceiptPage, CliError> {
    db.read_snapshot(|reader| {
        let through = head(reader)?;
        if after > through {
            return Err(CliError::Validation(
                "audit cursor is ahead of the ledger's audit head".into(),
            ));
        }
        read_page(reader, company, after, through, limit)
    })
}

/// Continue the originally captured range on the same ledger.
///
/// # Errors
/// Returns a scope or storage error.
pub fn continue_receipts(
    db: &Db,
    next: &ReceiptContinuation,
    limit: ReceiptPageLimit,
) -> Result<ReceiptPage, CliError> {
    db.read_snapshot(|reader| {
        if next.through > head(reader)? {
            return Err(CliError::Validation(
                "receipt continuation is ahead of the ledger's audit head".into(),
            ));
        }
        read_page(reader, &next.company, next.after(), next.through, limit)
    })
}

fn head(reader: ReadConn<'_>) -> Result<AuditCursor, CliError> {
    reader
        .query_row("SELECT COALESCE(MAX(id), 0) FROM audit_log", [], |row| {
            row.get::<_, i64>(0)
        })?
        .try_into()
}

fn read_page(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    after: AuditCursor,
    through: AuditCursor,
    limit: ReceiptPageLimit,
) -> Result<ReceiptPage, CliError> {
    Company::bind_retained(reader, company.clone())?;
    let columns = audit_columns(crate::db::schema::CURRENT_VERSION);
    let mut statement = reader.prepare(&format!(
        "SELECT {columns} FROM audit_log a
         WHERE a.company_slug = ?1 AND a.id > ?2 AND a.id <= ?3
           AND NOT (a.entity = 'transaction' AND a.action = 'create' AND EXISTS (
               SELECT 1 FROM audit_receipt_postings p
               WHERE p.company_slug = a.company_slug AND p.posting_id = CAST(a.entity_id AS INTEGER)
                 AND p.id > a.id AND p.id <= ?3
           ))
         ORDER BY a.id LIMIT ?4"
    ))?;
    let rows = statement.query_map(
        params![
            company,
            i64::from(after),
            i64::from(through),
            i64::from(u8::from(limit)) + 1
        ],
        read_audit_row,
    )?;
    let mut receipts = Vec::new();
    let mut continuation = None;
    for row in rows {
        let row = row?;
        if receipts.len() == usize::from(u8::from(limit)) {
            continuation = receipts
                .last()
                .map(|receipt: &AuditReceipt| ReceiptContinuation {
                    company: company.clone(),
                    last: receipt.id,
                    through,
                });
            break;
        }
        let change = ReceiptEvent::try_from(row)?;
        let id = ReceiptId::from(change.id());
        let posting = match change.action() {
            AuditAction::Reverse | AuditAction::ProposalApprove => change
                .related_posting()
                .map(|posting| posting_creation(reader, company, posting, change.id()))
                .transpose()?
                .flatten(),
            _ => None,
        };
        receipts.push(AuditReceipt {
            id,
            change,
            posting,
        });
    }
    Ok(ReceiptPage {
        receipts,
        after,
        through,
        continuation,
    })
}

fn posting_creation(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    posting: TransactionId,
    before: AuditRowId,
) -> Result<Option<ReceiptEvent>, CliError> {
    let columns = audit_columns(crate::db::schema::CURRENT_VERSION);
    reader
        .query_row(
            &format!(
                "SELECT {columns} FROM audit_log
                  WHERE company_slug = ?1 AND entity = 'transaction' AND action = 'create'
                    AND entity_id = CAST(?2 AS TEXT) AND id < ?3 ORDER BY id DESC LIMIT 1"
            ),
            params![company, posting, before],
            read_audit_row,
        )
        .optional()?
        .map(ReceiptEvent::try_from)
        .transpose()
}
