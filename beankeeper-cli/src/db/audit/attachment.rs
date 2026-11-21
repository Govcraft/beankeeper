//! Attachment insertion and its audit event share one savepoint.

use beankeeper::types::Sha256Hex;
use rusqlite::{Connection, params};

use super::savepoint::{AuditedTx, with_audited_tx};
use super::{Actor, Creation};
use crate::db::attachments::StoreAttachmentParams;
use crate::db::ids::AttachmentId;
use crate::error::CliError;

pub(in crate::db) fn store_attachment(
    conn: &Connection,
    actor: &Actor,
    params: &StoreAttachmentParams<'_>,
) -> Result<AttachmentId, CliError> {
    with_audited_tx(conn, "sp_attachment_create", |tx| {
        insert_attachment(tx, actor, params)
    })
}

fn insert_attachment(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    params: &StoreAttachmentParams<'_>,
) -> Result<AttachmentId, CliError> {
    let hash = params.hash.map(Sha256Hex::as_str);
    let kind = params.document_type.as_str();
    let id = AttachmentId::try_from(tx.insert(
        "INSERT INTO attachments (transaction_id, entry_id, company_slug, uri, document_type, hash, original_filename)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            params.transaction_id,
            params.entry_id,
            params.company_slug,
            params.uri,
            kind,
            hash,
            params.original_filename,
        ],
    )?)?;
    tx.record_insert(
        actor,
        params.company_slug,
        Creation::Attachment,
        &params.transaction_id.to_string(),
        &serde_json::json!({
            "attachment_id": id,
            "entry_id": params.entry_id,
            "uri": params.uri,
            "hash": hash,
            "document_type": kind,
        }),
    )?;
    Ok(id)
}
