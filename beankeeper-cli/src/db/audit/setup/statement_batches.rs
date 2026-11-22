//! Frozen statement previews and receipts for owner-approved imports.

use beankeeper::types::{DebitOrCredit, Sha256Hex};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::super::{AuditAction, AuditChange, AuditEntity, atomically, record};
use super::requests::SetupReference;
use super::statements::{StatementFormat, StatementPreview, StatementRequest, prepare_in_session};
use super::{ActiveSession, Generation, SessionState, find_session, with_active_session};
use crate::db::companies::CompanySlug;
use crate::db::connection::{Db, ReadConn};
use crate::db::ids::{StatementBatchId, TransactionId};
use crate::db::transactions::{
    self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams,
};
use crate::db::{Actor, metadata};
use crate::error::CliError;
use crate::identity::{Granted, StatementApprover};
use crate::output::amounts::format_money;

const PREVIEW_EVENT: &str = "setup.statement.preview.";
const COMMIT_EVENT: &str = "setup.statement.commit.";

#[derive(Serialize, Deserialize)]
struct FrozenBatch {
    version: u8,
    company: String,
    reference: String,
    generation: Generation,
    profile_revision: i64,
    format: StatementFormat,
    #[serde(with = "hash_text")]
    source_sha256: Sha256Hex,
    bank: String,
    suspense: String,
    currency: String,
    rows: Vec<FrozenRow>,
}

#[derive(Serialize, Deserialize)]
struct FrozenRow {
    reference: String,
    date: String,
    description: String,
    metadata: Option<String>,
    entries: Vec<FrozenEntry>,
    existing_transaction: Option<TransactionId>,
}

#[derive(Serialize, Deserialize)]
struct FrozenEntry {
    account_code: String,
    account_name: String,
    #[serde(with = "DirectionCode")]
    direction: DebitOrCredit,
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_minor"
    )]
    amount_minor: i64,
    amount_display: String,
    memo: Option<String>,
    tax_category: Option<String>,
}

impl FrozenEntry {
    fn posting_params(&self) -> PostEntryParams {
        PostEntryParams {
            account_code: self.account_code.clone(),
            direction: self.direction.code().into(),
            amount: self.amount_minor,
            memo: self.memo.clone(),
            tax_category: self.tax_category.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "DebitOrCredit", rename_all = "lowercase")]
enum DirectionCode {
    Debit,
    Credit,
}

fn deserialize_minor<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<i64, D::Error> {
    String::deserialize(decoder)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[derive(Serialize)]
pub(crate) struct StatementReview {
    batch_id: StatementBatchId,
    #[serde(with = "hash_text")]
    preview_sha256: Sha256Hex,
    #[serde(flatten)]
    batch: FrozenBatch,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreviewStamp {
    #[serde(with = "hash_text")]
    request_sha256: Sha256Hex,
    #[serde(with = "hash_text")]
    preview_sha256: Sha256Hex,
}

struct StoredPreview {
    review: StatementReview,
    stamp: PreviewStamp,
    setup_jti: String,
    setup_uri: String,
}

/// One reviewed statement reference and the immutable ledger transaction it identifies.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementTransaction {
    /// Stable reference used for transaction deduplication.
    pub reference: String,
    /// ID accepted by the standard transaction and reversal commands.
    pub transaction_id: TransactionId,
}

/// Provenance and idempotent outcome of a committed statement batch.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementReceipt {
    /// Identifier of the frozen preview that was approved.
    pub batch_id: StatementBatchId,
    /// Company slug to use with the returned transaction IDs.
    pub company: String,
    /// Stable request reference scoped to the setup generation.
    pub reference: String,
    /// CSV or OFX/QFX statement format.
    pub format: StatementFormat,
    /// SHA-256 digest of the original statement contents.
    #[serde(with = "hash_text")]
    pub source_sha256: Sha256Hex,
    /// SHA-256 digest of the financial preview approved by the owner.
    #[serde(with = "hash_text")]
    pub preview_sha256: Sha256Hex,
    /// Verified approver URI.
    pub approved_by_uri: String,
    /// Token ID used for the approval.
    pub approved_by_jti: String,
    /// Transactions created by this batch, in statement order.
    pub created: Vec<StatementTransaction>,
    /// Exact matching transactions that already existed before this batch.
    pub preexisting: Vec<StatementTransaction>,
}

/// Historical statement import counts for one book.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatementImportSummary {
    /// Statement batches with recorded approvals, including all-preexisting batches.
    pub committed_batches: u64,
    /// Transactions created by those statement imports.
    pub created_transactions: u64,
    /// Source rows that matched a transaction already on the ledger.
    pub preexisting_matches: u64,
}

