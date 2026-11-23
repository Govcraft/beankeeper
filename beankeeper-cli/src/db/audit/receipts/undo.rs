use std::fmt;

use beankeeper::types::AccountCode;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use super::{ReceiptEvent, ReceiptId, ReceiptTarget, posting_creation};
use crate::db::audit::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, ReversalBlock, ReverseTransactionParams,
    audit_columns, read_audit_row, record_with_links, reverse_in_scope, savepoint::AuditedTx,
};
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::ReadConn;
use crate::db::ids::TransactionId;
use crate::db::periods::ClosedPostingPeriod;
use crate::db::transactions::PostResult;
use crate::db::{Actor, accounts};
use crate::error::CliError;
use crate::identity::{Granted, OwnerUndo};
use crate::request::LocalDate;

/// A receipt exists in this company but cannot be undone.
#[derive(Debug)]
pub enum ReceiptUndoError {
    /// This kind of change has no supported undo.
    Unsupported(ReceiptId),
    /// The affected ledger state no longer allows a safe undo.
    Ineligible {
        receipt: ReceiptId,
        reason: UndoIneligibility,
    },
}

impl fmt::Display for ReceiptUndoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(id) => write!(formatter, "receipt #{id} does not support undo"),
            Self::Ineligible { receipt, reason } => {
                write!(formatter, "receipt #{receipt} cannot be undone: {reason}")
            }
        }
    }
}

impl std::error::Error for ReceiptUndoError {}

impl From<ReceiptUndoError> for CliError {
    fn from(error: ReceiptUndoError) -> Self {
        Self::ReceiptUndo(error)
    }
}

/// A current ledger condition that prevents immediate receipt undo.
#[derive(Debug)]
pub enum UndoIneligibility {
    /// The mirror would land in a closed posting period.
    PeriodLocked(ClosedPostingPeriod),
    /// The posting itself cannot be reversed.
    ReversalBlocked(ReversalBlock),
    /// A newer audit event created an account with this code.
    NewerAccountCreation(AccountCode),
    /// There is no active account with this code.
    AccountNotActive(AccountCode),
}

impl fmt::Display for UndoIneligibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PeriodLocked(error) => error.fmt(formatter),
            Self::ReversalBlocked(error) => error.fmt(formatter),
            Self::NewerAccountCreation(code) => {
                write!(formatter, "account '{code}' has a newer creation")
            }
            Self::AccountNotActive(code) => {
                write!(formatter, "account '{code}' is no longer active")
            }
        }
    }
}

/// A mirror cannot be dated earlier than the posting it reverses.
pub(in crate::db::audit) struct UndoPostingDate(chrono::NaiveDate);

impl From<(chrono::NaiveDate, chrono::NaiveDate)> for UndoPostingDate {
    fn from((requested, original): (chrono::NaiveDate, chrono::NaiveDate)) -> Self {
        Self(requested.max(original))
    }
}

impl From<UndoPostingDate> for chrono::NaiveDate {
    fn from(date: UndoPostingDate) -> Self {
        date.0
    }
}

/// The audited result of owner undo. Retries return the same result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoOutcome {
    /// A mirror posting reverses the original transaction.
    Reversed {
        transaction: TransactionId,
        reversal: TransactionId,
        undo_receipt: ReceiptId,
    },
    /// The account remains in the ledger and rejects ordinary new postings.
    Archived {
        account: AccountCode,
        undo_receipt: ReceiptId,
    },
}

enum UndoTarget {
    Posting(TransactionId),
    Account(AccountCode),
}

