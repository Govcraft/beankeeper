//! Expense write transitions behind typed grants and shared journal posting.

use std::path::Path;

use beankeeper::types::{AccountCode, AccountType, DebitOrCredit, DocumentType, Sha256Hex};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    ClaimFunding, ClaimTerms, ClaimView, ExpenseClaimId, ReceiptFile, ReceiptIntakeId, ReceiptView,
    parse, receipt_path, receipt_show, require_receipt_file, show,
    types::{ClaimRevision, ClaimStatus, ClaimTransition, ReviewerIdentity, transition},
};
use crate::{
    commands::reference::TransactionReference,
    db::{
        Actor, Db, EntryRow,
        accounts::get_active_account,
        amounts::PositiveMinorUnits,
        attachments::{StoreAttachmentParams, hash_and_store},
        audit::{self, expense_claims as history, savepoint::AuditedTx},
        companies::{Company, CompanySlug},
        counterparties::find_counterparty,
        ids::TransactionId,
        transactions::{
            self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams,
            PostingSource,
        },
    },
    error::CliError,
    identity::{Approve, Granted, Propose, Reconcile},
};

fn validation(value: impl Into<String>) -> CliError {
    CliError::Validation(value.into())
}

fn digest(input: &impl Serialize) -> Result<Sha256Hex, CliError> {
    let bytes = serde_json::to_vec(input).map_err(|error| CliError::General(error.to_string()))?;
    Ok(Sha256Hex::from_digest(Sha256::digest(bytes).into()))
}

fn book_path(db: &Db) -> Result<std::path::PathBuf, CliError> {
    db.file_path()
        .ok_or_else(|| validation("receipt evidence requires a file-backed ledger"))
}

pub(crate) fn intake(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    file: ReceiptFile,
) -> Result<ReceiptView, CliError> {
    let path = book_path(db)?;
    Granted::<Propose>::with_db(db, actor, |grant| {
        grant.mutate("sp_receipt_intake", |tx, actor| {
            Company::bind(tx.read(), slug.clone())?;
            let prior = tx
                .read()
                .query_row(
                    "SELECT details FROM receipt_intakes WHERE company_slug=?1 AND digest=?2",
                    params![slug, file.hash.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let (stored, _) = hash_and_store(file.bytes.as_slice(), &path)?;
            if stored != file.hash {
                return Err(CliError::Database(
                    "receipt content hash changed during storage".into(),
                ));
            }
            if let Some(prior) = prior {
                return parse(&prior);
            }
            let record = ReceiptView {
                id: ReceiptIntakeId::generate(),
                hash: file.hash,
                media: file.media,
                filename: file.filename,
                findings: file.findings,
            };
            history::store_receipt(tx, actor, slug, &record)?;
            Ok(record)
        })
    })
}

#[derive(Serialize)]
pub(crate) struct DraftRequest {
    #[serde(skip)]
    pub(crate) reference: TransactionReference,
    pub(crate) terms: ClaimTerms,
    pub(crate) duplicate_reason: Option<String>,
}

fn bounded_reason(reason: &str) -> Result<&str, CliError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 512 {
        return Err(validation("reason requires 1 to 512 UTF-8 bytes"));
    }
    Ok(reason)
}

fn validate_terms(company: &Company<'_>, terms: &ClaimTerms) -> Result<(), CliError> {
    let functional = crate::db::fx::functional_currency(company.read(), company.slug())?;
    terms.validate(functional)?;
    find_counterparty(company, &terms.claimant)?.require_active()?;
    receipt_show(company, &terms.receipt)?;
    for split in &terms.splits {
        require_account(company, &split.account, &[AccountType::Expense])?;
    }
    match &terms.funding {
        ClaimFunding::Reimbursable { payable } => {
            require_account(company, payable, &[AccountType::Liability])
        }
        ClaimFunding::CompanyCard { transaction } => {
            let (journal, _) = transactions::get_transaction(company, *transaction)?;
            if journal.currency != terms.currency || journal.date < terms.incurred_on {
                return Err(validation(
                    "company card posting must use the confirmed currency and cannot predate the receipt",
                ));
            }
            Ok(())
        }
    }
}

fn require_account(
    company: &Company<'_>,
    code: &AccountCode,
    acceptable: &[AccountType],
) -> Result<(), CliError> {
    let account = get_active_account(company, code.as_str())?;
    let foreign = company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM fx_accounts WHERE company_slug=?1 AND account_code=?2)",
        params![company.slug(), code.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    if foreign || !acceptable.contains(&account.account_type) {
        return Err(validation(format!(
            "account {code} has the wrong type or uses a foreign currency"
        )));
    }
    Ok(())
}

pub(crate) fn create(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    input: DraftRequest,
) -> Result<ClaimView, CliError> {
    let intent = digest(&input)?;
    Granted::<Propose>::with_db(db, actor, |grant| {
        grant.mutate("sp_expense_draft", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let prior: Option<ExpenseClaimId> = tx
                .read()
                .query_row(
                    "SELECT id FROM expense_claims WHERE company_slug=?1 AND request_key=?2",
                    params![slug, input.reference.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = prior {
                return initial_retry(&company, &id, &intent, actor);
            }
            draft_in_scope(tx, actor, &company, input, intent)
        })
    })
}

fn initial_retry(
    company: &Company<'_>,
    id: &ExpenseClaimId,
    intent: &Sha256Hex,
    actor: &Actor,
) -> Result<ClaimView, CliError> {
    let raw = company.read().query_row(
        "SELECT details FROM expense_claim_revisions WHERE company_slug=?1 AND claim_id=?2 AND revision=1",
        params![company.slug(),id], |row| row.get::<_,String>(0),
    )?;
    let initial: ClaimView = parse(&raw)?;
    if initial.intent != *intent || initial.draft_owner != ReviewerIdentity::from(actor) {
        return Err(validation(
            "expense claim reference was already used for a different request",
        ));
    }
    show(company, id)
}

fn draft_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    input: DraftRequest,
    intent: Sha256Hex,
) -> Result<ClaimView, CliError> {
    validate_terms(company, &input.terms)?;
    let duplicate = tx.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM expense_claims WHERE company_slug=?1 AND receipt_id=?2)",
        params![company.slug(), input.terms.receipt],
        |row| row.get::<_, bool>(0),
    )?;
    let reason = input
        .duplicate_reason
        .as_deref()
        .map(bounded_reason)
        .transpose()?;
    if duplicate && reason.is_none() {
        return Err(validation(
            "receipt is already attached to another expense claim; an explicit duplicate reason is required",
        ));
    }
    let claim = ClaimView {
        id: ExpenseClaimId::generate(),
        revision: ClaimRevision::FIRST,
        status: ClaimStatus::Draft,
        terms: input.terms,
        draft_owner: ReviewerIdentity::from(actor),
        submitter: None,
        reviewed_by: None,
        approval_journal: None,
        reimbursed_minor: 0,
        reason: None,
        duplicate_reason: reason.map(str::to_owned),
        last_payment: None,
        last_journal: None,
        intent,
    };
    history::store_claim(tx, company.slug(), &claim, input.reference.as_str())?;
    history::store_revision(
        tx,
        actor,
        company.slug(),
        &claim,
        "create",
        input.reference.as_str(),
    )?;
    Ok(claim)
}