pub(crate) fn summary(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<StatementImportSummary, CliError> {
    use crate::db::schema::{STATEMENT_BATCH_SCHEMA_VERSION, read_schema_version};

    if read_schema_version(reader)? < STATEMENT_BATCH_SCHEMA_VERSION {
        return Ok(StatementImportSummary::default());
    }
    let (summary, valid) = reader.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(json_array_length(a.after, '$.created')), 0),
                COALESCE(SUM(json_array_length(a.after, '$.preexisting')), 0),
                COUNT(*) = COUNT(DISTINCT b.id) AND COUNT(*) = COUNT(CASE
                    WHEN json_type(a.after, '$.created') = 'array'
                     AND json_type(a.after, '$.preexisting') = 'array' THEN 1 END)
         FROM setup_statement_batches b
         JOIN audit_log a ON a.company_slug = b.company_slug AND a.entity_id = (?2 || b.id)
         WHERE b.company_slug = ?1 AND a.entity = 'company' AND a.action = 'create'",
        params![company.as_str(), COMMIT_EVENT],
        |row| {
            let count = |index| {
                let value = row.get::<_, i64>(index)?;
                u64::try_from(value)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
            };
            Ok((
                StatementImportSummary {
                    committed_batches: count(0)?,
                    created_transactions: count(1)?,
                    preexisting_matches: count(2)?,
                },
                row.get::<_, bool>(3)?,
            ))
        },
    )?;
    if !valid {
        return Err(CliError::Database(format!(
            "invalid statement import receipts for company '{company}'"
        )));
    }
    Ok(summary)
}

mod hash_text {
    use beankeeper::types::Sha256Hex;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(
        value: &Sha256Hex,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.as_str())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        decoder: D,
    ) -> Result<Sha256Hex, D::Error> {
        Sha256Hex::new(String::deserialize(decoder)?).map_err(D::Error::custom)
    }
}

fn digest(value: impl AsRef<[u8]>) -> Sha256Hex {
    Sha256Hex::from_digest(Sha256::digest(value).into())
}

fn encode(value: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(value)
        .map_err(|error| CliError::General(format!("cannot encode statement batch: {error}")))
}

fn decode<T: DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_str(value)
        .map_err(|error| CliError::Database(format!("invalid stored statement batch: {error}")))
}

fn corrupted(batch_id: StatementBatchId) -> CliError {
    CliError::Database(format!(
        "statement batch #{batch_id} does not match its audit history"
    ))
}

struct AuditSnapshot<T> {
    payload: T,
    agent_uri: Option<String>,
    token_jti: Option<String>,
}

fn audit_snapshot<T: DeserializeOwned>(
    reader: ReadConn<'_>,
    company: &str,
    batch_id: StatementBatchId,
    event: &str,
) -> Result<Option<AuditSnapshot<T>>, CliError> {
    let id = format!("{event}{batch_id}");
    let mut statement = reader.prepare(
        "SELECT after, agent_uri, token_jti FROM audit_log
         WHERE company_slug = ?1 AND entity = 'company' AND entity_id = ?2 AND action = 'create'
         LIMIT 2",
    )?;
    let mut rows = statement.query(params![company, id])?;
    let result = rows
        .next()?
        .map(|row| -> Result<_, CliError> {
            Ok(AuditSnapshot {
                payload: decode(&row.get::<_, String>(0)?)?,
                agent_uri: row.get(1)?,
                token_jti: row.get(2)?,
            })
        })
        .transpose()?;
    if rows.next()?.is_some() {
        return Err(corrupted(batch_id));
    }
    Ok(result)
}

