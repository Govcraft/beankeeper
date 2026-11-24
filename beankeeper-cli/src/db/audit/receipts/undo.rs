use std::fmt;

use beankeeper::types::AccountCode;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ReceiptChange, ReceiptEvent, ReceiptId, ReceiptTarget, posting_creation};
use crate::db::audit::setup::{
    books::{ProfileRevision, current_profile},
    direct::{self, OnboardingEffect},
};
use crate::db::audit::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, ReversalBlock, ReverseTransactionParams,
    audit_columns, read_audit_row, record_with_links, reverse_in_scope, savepoint::AuditedTx,
    snapshot,
};
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::ReadConn;
use crate::db::ids::TransactionId;
use crate::db::periods::ClosedPostingPeriod;
use crate::db::rules::{self, RuleActivation, RuleId};
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
    /// The original rule was removed or superseded.
    RuleNotCurrent(RuleId),
    /// A newer profile revision is still in effect.
    ProfileNotCurrent(ProfileRevision),
    /// The transaction's metadata diverged from the receipt or has a newer edit.
    AnnotationsNotCurrent(TransactionId),
    /// The newly created book still has active accounts or posted activity.
    BookInUse,
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
            Self::RuleNotCurrent(id) => write!(formatter, "rule '{id}' is no longer current"),
            Self::ProfileNotCurrent(revision) => write!(
                formatter,
                "book profile revision {} is no longer current",
                revision.get()
            ),
            Self::AnnotationsNotCurrent(id) => {
                write!(formatter, "transaction {id} has newer metadata changes")
            }
            Self::BookInUse => formatter.write_str("book has active accounts or posted activity"),
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
    /// The transaction's prior annotations were restored.
    AnnotationsRestored {
        transaction: TransactionId,
        undo_receipt: ReceiptId,
    },
    /// The account remains in the ledger and rejects ordinary new postings.
    Archived {
        account: AccountCode,
        undo_receipt: ReceiptId,
    },
}

impl UndoOutcome {
    /// The audit receipt that records the reversal or archive.
    #[must_use]
    pub const fn receipt(&self) -> ReceiptId {
        match *self {
            Self::Reversed { undo_receipt, .. }
            | Self::AnnotationsRestored { undo_receipt, .. }
            | Self::Archived { undo_receipt, .. } => undo_receipt,
        }
    }
}

/// Audited outcome for a single change or an onboarding operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ReceiptUndoOutcome {
    /// A single account archive, posting reversal, or annotation restore.
    Single(UndoOutcome),
    /// A complete onboarding operation reversed in one write.
    Onboarding(OnboardingUndoOutcome),
}

impl ReceiptUndoOutcome {
    /// The audit receipt recording the completed undo.
    #[must_use]
    pub const fn receipt(&self) -> ReceiptId {
        match self {
            Self::Single(outcome) => outcome.receipt(),
            Self::Onboarding(outcome) => outcome.undo_receipt,
        }
    }
}

/// The receipt and effects of an onboarding undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename = "onboarding_reverted")]
pub struct OnboardingUndoOutcome {
    undo_receipt: ReceiptId,
    #[serde(flatten)]
    change: OnboardingUndo,
}

/// Effects of the owner undo of an onboarding receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OnboardingUndo {
    /// Receipts for the posting reversals or account archives.
    child_receipts: Vec<ReceiptId>,
    /// The profile revision retired by this undo, if any.
    profile_revision: Option<ProfileRevision>,
    /// Whether the retired profile belonged to a book created by this request.
    book_archived: bool,
}

/// A categorization rule creation reversed by an owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleUndoOutcome {
    rule: RuleId,
    undo_receipt: ReceiptId,
}

impl RuleUndoOutcome {
    /// The retained rule that was deactivated.
    #[must_use]
    pub const fn rule(&self) -> &RuleId {
        &self.rule
    }

    /// The audit receipt that records the deactivation.
    #[must_use]
    pub const fn undo_receipt(&self) -> ReceiptId {
        self.undo_receipt
    }
}