#[derive(Debug, Serialize)]
pub(crate) enum ClaimAction {
    Amend(Box<ClaimTerms>),
    Submit,
    Approve,
    Reject(String),
    RecordPayment(TransactionId),
    Void { on: NaiveDate, reason: String },
}

impl ClaimAction {
    fn event(&self) -> &'static str {
        match self {
            Self::Amend(_) => "amend",
            Self::Submit => "submit",
            Self::Approve => "approve",
            Self::Reject(_) => "reject",
            Self::RecordPayment(_) => "record_payment",
            Self::Void { .. } => "void",
        }
    }
}

pub(crate) struct ChangeRequest {
    pub(crate) claim: ExpenseClaimId,
    pub(crate) reference: TransactionReference,
    pub(crate) expected_revision: u16,
    pub(crate) action: ClaimAction,
}

pub(crate) fn apply(
    db: &Db,
    actor: &Actor,
    slug: CompanySlug,
    input: ChangeRequest,
) -> Result<ClaimView, CliError> {
    let path = match input.action {
        ClaimAction::Approve => Some(book_path(db)?),
        _ => None,
    };
    let context = Decision {
        slug,
        input,
        path: path.as_deref(),
    };
    match context.input.action {
        ClaimAction::Amend(_) | ClaimAction::Submit => apply_with::<Propose>(db, actor, context),
        ClaimAction::Approve | ClaimAction::Reject(_) => apply_with::<Approve>(db, actor, context),
        ClaimAction::Void { .. } => apply_with::<crate::identity::Reverse>(db, actor, context),
        ClaimAction::RecordPayment(_) => apply_with::<Reconcile>(db, actor, context),
    }
}