fn load(
    reader: ReadConn<'_>,
    company: &str,
    batch_id: StatementBatchId,
) -> Result<StoredPreview, CliError> {
    let row = reader
        .query_row(
            "SELECT generation, setup_jti, reference, request_hash, preview_hash, payload
             FROM setup_statement_batches WHERE company_slug = ?1 AND id = ?2",
            params![company, batch_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let (generation, setup_jti, reference, request_hash, preview_hash, body) =
        row.ok_or_else(|| {
            CliError::NotFound(format!(
                "statement batch #{batch_id} not found in book '{company}'"
            ))
        })?;
    let batch: FrozenBatch = decode(&body)?;
    let stamp = PreviewStamp {
        request_sha256: Sha256Hex::new(request_hash).map_err(|_| corrupted(batch_id))?,
        preview_sha256: Sha256Hex::new(preview_hash).map_err(|_| corrupted(batch_id))?,
    };
    let audit = audit_snapshot::<PreviewStamp>(reader, company, batch_id, PREVIEW_EVENT)?
        .filter(|audit| {
            audit.payload == stamp && audit.token_jti.as_deref() == Some(setup_jti.as_str())
        })
        .ok_or_else(|| corrupted(batch_id))?;
    let setup_uri = audit.agent_uri.ok_or_else(|| corrupted(batch_id))?;
    if batch.version != 1
        || batch.company != company
        || batch.generation.get() != generation
        || batch.reference != reference
        || digest(&body) != stamp.preview_sha256
    {
        return Err(corrupted(batch_id));
    }
    Ok(StoredPreview {
        review: StatementReview {
            batch_id,
            preview_sha256: stamp.preview_sha256.clone(),
            batch,
        },
        stamp,
        setup_jti,
        setup_uri,
    })
}

fn record_event(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    batch_id: StatementBatchId,
    event: &str,
    snapshot: &impl Serialize,
) -> Result<(), CliError> {
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::Company,
            entity_id: &format!("{event}{batch_id}"),
            action: AuditAction::Create,
            snapshots: (None, Some(&encode(snapshot)?)),
        },
    )
}

