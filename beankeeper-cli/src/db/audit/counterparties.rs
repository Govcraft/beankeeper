//! Atomic counterparty changes and their audit snapshots.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links, snapshot,
};
use crate::{
    db::{
        Db,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        counterparties::{
            ContactDetails, ContactPatch, Counterparty, CounterpartyId, apply_contact_change,
            find_counterparty,
        },
        ids::{ReceiptId, TransactionId},
    },
    error::CliError,
    identity::{Admin, Granted},
};

pub(crate) enum PartyMutation {
    Create(ContactDetails),
    Amend(CounterpartyId, ContactPatch),
    Archive(CounterpartyId),
    Associate(CounterpartyId, TransactionId),
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum PartyMutationResult {
    Record {
        party: Counterparty,
        receipt_id: Option<ReceiptId>,
    },
    Association {
        counterparty_id: CounterpartyId,
        transaction_id: TransactionId,
        receipt_id: Option<ReceiptId>,
    },
}

pub(crate) fn mutate(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    mutation: PartyMutation,
) -> Result<PartyMutationResult, CliError> {
    Granted::<Admin>::with_db(db, actor, |grant| {
        grant.mutate("sp_counterparty", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            if let PartyMutation::Associate(id, transaction) = mutation {
                return associate(tx.connection(), actor, &company, id, transaction);
            }
            let (before, party, action) = prepare(&company, mutation)?;
            let receipt_id = if before.as_ref() == Some(&party) {
                None
            } else {
                Some(save(
                    tx.connection(),
                    actor,
                    slug,
                    before.as_ref(),
                    &party,
                    action,
                )?)
            };
            Ok(PartyMutationResult::Record { party, receipt_id })
        })
    })
}

fn prepare(
    company: &Company<'_>,
    mutation: PartyMutation,
) -> Result<(Option<Counterparty>, Counterparty, AuditAction), CliError> {
    match mutation {
        PartyMutation::Create(details) => Ok((
            None,
            Counterparty {
                id: CounterpartyId::generate(),
                details,
                archived: false,
            },
            AuditAction::CounterpartyCreate,
        )),
        PartyMutation::Amend(id, patch) => {
            let before = find_counterparty(company, &id)?;
            let after = apply_contact_change(before.clone(), patch)?;
            Ok((Some(before), after, AuditAction::CounterpartyAmend))
        }
        PartyMutation::Archive(id) => {
            let before = find_counterparty(company, &id)?;
            let mut after = before.clone();
            after.archived = true;
            Ok((Some(before), after, AuditAction::CounterpartyArchive))
        }
        PartyMutation::Associate(..) => Err(CliError::Database(
            "expected a counterparty record mutation".into(),
        )),
    }
}

fn save(
    conn: &rusqlite::Connection,
    actor: &Actor,
    slug: &CompanySlug,
    before: Option<&Counterparty>,
    party: &Counterparty,
    action: AuditAction,
) -> Result<ReceiptId, CliError> {
    let receipt = append_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(slug),
            entity: AuditEntity::Counterparty,
            entity_id: &party.id().to_string(),
            action,
            snapshots: (before.map(snapshot).as_deref(), Some(&snapshot(party))),
        },
        AuditLinks::None,
    )?;
    conn.execute(
        "INSERT INTO counterparties (company_slug, id, name, role, email, address, payment_terms, archived, receipt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(company_slug, id) DO UPDATE SET name = excluded.name, role = excluded.role,
             email = excluded.email, address = excluded.address, payment_terms = excluded.payment_terms,
             archived = excluded.archived, receipt = excluded.receipt",
        params![
            slug, party.id(), party.name().as_str(), party.role().as_str(),
            party.email().map(crate::db::counterparties::ContactEmail::as_str),
            party.address().map(crate::db::counterparties::PostalAddress::as_str),
            party.payment_terms().map(u16::from), party.is_archived(), receipt,
        ],
    )?;
    Ok(receipt)
}

#[derive(Serialize)]
struct AssociationSnapshot<'a> {
    counterparty_id: &'a CounterpartyId,
    transaction_id: TransactionId,
}