fn apply_with<C: audit::grant::sealed::GrantKind>(
    db: &Db,
    actor: &Actor,
    decision: Decision<'_>,
) -> Result<ClaimView, CliError> {
    Granted::<C>::with_db(db, actor, |grant| {
        grant.mutate("sp_expense_change", |tx, actor| decision.run(tx, actor))
    })
}

struct Decision<'a> {
    slug: CompanySlug,
    input: ChangeRequest,
    path: Option<&'a Path>,
}

impl Decision<'_> {
    fn run(self, tx: &AuditedTx<'_>, actor: &Actor) -> Result<ClaimView, CliError> {
        let company = Company::bind(tx.read(), self.slug.clone())?;
        let mut claim = show(&company, &self.input.claim)?;
        let principal = ReviewerIdentity::from(actor);
        let intent = digest(&(self.input.expected_revision, &self.input.action, &principal))?;
        let reference = self.input.reference.as_str();
        let retry = tx.read().query_row(
            "SELECT details FROM expense_claim_revisions WHERE company_slug=?1 AND claim_id=?2 AND change_key=?3",
            params![self.slug,claim.id,reference], |row| row.get::<_,String>(0),
        ).optional()?;
        if let Some(raw) = retry {
            let event: ClaimView = parse(&raw)?;
            if event.intent != intent {
                return Err(validation(
                    "expense claim change reference was already used for a different request",
                ));
            }
            return Ok(event);
        }
        if self.input.expected_revision != claim.revision.get() {
            return Err(validation(format!(
                "expense claim changed; review revision {}",
                claim.revision.get()
            )));
        }
        claim.revision = claim.revision.next()?;
        claim.last_payment = None;
        claim.last_journal = None;
        claim.intent = intent;
        let event = self.input.action.event();
        Writer { tx, actor, company }.perform(&mut claim, self.input.action, self.path)?;
        history::store_revision(tx, actor, &self.slug, &claim, event, reference)?;
        Ok(claim)
    }
}

struct Writer<'a> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    company: Company<'a>,
}

impl Writer<'_> {
    fn perform(
        &self,
        claim: &mut ClaimView,
        action: ClaimAction,
        path: Option<&Path>,
    ) -> Result<(), CliError> {
        let principal = ReviewerIdentity::from(self.actor);
        match action {
            ClaimAction::Amend(terms) => self.amend(claim, &principal, *terms),
            ClaimAction::Submit => {
                require_owner(claim, &principal)?;
                claim.status = transition(claim.status, 0, ClaimTransition::Submit)?;
                validate_terms(&self.company, &claim.terms)?;
                claim.submitter = Some(principal);
                Ok(())
            }
            ClaimAction::Approve => self.approve(
                claim,
                principal,
                path.ok_or_else(|| validation("receipt store path is unavailable"))?,
            ),
            ClaimAction::Reject(reason) => {
                require_other_reviewer(claim, &principal)?;
                claim.status = transition(claim.status, 0, ClaimTransition::Reject)?;
                claim.reviewed_by = Some(principal);
                claim.reason = Some(bounded_reason(&reason)?.to_owned());
                Ok(())
            }
            ClaimAction::RecordPayment(payment) => self.record_payment(claim, payment),
            ClaimAction::Void { on, reason } => self.void(claim, on, &reason),
        }
    }

    fn amend(
        &self,
        claim: &mut ClaimView,
        actor: &ReviewerIdentity,
        terms: ClaimTerms,
    ) -> Result<(), CliError> {
        require_owner(claim, actor)?;
        claim.status = transition(claim.status, 0, ClaimTransition::Amend)?;
        if claim.terms.receipt != terms.receipt || claim.terms.claimant != terms.claimant {
            return Err(validation("a claim's receipt and claimant are immutable"));
        }
        validate_terms(&self.company, &terms)?;
        claim.terms = terms;
        claim.submitter = None;
        claim.reviewed_by = None;
        claim.reason = None;
        Ok(())
    }

    fn approve(
        &self,
        claim: &mut ClaimView,
        principal: ReviewerIdentity,
        root: &Path,
    ) -> Result<(), CliError> {
        require_other_reviewer(claim, &principal)?;
        validate_terms(&self.company, &claim.terms)?;
        let receipt = receipt_show(&self.company, &claim.terms.receipt)?;
        require_receipt_file(root, &receipt)?;
        let (transaction, role, status) = match &claim.terms.funding {
            ClaimFunding::Reimbursable { payable } => {
                let status = transition(claim.status, 0, ClaimTransition::Approve)?;
                let journal = self.post(claim, payable, claim.terms.incurred_on, false)?;
                (journal, "liability", status)
            }
            ClaimFunding::CompanyCard { transaction } => {
                let status = transition(claim.status, 0, ClaimTransition::MatchCard)?;
                match_card(&self.company, &claim.terms, *transaction)?;
                (*transaction, "card", status)
            }
        };
        history::link_journal(self.tx, self.company.slug(), claim, transaction, role)?;
        attach(self, &receipt, transaction)?;
        claim.status = status;
        claim.approval_journal = Some(transaction);
        claim.last_journal = Some(transaction);
        claim.reviewed_by = Some(principal);
        Ok(())
    }

    fn post(
        &self,
        claim: &ClaimView,
        payable: &AccountCode,
        on: NaiveDate,
        reverse: bool,
    ) -> Result<TransactionId, CliError> {
        let terms = &claim.terms;
        let side = if reverse {
            DebitOrCredit::Credit
        } else {
            DebitOrCredit::Debit
        };
        let entries = terms
            .splits
            .iter()
            .map(|split| line(&split.account, side, split.amount_minor))
            .chain([line(payable, side.opposite(), terms.amount_minor)])
            .collect::<Vec<_>>();
        let label = if reverse {
            "Expense claim cancellation"
        } else {
            "Expense claim"
        };
        let reference = if reverse {
            format!("claim-void:{}", claim.id)
        } else {
            format!("claim-approval:{}", claim.id)
        };
        let result = transactions::post_direct(
            self.tx,
            &PostTransactionParams {
                company_slug: self.company.slug(),
                description: &format!("{label}: {}", terms.merchant),
                metadata: None,
                currency: terms.currency,
                date: on,
                entries: &entries,
                correlate: None,
                reference: Some(&reference),
                on_conflict: ConflictStrategy::Error,
                actor: self.actor,
            },
            PostingSource::Storage,
        )?;
        match result {
            PostResult::Created(id) => Ok(id),
            PostResult::Skipped(_) => Err(CliError::Database(
                "a claim journal was unexpectedly skipped".into(),
            )),
        }
    }
}