enum UndoTarget {
    Posting(TransactionId),
    Account(AccountCode),
    Annotations(AnnotationUndo),
}

struct AnnotationUndo {
    transaction: TransactionId,
    before: Option<String>,
    after: String,
}

// Provenance checked before a rule can be deactivated.
struct RuleCreation {
    rule: RuleId,
    receipt: ReceiptId,
}

impl TryFrom<(RuleId, ReceiptId, AuditAction)> for RuleCreation {
    type Error = ReceiptUndoError;

    fn try_from(
        (rule, receipt, action): (RuleId, ReceiptId, AuditAction),
    ) -> Result<Self, Self::Error> {
        match action {
            AuditAction::Create => Ok(Self { rule, receipt }),
            _ => Err(ReceiptUndoError::Unsupported(receipt)),
        }
    }
}

impl RuleCreation {
    fn recorded_deactivation(self, event: &ReceiptEvent) -> Result<RuleUndoOutcome, CliError> {
        match (event.target(), event.action()) {
            (ReceiptTarget::Rule(rule), AuditAction::RuleDeactivate) if *rule == self.rule => {
                Ok(RuleUndoOutcome {
                    rule: self.rule,
                    undo_receipt: event.id().into(),
                })
            }
            _ => Err(invalid_stored_target("expected rule deactivation")),
        }
    }
}

/// Undo a single account, posting, or annotation receipt within the live owner write scope.
///
/// Use [`undo_change`] to also support direct onboarding receipts.
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
        let scope = Company::bind_retained(tx.read(), company.clone())?;
        let target = resolve_target(&scope, receipt)?;
        undo_single(tx, actor, &scope, receipt, &target)
    })
}

/// Undo an account, posting, annotation, or complete direct onboarding operation.
///
/// # Errors
/// Returns an authorization, not-found, eligibility, or storage error.
/// All effects of an onboarding undo commit together.
pub fn undo_change(
    grant: Granted<'_, OwnerUndo>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<ReceiptUndoOutcome, CliError> {
    grant.mutate("sp_owner_undo_change", |tx, actor| {
        let scope = Company::bind_retained(tx.read(), company.clone())?;
        if let Some(effect) = direct::find_effect(tx.read(), company, receipt)? {
            undo_onboarding(tx, actor, &scope, receipt, effect).map(ReceiptUndoOutcome::Onboarding)
        } else {
            let target = resolve_target(&scope, receipt)?;
            undo_single(tx, actor, &scope, receipt, &target).map(ReceiptUndoOutcome::Single)
        }
    })
}

fn undo_single(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    receipt: ReceiptId,
    target: &UndoTarget,
) -> Result<UndoOutcome, CliError> {
    let company = scope.slug();
    if let Some(outcome) = recorded_outcome(tx.read(), company, receipt, target)? {
        return Ok(outcome);
    }
    match target {
        UndoTarget::Posting(id) => reverse_posting(tx, actor, company, receipt, *id)?,
        UndoTarget::Account(code) => archive_account(tx, actor, scope, receipt, code)?,
        UndoTarget::Annotations(edit) => edit.restore(tx, actor, company, receipt)?,
    }
    recorded_outcome(tx.read(), company, receipt, target)?
        .ok_or_else(|| CliError::Database(format!("undo of receipt #{receipt} was not recorded")))
}

