//! Audited display-label changes scoped to a single existing account.

use std::fmt;

use beankeeper::types::{AccountCode, AccountName};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, grant::OnboardingPostingGrant,
    receipts::desktop_receipt_path, record_with_links, savepoint::AuditedTx, snapshot,
};
use crate::db::{
    Db, accounts,
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::ReceiptId,
    proposals::ProposalId,
};
use crate::error::CliError;

mod proposal;
pub use proposal::{RenameProposal, approve, get, list, reject};

/// A requested label is already used by a different account in this book.
#[derive(Debug)]
pub struct DuplicateAccountName {
    company: CompanySlug,
    code: AccountCode,
    name: String,
}

impl fmt::Display for DuplicateAccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "account name '{}' is already used by account '{}' in company '{}'",
            self.name, self.code, self.company
        )
    }
}

impl std::error::Error for DuplicateAccountName {}

/// The exact label recorded in an audit snapshot. Historical labels may
/// contain whitespace that new input would trim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountLabel {
    name: String,
}

impl AccountLabel {
    /// The label as stored, including any historical whitespace.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum RenameSubmission {
    Renamed {
        code: AccountCode,
        previous_name: String,
        name: AccountName,
        receipt_id: ReceiptId,
        desktop_path: String,
    },
    Unchanged {
        code: AccountCode,
        name: AccountName,
    },
    Proposed {
        code: AccountCode,
        name: AccountName,
        proposal_id: ProposalId,
    },
}

impl fmt::Display for RenameSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Renamed {
                code,
                previous_name,
                name,
                receipt_id,
                desktop_path,
            } => write!(
                formatter,
                "Renamed account {code} from {:?} to {name:?}. Undo receipt #{receipt_id}. Desktop {desktop_path}",
                previous_name,
                name = name.as_ref(),
            ),
            Self::Unchanged { code, name } => write!(
                formatter,
                "Account {code} is already named {:?}.",
                name.as_ref(),
            ),
            Self::Proposed {
                code,
                name,
                proposal_id,
            } => write!(
                formatter,
                "Proposed renaming account {code} to {:?}. Proposal {proposal_id} is ready for sign-off.",
                name.as_ref(),
            ),
        }
    }
}

pub(crate) fn submit(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    code: AccountCode,
    name: AccountName,
) -> Result<RenameSubmission, CliError> {
    OnboardingPostingGrant::with_db(db, actor, |grant| match grant {
        OnboardingPostingGrant::Direct(grant) => grant.mutate("sp_account_rename", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            let account = CurrentAccount::load(&scope, &code)?;
            account.apply(tx, actor, &scope, code, name)
        }),
        OnboardingPostingGrant::Proposal(grant) => {
            grant.mutate("sp_propose_account_rename", |tx, actor| {
                let scope = Company::bind(tx.read(), company.clone())?;
                proposal::reserve(tx, actor, &scope, code, name)
            })
        }
    })
}

struct CurrentAccount {
    label: AccountLabel,
    revision: Option<ReceiptId>,
}

struct ActiveRename {
    id: ReceiptId,
    before: AccountLabel,
    after: AccountLabel,
}