fn reserve(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    reference: &str,
    request_hash: &Sha256Hex,
    prepare: impl FnOnce() -> Result<FrozenBatch, CliError>,
) -> Result<StatementReview, CliError> {
    let conn = session.connection;
    let reader = ReadConn::new(conn);
    let generation = session.session.generation;
    let previous = reader
        .query_row(
            "SELECT id FROM setup_statement_batches
             WHERE company_slug = ?1 AND generation = ?2 AND reference = ?3",
            params![company.as_str(), generation.get(), reference],
            |row| row.get::<_, StatementBatchId>(0),
        )
        .optional()?;
    if let Some(id) = previous {
        let stored = load(reader, company.as_str(), id)?;
        if &stored.stamp.request_sha256 != request_hash {
            return Err(CliError::Validation(format!(
                "statement reference '{reference}' belongs to a different request"
            )));
        }
        return Ok(stored.review);
    }

    let batch = prepare()?;
    let body = encode(&batch)?;
    let stamp = PreviewStamp {
        request_sha256: request_hash.clone(),
        preview_sha256: digest(&body),
    };
    let id: StatementBatchId = conn.query_row(
        "INSERT INTO setup_statement_batches
            (company_slug, generation, reference, setup_jti, request_hash, preview_hash, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
        params![
            company.as_str(),
            generation.get(),
            reference,
            session.session.token_jti.as_str(),
            stamp.request_sha256.as_str(),
            stamp.preview_sha256.as_str(),
            body,
        ],
        |row| row.get(0),
    )?;
    record_event(
        conn,
        session.actor(),
        company.as_str(),
        id,
        PREVIEW_EVENT,
        &stamp,
    )?;
    Ok(StatementReview {
        batch_id: id,
        preview_sha256: stamp.preview_sha256,
        batch,
    })
}

pub(crate) fn preview(
    db: &Db,
    actor: &Actor,
    reference: &SetupReference,
    request: &StatementRequest,
) -> Result<StatementReview, CliError> {
    with_active_session(db, actor, request.generation, |session| {
        reserve(
            session,
            &request.company,
            &reference.to_string(),
            &request.fingerprint()?,
            || freeze(session, reference, prepare_in_session(session, request)?),
        )
    })
}

fn freeze(
    session: &ActiveSession<'_>,
    reference: &SetupReference,
    preview: StatementPreview,
) -> Result<FrozenBatch, CliError> {
    let mut batch = FrozenBatch {
        version: 1,
        company: preview.company.to_string(),
        reference: reference.to_string(),
        generation: preview.generation,
        profile_revision: preview.profile_revision.get(),
        format: preview.format,
        source_sha256: preview.content_hash,
        bank: preview.bank.to_string(),
        suspense: preview.suspense.to_string(),
        currency: preview.currency.code().into(),
        rows: Vec::with_capacity(preview.rows.len()),
    };
    for row in preview.rows {
        let entries = row
            .journal
            .entries()
            .iter()
            .map(|entry| -> Result<_, CliError> {
                let amount = entry.amount();
                Ok(FrozenEntry {
                    account_code: entry.account().code().to_string(),
                    account_name: entry.account().name().to_owned(),
                    direction: entry.direction(),
                    amount_minor: i64::try_from(amount.amount().minor_units()).map_err(|_| {
                        CliError::Validation("statement amount exceeds storage".into())
                    })?,
                    amount_display: format_money(amount),
                    memo: entry.memo().map(str::to_owned),
                    tax_category: entry.tax_category().map(str::to_owned),
                })
            })
            .collect::<Result<_, _>>()?;
        let row = FrozenRow {
            reference: row.reference.into_string(),
            date: row.journal.date().to_string(),
            description: row.journal.description().to_owned(),
            metadata: row.journal.metadata().map(str::to_owned),
            entries,
            existing_transaction: row.existing_transaction,
        };
        if let Some(id) = row.existing_transaction {
            require_existing_match(session.connection, &batch, &row, id)?;
        }
        batch.rows.push(row);
    }
    Ok(batch)
}

pub(crate) fn commit(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    batch_id: StatementBatchId,
    expected: &Sha256Hex,
) -> Result<StatementReceipt, CliError> {
    atomically(db.conn(), || {
        let grant = Granted::<StatementApprover>::check(db.conn(), actor)?;
        let conn = grant.connection();
        let reader = ReadConn::new(conn);
        let preview = load(reader, company.as_str(), batch_id)?;
        if &preview.stamp.preview_sha256 != expected {
            return Err(CliError::Validation(format!(
                "approval digest does not match statement batch #{batch_id}"
            )));
        }
        if let Some(receipt) = receipt_for(reader, &preview)? {
            return Ok(receipt);
        }
        authorize_first_commit(conn, &grant, &preview)?;
        let receipt = apply_batch(conn, grant.actor(), &preview)?;
        record_event(
            conn,
            grant.actor(),
            company.as_str(),
            batch_id,
            COMMIT_EVENT,
            &receipt,
        )?;
        Ok(receipt)
    })
}

/// Read the typed outcome of a statement import. Existing matches are
/// reported separately and remain outside this batch's imported history.
pub(crate) fn get_receipt(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    batch_id: StatementBatchId,
) -> Result<StatementReceipt, CliError> {
    let preview = load(reader, company.as_str(), batch_id)?;
    receipt_for(reader, &preview)?.ok_or_else(|| {
        CliError::NotFound(format!(
            "statement batch #{batch_id} has not been committed"
        ))
    })
}

fn receipt_for(
    reader: ReadConn<'_>,
    preview: &StoredPreview,
) -> Result<Option<StatementReceipt>, CliError> {
    let id = preview.review.batch_id;
    let batch = &preview.review.batch;
    let Some(audit) = audit_snapshot::<StatementReceipt>(reader, &batch.company, id, COMMIT_EVENT)?
    else {
        return Ok(None);
    };
    let receipt = audit.payload;
    if receipt.batch_id != id
        || receipt.company != batch.company
        || receipt.reference != batch.reference
        || receipt.format != batch.format
        || receipt.source_sha256 != batch.source_sha256
        || receipt.preview_sha256 != preview.stamp.preview_sha256
        || audit.agent_uri.as_deref() != Some(receipt.approved_by_uri.as_str())
        || audit.token_jti.as_deref() != Some(receipt.approved_by_jti.as_str())
        || receipt.approved_by_jti == preview.setup_jti
        || receipt.approved_by_uri == preview.setup_uri
    {
        return Err(corrupted(id));
    }
    let mut created = receipt.created.iter();
    let mut preexisting = receipt.preexisting.iter();
    for row in &batch.rows {
        let item = match row.existing_transaction {
            Some(id) => preexisting.next().filter(|item| item.transaction_id == id),
            None => created.next(),
        }
        .filter(|item| item.reference == row.reference)
        .ok_or_else(|| corrupted(id))?;
        let present = reader.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM transactions
                 WHERE id = ?1 AND company_slug = ?2 AND reference = ?3
                   AND (?4 OR (agent_uri = ?5 AND token_jti = ?6))
             )",
            params![
                item.transaction_id,
                receipt.company,
                item.reference,
                row.existing_transaction.is_some(),
                receipt.approved_by_uri,
                receipt.approved_by_jti,
            ],
            |row| row.get::<_, bool>(0),
        )?;
        if !present {
            return Err(corrupted(id));
        }
    }
    if created.next().is_some() || preexisting.next().is_some() {
        return Err(corrupted(id));
    }
    Ok(Some(receipt))
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mut statement = reader.prepare(
        "SELECT company_slug, id FROM setup_statement_batches
         UNION
         SELECT company_slug, CAST(SUBSTR(entity_id, LENGTH(?1) + 1) AS INTEGER)
         FROM audit_log WHERE entity = 'company' AND action = 'create' AND entity_id GLOB ?1 || '*'
         UNION
         SELECT company_slug, CAST(SUBSTR(entity_id, LENGTH(?2) + 1) AS INTEGER)
         FROM audit_log WHERE entity = 'company' AND action = 'create' AND entity_id GLOB ?2 || '*'",
    )?;
    let rows = statement.query_map([PREVIEW_EVENT, COMMIT_EVENT], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, StatementBatchId>(1)?))
    })?;
    for row in rows {
        let (company, id) = row?;
        let preview = load(reader, &company, id).map_err(|error| match error {
            CliError::NotFound(_) => corrupted(id),
            other => other,
        })?;
        receipt_for(reader, &preview)?;
    }
    Ok(())
}