fn associate(
    conn: &rusqlite::Connection,
    actor: &Actor,
    company: &Company<'_>,
    id: CounterpartyId,
    transaction: TransactionId,
) -> Result<PartyMutationResult, CliError> {
    let slug = company.slug();
    let party = find_counterparty(company, &id)?;
    let previous = conn
        .query_row(
            "SELECT link.counterparty_id FROM posted_transactions txn
         LEFT JOIN transaction_counterparties link
           ON link.company_slug = txn.company_slug AND link.transaction_id = txn.id
         WHERE txn.company_slug = ?1 AND txn.id = ?2",
            params![slug, transaction],
            |row| row.get::<_, Option<CounterpartyId>>(0),
        )
        .optional()?
        .ok_or_else(|| {
            CliError::NotFound(format!("transaction '{transaction}' not found in '{slug}'"))
        })?;
    let receipt_id = if previous.as_ref() == Some(&id) {
        None
    } else {
        party.require_active()?;
        let receipt = append_with_links(
            conn,
            actor,
            AuditChange {
                company_slug: Some(slug),
                entity: AuditEntity::TransactionCounterparty,
                entity_id: &transaction.to_string(),
                action: AuditAction::CounterpartyAssociate,
                snapshots: (
                    previous
                        .as_ref()
                        .map(|counterparty_id| {
                            snapshot(&AssociationSnapshot {
                                counterparty_id,
                                transaction_id: transaction,
                            })
                        })
                        .as_deref(),
                    Some(&snapshot(&AssociationSnapshot {
                        counterparty_id: &id,
                        transaction_id: transaction,
                    })),
                ),
            },
            AuditLinks::None,
        )?;
        conn.execute(
            "INSERT INTO transaction_counterparties (company_slug, transaction_id, counterparty_id, receipt)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(company_slug, transaction_id)
             DO UPDATE SET counterparty_id = excluded.counterparty_id, receipt = excluded.receipt",
            params![slug, transaction, id, receipt],
        )?;
        Some(receipt)
    };
    Ok(PartyMutationResult::Association {
        counterparty_id: id,
        transaction_id: transaction,
        receipt_id,
    })
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let conn = reader.connection();
    let invalid: Option<String> = conn
        .query_row(
            "WITH current AS (
             SELECT event.id, event.company_slug, event.entity_id, event.after
             FROM bookkeeping_audit event
             JOIN (
                 SELECT MAX(id) AS id FROM bookkeeping_audit WHERE entity = 'counterparty'
                 GROUP BY company_slug, entity_id
             ) latest ON latest.id = event.id
         )
         SELECT CAST(COALESCE(party.id, current.entity_id) AS TEXT)
         FROM counterparties party FULL OUTER JOIN current
           ON current.company_slug = party.company_slug AND current.entity_id = party.id
         WHERE current.id IS NOT party.receipt OR current.id IS NULL OR party.id IS NULL
            OR json_extract(current.after, '$.id') IS NOT party.id
            OR json_extract(current.after, '$.name') IS NOT party.name
            OR json_extract(current.after, '$.role') IS NOT party.role
            OR json_extract(current.after, '$.email') IS NOT party.email
            OR json_extract(current.after, '$.address') IS NOT party.address
            OR json_extract(current.after, '$.payment_terms') IS NOT party.payment_terms
            OR json_extract(current.after, '$.archived') IS NOT party.archived
         LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = invalid {
        return Err(CliError::Database(format!(
            "counterparty '{id}' differs from its current audit snapshot"
        )));
    }
    let invalid: Option<String> = conn.query_row(
        "WITH current AS (
             SELECT event.id, event.company_slug, event.entity_id, event.after
             FROM bookkeeping_audit event
             JOIN (
                 SELECT MAX(id) AS id FROM bookkeeping_audit WHERE entity = 'transaction_counterparty'
                 GROUP BY company_slug, entity_id
             ) latest ON latest.id = event.id
         )
         SELECT COALESCE(CAST(link.transaction_id AS TEXT), current.entity_id)
         FROM transaction_counterparties link FULL OUTER JOIN current
           ON current.company_slug = link.company_slug
              AND current.entity_id = CAST(link.transaction_id AS TEXT)
         WHERE current.id IS NOT link.receipt OR current.id IS NULL OR link.transaction_id IS NULL
            OR json_extract(current.after, '$.counterparty_id') IS NOT link.counterparty_id
            OR json_extract(current.after, '$.transaction_id') IS NOT link.transaction_id
         LIMIT 1", [], |row| row.get(0)
    ).optional()?;
    if let Some(id) = invalid {
        return Err(CliError::Database(format!(
            "transaction '{id}' association differs from its current audit snapshot"
        )));
    }
    Ok(())
}