impl Writer<'_> {
    fn record_payment(
        &self,
        claim: &mut ClaimView,
        payment: TransactionId,
    ) -> Result<(), CliError> {
        let ClaimFunding::Reimbursable { payable } = &claim.terms.funding else {
            return Err(validation(
                "company-card claims do not have a reimbursement payable",
            ));
        };
        let (journal, entries) = transactions::get_transaction(&self.company, payment)?;
        require_posted(&self.company, payment, &journal, &claim.terms)?;
        if entries.len() != 2 {
            return Err(validation(
                "reimbursement evidence needs one payable debit and one funding credit",
            ));
        }
        let debit = entries
            .iter()
            .find(|entry| {
                entry.direction == DebitOrCredit::Debit && entry.account_code == payable.as_str()
            })
            .ok_or_else(|| {
                validation("reimbursement evidence does not debit the approved payable")
            })?;
        let credit = entries
            .iter()
            .find(|entry| entry.direction == DebitOrCredit::Credit)
            .ok_or_else(|| {
                validation("reimbursement evidence does not credit a funding account")
            })?;
        let funding: AccountCode = credit
            .account_code
            .parse()
            .map_err(|error| validation(format!("invalid funding account: {error}")))?;
        require_account(&self.company, &funding, &[AccountType::Asset])?;
        let outstanding = claim.terms.amount_minor.minor_units() - claim.reimbursed_minor;
        claim.status = transition(
            claim.status,
            outstanding,
            ClaimTransition::RecordPayment(debit.amount),
        )?;
        history::link_journal(self.tx, self.company.slug(), claim, payment, "payment")?;
        claim.reimbursed_minor += debit.amount.minor_units();
        claim.last_payment = Some(payment);
        Ok(())
    }

    fn void(&self, claim: &mut ClaimView, on: NaiveDate, reason: &str) -> Result<(), CliError> {
        let ClaimFunding::Reimbursable { payable } = &claim.terms.funding else {
            return Err(validation(
                "only unreimbursed liability claims can be cancelled",
            ));
        };
        let status = transition(claim.status, 0, ClaimTransition::Void)?;
        let original = claim
            .approval_journal
            .ok_or_else(|| CliError::Database("claim approval journal is missing".into()))?;
        crate::db::periods::check_open_date(
            self.tx.read(),
            self.company.slug(),
            claim.terms.incurred_on,
        )?;
        if on < claim.terms.incurred_on {
            return Err(validation(
                "cancellation date cannot predate the approved expense",
            ));
        }
        let cleared = self.tx.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM entry_clearings clearing JOIN entries entry ON entry.id=clearing.entry_id
                           WHERE entry.transaction_id=?1 AND entry.company_slug=?2)",
            params![original,self.company.slug()], |row| row.get::<_,bool>(0),
        )?;
        if cleared {
            return Err(validation(
                "remove any clearings before cancelling the approved expense",
            ));
        }
        let reason = bounded_reason(reason)?;
        let journal = self.post(claim, payable, on, true)?;
        history::link_journal(self.tx, self.company.slug(), claim, journal, "cancellation")?;
        claim.status = status;
        claim.reason = Some(reason.to_owned());
        claim.last_journal = Some(journal);
        Ok(())
    }
}

