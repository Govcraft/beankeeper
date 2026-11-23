//! Company-scoped audit receipts and immediate owner undo.

use std::{fmt, num::NonZeroU8, str::FromStr};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{AuditAction, audit_columns, read_audit_row};
use crate::db::Db;
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::ReadConn;
use crate::db::ids::{AuditRowId, TransactionId};
use crate::error::CliError;

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
pub(super) use undo::UndoPostingDate;
pub(in crate::db) use undo::verify_archives;
pub use undo::{ReceiptUndoError, UndoIneligibility, UndoOutcome, undo_receipt};

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

impl fmt::Display for AuditCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        i64::from(*self).fmt(formatter)
    }
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
            .ok_or_else(|| {
                CliError::Validation(format!(
                    "receipt page limit must be between 1 and {}",
                    super::AuditPageLimit::MAX
                ))
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
    after: AuditCursor,
    through: AuditCursor,
}

/// One page in an ascending, bounded receipt range.
#[derive(Debug)]
pub struct ReceiptPage {
    receipts: Vec<AuditReceipt>,
    through: AuditCursor,
    continuation: Option<ReceiptContinuation>,
}

impl ReceiptPage {
    /// Receipts in ascending order of their anchor audit row.
    #[must_use]
    pub fn receipts(&self) -> &[AuditReceipt] {
        &self.receipts
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
        read_page(reader, &next.company, next.after, next.through, limit)
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
    Company::bind(reader, company.clone())?;
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
                    after: AuditCursor(Some(receipt.id.into())),
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