fn active_rename(
    scope: &Company<'_>,
    code: &AccountCode,
) -> Result<Option<ActiveRename>, CliError> {
    let event = scope
        .read()
        .query_row(
            &format!(
                "SELECT id, action, before, after FROM {audit} event
         WHERE company_slug = ?1 AND entity = 'account' AND entity_id = ?2 AND (
             action IN ('create', 'account_delete', 'account_archive')
             OR (action = 'account_rename' AND undo_of IS NULL AND NOT EXISTS (
                 SELECT 1 FROM {audit} restored WHERE restored.undo_of = event.id
             ))
         ) ORDER BY id DESC LIMIT 1",
                audit = scope.read().bookkeeping_audit()
            ),
            params![scope.slug(), code.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, AuditAction>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    match event {
        Some((id, AuditAction::AccountRename, Some(before), Some(after))) => {
            Ok(Some(ActiveRename {
                id,
                before: decode(&before)?,
                after: decode(&after)?,
            }))
        }
        _ => Ok(None),
    }
}

impl CurrentAccount {
    fn load(scope: &Company<'_>, code: &AccountCode) -> Result<Self, CliError> {
        let account = accounts::get_active_account(scope, code.as_str())?;
        let revision = scope
            .read()
            .query_row(
                &format!(
                    "SELECT id FROM {audit}
             WHERE company_slug = ?1 AND entity = 'account' AND entity_id = ?2
             ORDER BY id DESC LIMIT 1",
                    audit = scope.read().bookkeeping_audit()
                ),
                params![scope.slug(), code.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(Self {
            label: AccountLabel { name: account.name },
            revision,
        })
    }

    const fn revision(&self) -> Option<ReceiptId> {
        self.revision
    }

    fn apply(
        self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        scope: &Company<'_>,
        code: AccountCode,
        name: AccountName,
    ) -> Result<RenameSubmission, CliError> {
        let (receipt_id, previous_name) = if self.label.as_str() == name.as_ref() {
            let Some(active) = active_rename(scope, &code)? else {
                return Ok(RenameSubmission::Unchanged { code, name });
            };
            if active.after.as_str() != name.as_ref() {
                return Err(invalid_snapshot(
                    "current account label differs from its rename",
                ));
            }
            (active.id, active.before)
        } else {
            let receipt = store_label(
                tx,
                actor,
                scope,
                &code,
                &self.label,
                LabelChange::Rename(&name),
            )?;
            (receipt, self.label)
        };
        Ok(RenameSubmission::Renamed {
            code,
            previous_name: previous_name.name,
            name,
            receipt_id,
            desktop_path: desktop_receipt_path(scope.slug(), receipt_id),
        })
    }
}

fn ensure_available(scope: &Company<'_>, code: &AccountCode, name: &str) -> Result<(), CliError> {
    let duplicate = scope
        .read()
        .query_row(
            "SELECT code FROM accounts WHERE company_slug = ?1 AND code <> ?2 AND name = ?3
             ORDER BY code LIMIT 1",
            params![scope.slug(), code.as_str(), name],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(duplicate) = duplicate {
        return Err(CliError::DuplicateAccountName(DuplicateAccountName {
            company: scope.slug().clone(),
            code: duplicate.parse().map_err(invalid_snapshot)?,
            name: name.to_owned(),
        }));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum LabelChange<'a> {
    Rename(&'a AccountName),
    Restore(&'a AccountLabel, ReceiptId),
}

fn store_label(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    code: &AccountCode,
    before: &AccountLabel,
    change: LabelChange<'_>,
) -> Result<ReceiptId, CliError> {
    let (after, links) = match change {
        LabelChange::Rename(name) => (name.as_ref(), AuditLinks::None),
        LabelChange::Restore(label, receipt) => (label.as_str(), AuditLinks::Undo(receipt)),
    };
    ensure_available(scope, code, after)?;
    let conn = tx.connection();
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(scope.slug()),
            entity: AuditEntity::Account,
            entity_id: code.as_str(),
            action: AuditAction::AccountRename,
            snapshots: (
                Some(&snapshot(before)),
                Some(&snapshot(&serde_json::json!({ "name": after }))),
            ),
        },
        links,
    )?;
    let receipt = conn.last_insert_rowid().try_into()?;
    conn.execute(
        "UPDATE accounts SET name = ?3 WHERE company_slug = ?1 AND code = ?2",
        params![scope.slug(), code.as_str(), after],
    )?;
    Ok(receipt)
}

pub(super) fn restore(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    code: &AccountCode,
    receipt: ReceiptId,
    (before, after): (&AccountLabel, &AccountLabel),
) -> Result<(), CliError> {
    use super::receipts::{ReceiptUndoError, UndoIneligibility};

    let stale = || ReceiptUndoError::Ineligible {
        receipt,
        reason: UndoIneligibility::AccountNameNotCurrent(code.clone()),
    };
    let current = CurrentAccount::load(scope, code).map_err(|error| match error {
        CliError::NotFound(_) | CliError::Validation(_) => stale().into(),
        other => other,
    })?;
    if current.label.as_str() != after.as_str()
        || active_rename(scope, code)?.is_none_or(|rename| rename.id != receipt)
    {
        return Err(stale().into());
    }
    store_label(
        tx,
        actor,
        scope,
        code,
        after,
        LabelChange::Restore(before, receipt),
    )?;
    Ok(())
}

pub(in crate::db) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mismatch: Option<ReceiptId> = reader
        .query_row(
            &format!(
                "SELECT event.id FROM {audit} event
         LEFT JOIN accounts account ON account.company_slug = event.company_slug
                                  AND account.code = event.entity_id
         WHERE event.action = 'account_rename' AND NOT EXISTS (
             SELECT 1 FROM {audit} newer WHERE newer.company_slug = event.company_slug
               AND newer.entity = 'account' AND newer.entity_id = event.entity_id
               AND newer.action IN ('create', 'account_delete', 'account_rename')
               AND newer.id > event.id
         ) AND (account.code IS NULL OR account.name IS NOT json_extract(event.after, '$.name'))
         LIMIT 1",
                audit = reader.bookkeeping_audit()
            ),
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(receipt) = mismatch {
        return Err(CliError::Database(format!(
            "account label does not match receipt #{receipt}"
        )));
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, CliError> {
    serde_json::from_str(raw).map_err(invalid_snapshot)
}

fn invalid_snapshot(error: impl fmt::Display) -> CliError {
    CliError::Database(format!("invalid account rename snapshot: {error}"))
}