/// Undo a receipt immediately within the live, reserved owner write scope.
///
/// A signed administration grant is required even when ledger enforcement is
/// disabled. The receipt and its current eligibility are resolved in that
/// same scope. An already completed owner undo returns the recorded outcome.
///
/// # Errors
/// Returns not-found for a receipt outside this company, a named undo error
/// for an unsupported or ineligible change, or an authorization/storage error.
pub fn undo_receipt(
    grant: Granted<'_, OwnerUndo>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<UndoOutcome, CliError> {
    grant.mutate("sp_owner_undo_receipt", |tx, actor| {
        let scope = Company::bind(tx.read(), company.clone())?;
        let target = resolve_target(&scope, receipt)?;
        if let Some(outcome) = recorded_outcome(tx.read(), company, receipt, &target)? {
            return Ok(outcome);
        }

        match &target {
            UndoTarget::Posting(id) => reverse_posting(tx, actor, company, receipt, *id)?,
            UndoTarget::Account(code) => archive_account(tx, actor, &scope, receipt, code)?,
        }
        recorded_outcome(tx.read(), company, receipt, &target)?.ok_or_else(|| {
            CliError::Database(format!("undo of receipt #{receipt} was not recorded"))
        })
    })
}

fn resolve_target(scope: &Company<'_>, receipt: ReceiptId) -> Result<UndoTarget, CliError> {
    let columns = audit_columns(crate::db::schema::CURRENT_VERSION);
    let reader = scope.read();
    let event = reader
        .query_row(
            &format!("SELECT {columns} FROM audit_log WHERE company_slug = ?1 AND id = ?2"),
            params![scope.slug(), receipt],
            read_audit_row,
        )
        .optional()?
        .map(ReceiptEvent::try_from)
        .transpose()?
        .ok_or_else(|| {
            CliError::NotFound(format!(
                "receipt #{receipt} not found in company '{}'",
                scope.slug()
            ))
        })?;
    match (event.target(), event.action()) {
        (ReceiptTarget::Account(code), AuditAction::Create) => {
            Ok(UndoTarget::Account(code.clone()))
        }
        (ReceiptTarget::Proposal(_), AuditAction::ProposalApprove) => {
            if let Some(id) = event.related_posting()
                && posting_creation(reader, scope.slug(), id, event.id())?.is_some()
            {
                return Ok(UndoTarget::Posting(id));
            }
            Err(ReceiptUndoError::Unsupported(receipt).into())
        }
        (ReceiptTarget::Transaction(id), AuditAction::Create) => {
            let grouped = reader.query_row(
                "SELECT EXISTS(SELECT 1 FROM audit_receipt_postings
                 WHERE company_slug = ?1 AND posting_id = ?2 AND id > ?3)",
                params![scope.slug(), id, receipt],
                |row| row.get::<_, bool>(0),
            )?;
            if grouped {
                return Err(ReceiptUndoError::Unsupported(receipt).into());
            }
            Ok(UndoTarget::Posting(*id))
        }
        _ => Err(ReceiptUndoError::Unsupported(receipt).into()),
    }
}

fn invalid_stored_target(error: impl fmt::Display) -> CliError {
    CliError::Database(format!("invalid target in audit receipt: {error}"))
}

fn recorded_outcome(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
    target: &UndoTarget,
) -> Result<Option<UndoOutcome>, CliError> {
    let columns = audit_columns(crate::db::schema::CURRENT_VERSION);
    let event = reader
        .query_row(
            &format!(
                "SELECT {columns} FROM audit_log
         WHERE company_slug = ?1 AND undo_of = ?2"
            ),
            params![company, receipt],
            read_audit_row,
        )
        .optional()?
        .map(ReceiptEvent::try_from)
        .transpose()?;
    let Some(event) = event else { return Ok(None) };
    let undo_receipt = ReceiptId::from(event.id());
    match (target, event.target(), event.action()) {
        (
            UndoTarget::Posting(transaction),
            ReceiptTarget::Transaction(recorded),
            AuditAction::Reverse,
        ) if recorded == transaction => {
            let reversal = event
                .related_posting()
                .ok_or_else(|| invalid_stored_target("missing reversal"))?;
            Ok(Some(UndoOutcome::Reversed {
                transaction: *transaction,
                reversal,
                undo_receipt,
            }))
        }
        (
            UndoTarget::Account(account),
            ReceiptTarget::Account(recorded),
            AuditAction::AccountArchive,
        ) if recorded == account => Ok(Some(UndoOutcome::Archived {
            account: account.clone(),
            undo_receipt,
        })),
        _ => Err(invalid_stored_target("undo outcome does not match receipt")),
    }
}