fn authorize_first_commit(
    conn: &Connection,
    grant: &Granted<'_, StatementApprover>,
    preview: &StoredPreview,
) -> Result<(), CliError> {
    let batch = &preview.review.batch;
    let identity = grant.actor().identity().ok_or_else(|| {
        CliError::Unauthorized("statement commit requires a live approver".into())
    })?;
    let session = find_session(conn, batch.generation)?.ok_or_else(|| {
        CliError::Forbidden("statement setup generation is no longer active".into())
    })?;
    let revoked = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_revocations WHERE jti = ?1)",
        [&preview.setup_jti],
        |row| row.get::<_, bool>(0),
    )?;
    if session.state != SessionState::Active
        || session.expires_at <= Utc::now()
        || session.token_jti.as_str() != preview.setup_jti
        || revoked
        || session.agent_uri != preview.setup_uri
        || identity.jti() == preview.setup_jti
        || identity.uri() == preview.setup_uri
    {
        return Err(CliError::Forbidden(
            "statement needs a live setup generation and a separate approver".into(),
        ));
    }
    let current = conn
        .query_row(
            "SELECT revision FROM setup_book_profiles WHERE company_slug = ?1
             ORDER BY revision DESC LIMIT 1",
            [&batch.company],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if current != Some(batch.profile_revision) {
        return Err(CliError::Validation(
            "statement book profile changed after preview".into(),
        ));
    }
    Ok(())
}