fn undo_onboarding(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    receipt: ReceiptId,
    effect: OnboardingEffect,
) -> Result<OnboardingUndoOutcome, CliError> {
    let company = scope.slug();
    let saved: Option<(ReceiptId, String)> = tx
        .read()
        .query_row(
            "SELECT id, after FROM audit_log WHERE undo_of = ?1 AND company_slug = ?2
         AND action = 'setup_revert'",
            params![receipt, company],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((undo_receipt, raw)) = saved {
        let change = serde_json::from_str(&raw).map_err(invalid_stored_target)?;
        return Ok(OnboardingUndoOutcome {
            undo_receipt,
            change,
        });
    }

    let change = match effect {
        OnboardingEffect::Profile {
            revision,
            created_book,
        } => check_profile_undo(tx.read(), company, receipt, revision, created_book)?,
        OnboardingEffect::Accounts { receipts } => {
            undo_onboarding_children(tx, actor, scope, receipt, receipts, AuditEntity::Account)?
        }
        OnboardingEffect::Opening { receipt: member } => undo_onboarding_children(
            tx,
            actor,
            scope,
            receipt,
            vec![member],
            AuditEntity::Transaction,
        )?,
        OnboardingEffect::Statement { receipts } => undo_onboarding_children(
            tx,
            actor,
            scope,
            receipt,
            receipts,
            AuditEntity::Transaction,
        )?,
    };
    record_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::Company,
            entity_id: &receipt.to_string(),
            action: AuditAction::SetupRevert,
            snapshots: (None, Some(&snapshot(&change))),
        },
        AuditLinks::Undo(receipt),
    )?;
    Ok(OnboardingUndoOutcome {
        undo_receipt: tx.connection().last_insert_rowid().try_into()?,
        change,
    })
}

fn check_profile_undo(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
    revision: ProfileRevision,
    created_book: bool,
) -> Result<OnboardingUndo, CliError> {
    match current_profile(reader, company) {
        Ok(profile) if profile.revision == revision => (),
        Ok(_) | Err(CliError::NotFound(_)) => {
            return Err(ineligible(
                receipt,
                UndoIneligibility::ProfileNotCurrent(revision),
            ));
        }
        Err(error) => return Err(error),
    }
    if created_book {
        let in_use = reader.query_row(
            "SELECT EXISTS (SELECT 1 FROM accounts WHERE company_slug = ?1 AND archived = 0)
                 OR EXISTS (SELECT 1 FROM transactions WHERE company_slug = ?1)",
            [company],
            |row| row.get::<_, bool>(0),
        )?;
        if in_use {
            return Err(ineligible(receipt, UndoIneligibility::BookInUse));
        }
    }
    Ok(OnboardingUndo {
        child_receipts: Vec::new(),
        profile_revision: Some(revision),
        book_archived: created_book,
    })
}