fn reverse_posting(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    receipt: ReceiptId,
    id: TransactionId,
) -> Result<(), CliError> {
    let result = reverse_in_scope(
        tx,
        actor,
        &ReverseTransactionParams {
            company_slug: company,
            id,
            date: LocalDate::default().into(),
            description: None,
            reference: None,
        },
        Some(receipt),
    );
    match result {
        Ok(PostResult::Created(_)) => Ok(()),
        Ok(PostResult::Skipped(_)) => Err(invalid_stored_target(
            "owner reversal was unexpectedly skipped",
        )),
        Err(CliError::PeriodLocked(error)) => {
            Err(ineligible(receipt, UndoIneligibility::PeriodLocked(error)))
        }
        Err(CliError::ReversalBlocked(error)) => Err(ineligible(
            receipt,
            UndoIneligibility::ReversalBlocked(error),
        )),
        Err(error) => Err(error),
    }
}

fn archive_account(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    receipt: ReceiptId,
    code: &AccountCode,
) -> Result<(), CliError> {
    let company = scope.slug();
    let latest_creation: Option<ReceiptId> = tx
        .read()
        .query_row(
            "SELECT id FROM audit_log WHERE company_slug = ?1 AND entity = 'account'
         AND entity_id = ?2 AND action = 'create' ORDER BY id DESC LIMIT 1",
            params![company, code.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if latest_creation.is_some_and(|latest| latest > receipt) {
        return Err(ineligible(
            receipt,
            UndoIneligibility::NewerAccountCreation(code.clone()),
        ));
    }
    if accounts::find_status(scope, code.as_str())? != Some(accounts::AccountStatus::Active) {
        return Err(ineligible(
            receipt,
            UndoIneligibility::AccountNotActive(code.clone()),
        ));
    }

    let conn = tx.connection();
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::Account,
            entity_id: code.as_str(),
            action: AuditAction::AccountArchive,
            snapshots: (
                Some(&json!({ "archived": false }).to_string()),
                Some(&json!({ "archived": true, "undo_of": receipt }).to_string()),
            ),
        },
        AuditLinks::Archive(receipt),
    )?;
    conn.execute(
        "UPDATE accounts SET archived = 1 WHERE company_slug = ?1 AND code = ?2",
        params![company, code.as_str()],
    )?;
    Ok(())
}

fn ineligible(receipt: ReceiptId, reason: UndoIneligibility) -> CliError {
    ReceiptUndoError::Ineligible { receipt, reason }.into()
}

pub(in crate::db) fn verify_archives(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mismatch: Option<String> = reader
        .query_row(
            "SELECT a.company_slug || '/' || a.code
             FROM accounts a LEFT JOIN audit_log event ON event.company_slug = a.company_slug
                 AND event.entity = 'account' AND event.entity_id = a.code
                 AND event.action = 'account_archive'
             WHERE a.archived <> (event.id IS NOT NULL)
         UNION ALL
         SELECT event.company_slug || '/' || event.entity_id
             FROM audit_log event LEFT JOIN accounts a ON a.company_slug = event.company_slug
                 AND a.code = event.entity_id
             WHERE event.action = 'account_archive' AND (
                 a.code IS NULL OR event.agent_uri IS NULL
                 OR NOT EXISTS (
                     SELECT 1 FROM audit_log original
                     WHERE original.id = event.undo_of
                       AND original.id < event.id AND original.company_slug = event.company_slug
                       AND original.entity = 'account' AND original.entity_id = event.entity_id
                       AND original.action = 'create'
                 )
             ) LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(account) = mismatch {
        return Err(CliError::Database(format!(
            "account '{account}' does not match its archive history"
        )));
    }
    Ok(())
}