fn apply_batch(
    conn: &Connection,
    actor: &Actor,
    preview: &StoredPreview,
) -> Result<StatementReceipt, CliError> {
    let batch = &preview.review.batch;
    let identity = actor.identity().ok_or_else(|| {
        CliError::Unauthorized("statement commit requires a live approver".into())
    })?;
    let mut receipt = StatementReceipt {
        batch_id: preview.review.batch_id,
        company: batch.company.clone(),
        reference: batch.reference.clone(),
        format: batch.format,
        source_sha256: batch.source_sha256.clone(),
        preview_sha256: preview.stamp.preview_sha256.clone(),
        approved_by_uri: identity.uri().to_owned(),
        approved_by_jti: identity.jti().to_owned(),
        created: Vec::new(),
        preexisting: Vec::new(),
    };
    for row in &batch.rows {
        let (transaction_id, items) = if let Some(id) = row.existing_transaction {
            require_existing_match(conn, batch, row, id)?;
            (id, &mut receipt.preexisting)
        } else {
            let entry_params: Vec<_> = row
                .entries
                .iter()
                .map(FrozenEntry::posting_params)
                .collect();
            let result = transactions::post_approved_statement(ApprovedStatementPosting {
                conn,
                actor,
                batch,
                row,
                entries: &entry_params,
            })?;
            let PostResult::Created(id) = result else {
                return Err(reference_conflict(&row.reference));
            };
            (TransactionId::try_from(id)?, &mut receipt.created)
        };
        items.push(StatementTransaction {
            reference: row.reference.clone(),
            transaction_id,
        });
    }
    Ok(receipt)
}

fn reference_conflict(reference: &str) -> CliError {
    CliError::Validation(format!(
        "statement reference '{reference}' no longer matches the preview"
    ))
}

fn require_existing_match(
    conn: &Connection,
    batch: &FrozenBatch,
    row: &FrozenRow,
    id: TransactionId,
) -> Result<(), CliError> {
    let (transaction, entries) = transactions::get_transaction(conn, &batch.company, id.get())?;
    let metadata_matches = match row.metadata.as_deref().and_then(metadata::normalize) {
        None => true,
        Some(expected) => {
            let actual = transaction
                .metadata
                .as_deref()
                .and_then(metadata::normalize);
            expected.into_value().as_object().is_some_and(|expected| {
                actual.is_some_and(|actual| {
                    let actual = actual.into_value();
                    expected
                        .iter()
                        .all(|(key, value)| actual.get(key) == Some(value))
                })
            })
        }
    };
    let entries_match = entries.len() == row.entries.len()
        && entries.iter().zip(&row.entries).all(|(actual, expected)| {
            actual.account_code == expected.account_code
                && actual.direction == expected.direction
                && actual.amount == expected.amount_minor
                && actual.memo == expected.memo
                && actual.tax_category == expected.tax_category
        });
    if transaction.reference.as_deref() != Some(row.reference.as_str())
        || transaction.date != row.date
        || transaction.description != row.description
        || transaction.currency != batch.currency
        || !metadata_matches
        || !entries_match
    {
        return Err(reference_conflict(&row.reference));
    }
    Ok(())
}

pub(in crate::db) struct ApprovedStatementPosting<'a> {
    conn: &'a Connection,
    actor: &'a Actor,
    batch: &'a FrozenBatch,
    row: &'a FrozenRow,
    entries: &'a [PostEntryParams],
}

impl<'a> ApprovedStatementPosting<'a> {
    pub(in crate::db) fn connection(&self) -> &'a Connection {
        self.conn
    }

    pub(in crate::db) fn params(&self) -> PostTransactionParams<'_> {
        PostTransactionParams {
            company_slug: &self.batch.company,
            description: &self.row.description,
            metadata: self.row.metadata.as_deref(),
            currency: &self.batch.currency,
            date: &self.row.date,
            entries: self.entries,
            correlate: None,
            reference: Some(&self.row.reference),
            on_conflict: ConflictStrategy::Error,
            actor: self.actor,
        }
    }
}
