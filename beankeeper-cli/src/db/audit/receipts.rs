//! Company-scoped audit receipts and immediate owner undo.

use std::{fmt, num::NonZeroU8, str::FromStr};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AuditAction, AuditEntity, AuditLogRow, ClientHint, audit_columns, read_audit_row};
use crate::db::Db;
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::ReadConn;
use crate::db::ids::{AuditRowId, TransactionId};
use crate::error::CliError;

pub use crate::db::ids::ReceiptId;

mod undo;
pub(in crate::db) use undo::verify_archives;
pub use undo::{ReceiptUndoError, UndoOutcome, undo_receipt};

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
        self.change.source_reference.as_deref().or_else(|| {
            self.posting
                .as_ref()
                .and_then(|event| event.source_reference.as_deref())
        })
    }
}

/// One immutable event as recorded in the audit trail.
/// Snapshots are decoded JSON values. Historical text is returned as a string.
#[derive(Debug, Serialize)]
pub struct ReceiptEvent {
    pub id: AuditRowId,
    pub changed_at: String,
    pub actor: String,
    pub agent_uri: Option<String>,
    pub token_jti: Option<String>,
    pub client_hint: Option<ClientHint>,
    pub entity: AuditEntity,
    pub entity_id: String,
    pub action: AuditAction,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub source_reference: Option<String>,
    pub related_posting: Option<TransactionId>,
    pub undo_of: Option<ReceiptId>,
}

impl TryFrom<AuditLogRow> for ReceiptEvent {
    type Error = CliError;

    fn try_from(row: AuditLogRow) -> Result<Self, CliError> {
        let before = row.before.map(decode_snapshot);
        let after = row.after.map(decode_snapshot);
        let source_reference = after
            .iter()
            .chain(before.iter())
            .find_map(|snapshot| {
                snapshot
                    .get("source_reference")
                    .and_then(Value::as_str)
                    .or_else(|| snapshot.get("reference").and_then(Value::as_str))
            })
            .map(str::to_owned);
        let relation = match row.action {
            AuditAction::Reverse => Some("reversed_by"),
            AuditAction::ProposalApprove => Some("transaction_id"),
            AuditAction::Correlate => Some("correlated_with"),
            _ => None,
        };
        let related_posting = relation
            .and_then(|field| after.as_ref()?.get(field)?.as_i64())
            .map(TransactionId::try_from)
            .transpose()?;
        let undo_of = if matches!(
            row.action,
            AuditAction::Reverse | AuditAction::AccountArchive
        ) {
            after
                .as_ref()
                .and_then(|value| value.get("undo_of"))
                .and_then(Value::as_i64)
                .map(ReceiptId::try_from)
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            id: row.id.try_into()?,
            changed_at: row.changed_at,
            actor: row.actor,
            agent_uri: row.agent_uri,
            token_jti: row.token_jti,
            client_hint: row.client_hint,
            entity: row.entity,
            entity_id: row.entity_id,
            action: row.action,
            before,
            after,
            source_reference,
            related_posting,
            undo_of,
        })
    }
}

fn decode_snapshot(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::String(text))
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
        let id = row.id.try_into()?;
        let change = ReceiptEvent::try_from(row)?;
        let posting = match change.action {
            AuditAction::Reverse | AuditAction::ProposalApprove => change
                .related_posting
                .map(|posting| posting_creation(reader, company, posting, change.id))
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
                    AND entity_id = ?2 AND id < ?3 ORDER BY id DESC LIMIT 1"
            ),
            params![company, posting.to_string(), before],
            read_audit_row,
        )
        .optional()?
        .map(ReceiptEvent::try_from)
        .transpose()
}
