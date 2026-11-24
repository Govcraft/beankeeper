//! The one-time translation of legacy links and their audit commitment.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::db::Actor;
use crate::db::audit::{AuditAction, AuditChange, AuditEntity, record};
use crate::db::connection::ReadConn;
use crate::db::ids::{AuditRowId, TransactionId};
use crate::error::CliError;

#[derive(Deserialize)]
struct LegacyPostingLink {
    reversed_by: Option<TransactionId>,
    transaction_id: Option<TransactionId>,
    correlated_with: Option<TransactionId>,
}

impl LegacyPostingLink {
    const fn posting(&self, action: AuditAction) -> Option<TransactionId> {
        match action {
            AuditAction::Reverse => self.reversed_by,
            AuditAction::ProposalApprove => self.transaction_id,
            AuditAction::Correlate => self.correlated_with,
            _ => None,
        }
    }
}

pub(in crate::db) fn backfill_legacy_links(conn: &Connection) -> Result<(), CliError> {
    let mut select = conn.prepare(
        "SELECT id, action, after FROM audit_log
         WHERE action IN ('reverse', 'proposal_approve', 'correlate') ORDER BY id",
    )?;
    let mut rows = select.query([])?;
    let mut update =
        conn.prepare("UPDATE audit_log_rebuild SET related_posting = ?2 WHERE id = ?1")?;
    while let Some(row) = rows.next()? {
        let id: AuditRowId = row.get(0)?;
        let action: AuditAction = row.get(1)?;
        let raw: String = row.get(2)?;
        let snapshot: LegacyPostingLink = serde_json::from_str(&raw).map_err(|error| {
            CliError::Database(format!(
                "cannot migrate the posting link of audit row #{id}: {error}"
            ))
        })?;
        let posting = snapshot.posting(action).ok_or_else(|| {
            CliError::Database(format!("missing posting link in legacy audit row #{id}"))
        })?;
        update.execute(params![id, posting])?;
    }
    Ok(())
}

// Legacy row hashes already commit to the original bytes. A new chain event
// commits to their migrated columns without changing any historical row hash.
fn legacy_digest(reader: ReadConn<'_>) -> Result<Option<String>, CliError> {
    let mut stmt = reader.prepare(
        "SELECT id, undo_of, related_posting FROM audit_log
         WHERE hash_version < 6 AND (undo_of IS NOT NULL OR related_posting IS NOT NULL)
         ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut digest = Sha256::new();
    digest.update(b"beankeeper.audit.legacy_receipt_links.v1");
    let mut has_links = false;
    while let Some(row) = rows.next()? {
        has_links = true;
        let id: AuditRowId = row.get(0)?;
        let undo_of: Option<super::ReceiptId> = row.get(1)?;
        let posting: Option<TransactionId> = row.get(2)?;
        digest.update(id.get().to_be_bytes());
        digest.update(undo_of.map_or(0, super::ReceiptId::get).to_be_bytes());
        digest.update(posting.map_or(0, TransactionId::get).to_be_bytes());
    }
    Ok(has_links.then(|| format!("{:x}", digest.finalize())))
}

pub(in crate::db) fn seal_legacy_links(conn: &Connection) -> Result<(), CliError> {
    if let Some(digest) = legacy_digest(ReadConn::new(conn))? {
        record(
            conn,
            &Actor::new("migration".parse()?),
            AuditChange {
                company_slug: None,
                entity: AuditEntity::Audit,
                entity_id: &digest,
                action: AuditAction::ReceiptLinkMigration,
                snapshots: (None, None),
            },
        )?;
    }
    Ok(())
}

pub(in crate::db::audit) fn verify_legacy_links(reader: ReadConn<'_>) -> Result<(), CliError> {
    let expected = legacy_digest(reader)?;
    let mut stmt = reader.prepare(
        "SELECT entity_id FROM audit_log WHERE action = 'receipt_link_migration' ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let committed: Option<String> = rows.next()?.map(|row| row.get(0)).transpose()?;
    if committed != expected || rows.next()?.is_some() {
        return Err(CliError::Database(
            "migrated receipt links do not match their audit commitment".into(),
        ));
    }
    Ok(())
}

pub(in crate::db::audit) fn verify_links(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mismatch: Option<AuditRowId> = reader
        .query_row(
            "SELECT event.id FROM audit_log event
             LEFT JOIN transactions posting ON posting.id = event.related_posting
             LEFT JOIN audit_log original ON original.id = event.undo_of
             WHERE (event.related_posting IS NOT NULL AND (
                 posting.id IS NULL
                 OR (event.action = 'correlate') = (posting.company_slug IS event.company_slug)
             )) OR (event.undo_of IS NOT NULL AND (
                 original.id IS NULL OR original.id >= event.id
                 OR original.company_slug IS NOT event.company_slug
                 OR (event.action = 'reverse' AND NOT (
                     (original.entity = 'transaction' AND original.action = 'create'
                         AND original.entity_id = event.entity_id)
                     OR (original.entity = 'proposal' AND original.action = 'proposal_approve'
                         AND original.related_posting IS CAST(event.entity_id AS INTEGER))
                 ))
                 OR (event.action = 'setup_revert' AND NOT (
                     event.entity = 'company' AND original.entity = 'company'
                     AND original.action = 'setup_apply'
                     AND event.entity_id = CAST(original.id AS TEXT)
                 ))
                 OR (event.action = 'rule_deactivate' AND NOT (
                     event.entity = 'rule' AND original.entity = 'rule'
                     AND original.action = 'create' AND original.entity_id = event.entity_id
                     AND EXISTS (SELECT 1 FROM categorization_rules rule
                         WHERE rule.company_slug = event.company_slug
                           AND rule.id = event.entity_id)
                 ))
             )) LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = mismatch {
        return Err(CliError::Database(format!(
            "invalid receipt link in audit row #{id}"
        )));
    }
    Ok(())
}