fn undo_onboarding_children(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    receipt: ReceiptId,
    members: Vec<ReceiptId>,
    entity: AuditEntity,
) -> Result<OnboardingUndo, CliError> {
    let mut child_receipts = Vec::with_capacity(members.len());
    for member in members {
        let belongs = tx.read().query_row(
            "SELECT EXISTS (SELECT 1 FROM audit_log WHERE id = ?1 AND id < ?2
             AND company_slug = ?3 AND entity = ?4 AND action = 'create')",
            params![member, receipt, scope.slug(), entity.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if !belongs {
            return Err(invalid_stored_target(
                "onboarding member does not match receipt",
            ));
        }
        let target = resolve_target(scope, member)?;
        child_receipts.push(undo_single(tx, actor, scope, member, &target)?.receipt());
    }
    Ok(OnboardingUndo {
        child_receipts,
        profile_revision: None,
        book_archived: false,
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
        (ReceiptTarget::Transaction(id), AuditAction::MetadataSet | AuditAction::MetadataUnset) => {
            let ReceiptChange::AnnotationsEdited { before, after } = event.summary() else {
                return Err(invalid_stored_target("missing annotation snapshots"));
            };
            Ok(UndoTarget::Annotations(AnnotationUndo {
                transaction: *id,
                before: before.clone(),
                after: after.clone(),
            }))
        }
        _ => Err(ReceiptUndoError::Unsupported(receipt).into()),
    }
}

fn invalid_stored_target(error: impl fmt::Display) -> CliError {
    CliError::Database(format!("invalid target in audit receipt: {error}"))
}

fn recorded_undo_event(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<Option<ReceiptEvent>, CliError> {
    let columns = audit_columns(crate::db::schema::CURRENT_VERSION);
    reader
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
        .transpose()
}

fn recorded_outcome(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
    target: &UndoTarget,
) -> Result<Option<UndoOutcome>, CliError> {
    let event = recorded_undo_event(reader, company, receipt)?;
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
        (
            UndoTarget::Annotations(edit),
            ReceiptTarget::Transaction(recorded),
            AuditAction::MetadataRestore,
        ) if *recorded == edit.transaction => Ok(Some(UndoOutcome::AnnotationsRestored {
            transaction: edit.transaction,
            undo_receipt,
        })),
        _ => Err(invalid_stored_target("undo outcome does not match receipt")),
    }
}

impl AnnotationUndo {
    fn restore(
        &self,
        tx: &AuditedTx<'_>,
        actor: &Actor,
        company: &CompanySlug,
        receipt: ReceiptId,
    ) -> Result<(), CliError> {
        let conn = tx.connection();
        let target = self.transaction.to_string();
        let changed = conn.execute(
            "UPDATE transactions SET metadata = ?1
             WHERE company_slug = ?2 AND id = ?3 AND metadata IS ?4
               AND NOT EXISTS (
                 SELECT 1 FROM audit_log newer
                 WHERE newer.company_slug = ?2 AND newer.entity = 'transaction'
                   AND newer.entity_id = ?5 AND newer.id > ?6
                   AND newer.action IN ('metadata_set', 'metadata_unset')
                   AND NOT EXISTS (SELECT 1 FROM audit_log restored WHERE restored.undo_of = newer.id)
               )",
            params![self.before, company, self.transaction, self.after, target, receipt],
        )?;
        if changed == 0 {
            return Err(ineligible(
                receipt,
                UndoIneligibility::AnnotationsNotCurrent(self.transaction),
            ));
        }
        record_with_links(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Transaction,
                entity_id: &target,
                action: AuditAction::MetadataRestore,
                snapshots: (Some(&self.after), self.before.as_deref()),
            },
            AuditLinks::Undo(receipt),
        )
    }
}

/// Undo a rule creation using the rule ID returned to the caller on creation.
///
/// # Errors
/// Returns not-found for unknown rules, a named undo error for amendments or
/// removed rules, or an authorization or storage error.
pub fn undo_rule_creation(
    grant: Granted<'_, OwnerUndo>,
    company: &CompanySlug,
    id: RuleId,
) -> Result<RuleUndoOutcome, CliError> {
    grant.mutate("sp_owner_undo_rule", |tx, actor| {
        let scope = Company::bind(tx.read(), company.clone())?;
        let (receipt, action): (ReceiptId, AuditAction) = tx
            .read()
            .query_row(
                "SELECT id, action FROM audit_log WHERE company_slug = ?1 AND entity = 'rule'
                 AND entity_id = ?2 AND action IN ('create', 'rule_supersede')",
                params![company, id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                CliError::NotFound(format!("rule '{id}' not found in company '{company}'"))
            })?;
        let creation = RuleCreation::try_from((id, receipt, action))?;
        if let Some(event) = recorded_undo_event(tx.read(), company, receipt)? {
            return creation.recorded_deactivation(&event);
        }
        append_rule_deactivation(tx, actor, &scope, creation)
    })
}

fn append_rule_deactivation(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    RuleCreation { rule: id, receipt }: RuleCreation,
) -> Result<RuleUndoOutcome, CliError> {
    let rule = rules::get_rule(scope, &id).map_err(|error| match error {
        CliError::NotFound(_) => ineligible(receipt, UndoIneligibility::RuleNotCurrent(id.clone())),
        other => other,
    })?;
    let conn = tx.connection();
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(scope.slug()),
            entity: AuditEntity::Rule,
            entity_id: &rule.id().to_string(),
            action: AuditAction::RuleDeactivate,
            snapshots: (
                Some(&snapshot(&RuleActivation::Active)),
                Some(&snapshot(&RuleActivation::Deactivated)),
            ),
        },
        AuditLinks::Undo(receipt),
    )?;
    Ok(RuleUndoOutcome {
        rule: id,
        undo_receipt: conn.last_insert_rowid().try_into()?,
    })
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
            date: LocalDate::from(None).into(),
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
        AuditLinks::Undo(receipt),
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