fn require_owner(claim: &ClaimView, actor: &ReviewerIdentity) -> Result<(), CliError> {
    if claim.draft_owner != *actor {
        return Err(CliError::Forbidden(
            "only the draft owner may amend or submit this claim".into(),
        ));
    }
    Ok(())
}

fn require_other_reviewer(claim: &ClaimView, actor: &ReviewerIdentity) -> Result<(), CliError> {
    if claim.submitter.as_ref() == Some(actor) {
        return Err(CliError::Forbidden(
            "the submitter cannot review their own expense claim".into(),
        ));
    }
    Ok(())
}

fn line(
    code: &AccountCode,
    direction: DebitOrCredit,
    amount: PositiveMinorUnits,
) -> PostEntryParams {
    PostEntryParams {
        account_code: code.as_str().into(),
        direction,
        amount,
        memo: None,
        tax_category: None,
    }
}

fn attach(
    writer: &Writer<'_>,
    receipt: &ReceiptView,
    journal: TransactionId,
) -> Result<(), CliError> {
    let existing = crate::db::attachments::list_attachments(&writer.company, journal)?;
    if existing
        .iter()
        .all(|attachment| attachment.hash.as_ref() != Some(&receipt.hash))
    {
        audit::attachment::insert_attachment(
            writer.tx,
            writer.actor,
            &StoreAttachmentParams {
                transaction_id: journal,
                entry_id: None,
                company_slug: writer.company.slug(),
                uri: &receipt_path(&receipt.hash),
                document_type: DocumentType::Receipt,
                hash: Some(&receipt.hash),
                original_filename: receipt.filename.as_deref(),
            },
        )?;
    }
    Ok(())
}

fn require_posted(
    company: &Company<'_>,
    id: TransactionId,
    row: &crate::db::TransactionRow,
    terms: &ClaimTerms,
) -> Result<(), CliError> {
    let audit = company.read().bookkeeping_audit();
    let excluded = company.read().query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {audit} WHERE company_slug=?1 AND (
            action='reverse' AND (entity_id=CAST(?2 AS TEXT) OR related_posting=?2)
            OR action='recategorize' AND (entity_id=CAST(?2 AS TEXT) OR related_posting=?2)))"
        ),
        params![company.slug(), id],
        |row| row.get::<_, bool>(0),
    )?;
    if excluded
        || row.status() != crate::db::holds::EntryStatus::Posted
        || row.date < terms.incurred_on
        || row.currency != terms.currency
    {
        return Err(validation(
            "matching requires a posted, unreversed journal in the confirmed currency",
        ));
    }
    Ok(())
}

fn match_card(
    company: &Company<'_>,
    terms: &ClaimTerms,
    id: TransactionId,
) -> Result<(), CliError> {
    let (row, entries) = transactions::get_transaction(company, id)?;
    require_posted(company, id, &row, terms)?;
    if entries.len() != terms.splits.len() + 1
        || !terms.splits.iter().all(|split| {
            entries
                .iter()
                .any(|entry| matches_debit(entry, &split.account, split.amount_minor))
        })
    {
        return Err(validation(
            "company card posting must match the confirmed expense splits exactly",
        ));
    }
    let funding = entries
        .iter()
        .find(|entry| entry.direction == DebitOrCredit::Credit)
        .ok_or_else(|| validation("company card posting needs a funding credit"))?;
    let funding: AccountCode = funding
        .account_code
        .parse()
        .map_err(|error| validation(format!("invalid card account: {error}")))?;
    require_account(
        company,
        &funding,
        &[AccountType::Asset, AccountType::Liability],
    )
}

fn matches_debit(entry: &EntryRow, account: &AccountCode, amount: PositiveMinorUnits) -> bool {
    entry.direction == DebitOrCredit::Debit
        && entry.account_code == account.as_str()
        && entry.amount == amount
}
