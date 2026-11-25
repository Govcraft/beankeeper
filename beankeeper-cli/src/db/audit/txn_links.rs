//! Audited relationships between two transactions in one book.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links, record_with_links,
    savepoint::AuditedTx,
};
use crate::db::{
    Actor,
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::{ReceiptId, TransactionId},
};
pub use crate::db::{txn_link_id::TxnLinkId, txn_link_kind::TxnLinkKind};
use crate::error::CliError;
use crate::identity::{Granted, Post};

/// A typed relationship between two distinct transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLink")]
pub struct LinkSpec {
    source_id: TransactionId,
    target_id: TransactionId,
    kind: TxnLinkKind,
}

#[derive(Deserialize)]
struct RawLink {
    source_id: TransactionId,
    target_id: TransactionId,
    kind: TxnLinkKind,
}

impl TryFrom<RawLink> for LinkSpec {
    type Error = CliError;

    fn try_from(raw: RawLink) -> Result<Self, Self::Error> {
        (raw.source_id, raw.target_id, raw.kind).try_into()
    }
}

impl TryFrom<(TransactionId, TransactionId, TxnLinkKind)> for LinkSpec {
    type Error = CliError;

    fn try_from(
        (source_id, target_id, kind): (TransactionId, TransactionId, TxnLinkKind),
    ) -> Result<Self, Self::Error> {
        if source_id == target_id {
            return Err(CliError::Validation(
                "a transaction cannot link to itself".into(),
            ));
        }
        Ok(Self {
            source_id,
            target_id,
            kind,
        })
    }
}

impl LinkSpec {
    pub(crate) const fn endpoints(self) -> [TransactionId; 2] {
        [self.source_id, self.target_id]
    }
}

impl std::fmt::Display for LinkSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} link from transaction #{} to #{}",
            self.kind, self.source_id, self.target_id
        )
    }
}

// Both endpoints were resolved using the write scope that will record the link.
struct CompanyLink<'a> {
    tx: &'a AuditedTx<'a>,
    scope: Company<'a>,
    spec: LinkSpec,
}

impl<'a> TryFrom<(&'a AuditedTx<'a>, CompanySlug, LinkSpec)> for CompanyLink<'a> {
    type Error = CliError;

    fn try_from(
        (tx, company, spec): (&'a AuditedTx<'a>, CompanySlug, LinkSpec),
    ) -> Result<Self, Self::Error> {
        let scope = Company::bind(tx.read(), company)?;
        for id in spec.endpoints() {
            let found = scope.read().query_row(
                "SELECT EXISTS (SELECT 1 FROM transactions WHERE company_slug = ?1 AND id = ?2)",
                params![scope.slug(), id],
                |row| row.get::<_, bool>(0),
            )?;
            if !found {
                return Err(CliError::NotFound(format!(
                    "transaction {id} not found in company '{}'",
                    scope.slug()
                )));
            }
        }
        Ok(Self { tx, scope, spec })
    }
}

/// An active link and the audit receipt that created it.
#[derive(Debug, Clone, Serialize)]
pub struct TxnLink {
    id: TxnLinkId,
    #[serde(flatten)]
    spec: LinkSpec,
    receipt: ReceiptId,
}

impl TxnLink {
    pub(crate) const fn spec(&self) -> LinkSpec {
        self.spec
    }

    pub(crate) const fn receipt(&self) -> ReceiptId {
        self.receipt
    }
}

/// Record a link and its undo receipt in a single write.
///
/// # Errors
/// Returns a named error for an unavailable endpoint, duplicate or cycle, or a storage error.
pub fn create(
    grant: Granted<'_, Post>,
    company: CompanySlug,
    spec: LinkSpec,
) -> Result<TxnLink, CliError> {
    grant.mutate("sp_txn_link", |tx, actor| {
        CompanyLink::try_from((tx, company, spec))?.record(actor)
    })
}

impl CompanyLink<'_> {
    fn record(self, actor: &Actor) -> Result<TxnLink, CliError> {
        let company = self.scope.slug();
        let conn = self.tx.connection();
        let id = TxnLinkId::generate();
        let key = id.to_string();
        let after = super::snapshot(&self.spec);
        let receipt = append_with_links(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::TransactionLink,
                entity_id: &key,
                action: AuditAction::LinkCreate,
                snapshots: (None, Some(&after)),
            },
            AuditLinks::None,
        )?;
        conn.execute(
            "INSERT INTO transaction_links (id, company_slug, source_id, target_id, link_kind, receipt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key, company, self.spec.source_id, self.spec.target_id, self.spec.kind.as_str(), receipt],
        )
        .map_err(|error| match &error {
            rusqlite::Error::SqliteFailure(_, Some(message))
                if matches!(message.as_str(), "transaction link already exists" | "transaction link would create a cycle") =>
            {
                CliError::Validation(message.clone())
            }
            _ => error.into(),
        })?;
        Ok(TxnLink {
            id,
            spec: self.spec,
            receipt,
        })
    }
}

pub(super) fn undo(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    receipt: ReceiptId,
    id: &TxnLinkId,
) -> Result<(), CliError> {
    // Copy the original snapshot bytes so the reversal is tied to that exact audit record.
    let key = id.to_string();
    let before: String = tx.read().query_row(
        "SELECT after FROM audit_log WHERE id = ?1 AND company_slug = ?2",
        params![receipt, company],
        |row| row.get(0),
    )?;
    record_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::TransactionLink,
            entity_id: &key,
            action: AuditAction::LinkUndo,
            snapshots: (Some(&before), None),
        },
        AuditLinks::Undo(receipt),
    )?;
    Ok(())
}

pub(crate) fn for_transaction(
    scope: &Company<'_>,
    id: TransactionId,
) -> Result<Vec<TxnLink>, CliError> {
    let mut statement = scope.read().prepare_cached(
        "SELECT id, source_id, target_id, link_kind, receipt FROM active_transaction_links
         WHERE company_slug = ?1 AND (source_id = ?2 OR target_id = ?2)
         ORDER BY receipt",
    )?;
    let mut rows = statement.query(params![scope.slug(), id])?;
    let mut links = Vec::new();
    while let Some(row) = rows.next()? {
        links.push(decode(row)?);
    }
    Ok(links)
}

fn decode(row: &Row<'_>) -> Result<TxnLink, CliError> {
    Ok(TxnLink {
        id: row.get(0)?,
        spec: (
            row.get::<_, TransactionId>(1)?,
            row.get::<_, TransactionId>(2)?,
            row.get::<_, TxnLinkKind>(3)?,
        )
            .try_into()?,
        receipt: row.get(4)?,
    })
}

pub(in crate::db) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mismatch: Option<ReceiptId> = reader
        .query_row(
            "SELECT receipt FROM transaction_links
             WHERE NOT EXISTS (SELECT 1 FROM transaction_link_creations creation
                 WHERE creation.receipt = transaction_links.receipt
                   AND creation.id = transaction_links.id
                   AND creation.company_slug = transaction_links.company_slug
                   AND creation.source_id IS transaction_links.source_id
                   AND creation.target_id IS transaction_links.target_id
                   AND creation.link_kind IS transaction_links.link_kind)
             UNION ALL
             SELECT receipt FROM transaction_link_creations creation
             WHERE NOT EXISTS (SELECT 1 FROM transaction_links link WHERE link.receipt = creation.receipt)
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(receipt) = mismatch {
        return Err(CliError::Database(format!(
            "transaction link does not match creation receipt #{receipt}"
        )));
    }
    Ok(())
}
