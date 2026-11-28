//! Pair open items with posted settlements and generate audited credits.

use std::collections::BTreeSet;

use beankeeper::types::{AccountCode, DebitOrCredit};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{DocumentOutcome, persist_event, review::ReviewedMutation};
use crate::{
    db::{
        amounts::PositiveMinorUnits,
        audit::{
            Actor, AuditAction, AuditEntity, counterparties, has_reconciled_entries,
            post_in_audited_scope, savepoint::AuditedTx,
        },
        companies::Company,
        counterparties::CounterpartyId,
        documents::{
            AllocationHistory, AllocationId, AllocationKind, CapacityFor, DocumentError,
            DocumentId, DocumentNumber, EffectiveAllocation, IssuedDocument, IssuedLine,
            find_document, opposite_side, peak_applied, read_allocation,
        },
        holds::EntryStatus,
        ids::{ReceiptId, TransactionId},
        periods,
        tax_codes::{self, TaxError, TaxedLine, VatEvidence, VatOrigin},
        transactions::{
            ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams, get_transaction,
        },
    },
    error::CliError,
};

/// Apply part of an existing payment to one invoice or bill.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AllocateItem {
    /// The invoice or bill.
    pub document_id: DocumentId,
    /// Positive amount in integer minor units.
    #[schemars(with = "u64")]
    pub amount_minor: PositiveMinorUnits,
}

/// Post one partial or full credit against a document line.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreditInput {
    /// The document being credited.
    pub document_id: DocumentId,
    /// User-supplied credit note reference.
    pub number: DocumentNumber,
    /// One-based original line number.
    pub line: u32,
    /// Positive amount in integer minor units.
    #[schemars(with = "u64")]
    pub amount_minor: PositiveMinorUnits,
    /// Effective accounting date.
    #[schemars(with = "String")]
    pub date: NaiveDate,
}

pub(super) fn apply_payment(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    source: TransactionId,
    on: NaiveDate,
    items: Vec<AllocateItem>,
) -> Result<DocumentOutcome, CliError> {
    periods::ensure_open(company.read(), company.slug(), on)?;
    let planned = plan_payment(company, source, on, items)?;
    if let Some(party) = planned.link_party {
        counterparties::associate_issued(tx.connection(), actor, company, party, source)?;
    }
    let mut applied = Vec::with_capacity(planned.items.len());
    for item in planned.items {
        applied.push(save_allocation(tx, actor, company, item, None)?);
    }
    Ok(DocumentOutcome::Allocations {
        items: applied,
        unapplied_minor: Some(planned.unapplied_minor),
    })
}

impl AllocationKind {
    const fn code(self) -> &'static str {
        match self {
            Self::Payment => "payment",
            Self::Credit => "credit",
        }
    }
}

struct AllocationPlan {
    document_id: DocumentId,
    source_id: TransactionId,
    effective_on: NaiveDate,
    amount: PositiveMinorUnits,
    kind: AllocationKind,
}

struct PaymentPlan {
    items: Vec<AllocationPlan>,
    link_party: Option<CounterpartyId>,
    unapplied_minor: i64,
}

fn plan_payment(
    company: &Company<'_>,
    source: TransactionId,
    on: NaiveDate,
    items: Vec<AllocateItem>,
) -> Result<PaymentPlan, CliError> {
    if items.is_empty() || items.len() > 1000 {
        return Err(CliError::Validation(
            "provide between 1 and 1000 allocations".into(),
        ));
    }
    let (payment, entries) = get_transaction(company, source)?;
    if payment.status() != EntryStatus::Posted
        || is_document_charge(company, source)?
        || is_source_reversed(company, source)?
    {
        return Err(DocumentError::IncompatibleSource.into());
    }
    if on < payment.date {
        return Err(DocumentError::AllocationBeforeSource.into());
    }
    let party: Option<CounterpartyId> = company.read().query_row(
        "SELECT counterparty_id FROM transaction_counterparties WHERE company_slug=?1 AND transaction_id=?2",
        params![company.slug(), source], |row| row.get(0),
    ).optional()?;
    let mut targets = BTreeSet::new();
    let mut planned = Vec::with_capacity(items.len());
    let mut base: Option<IssuedDocument> = None;
    let mut sum = 0_i128;
    for item in items {
        if !targets.insert(item.document_id.clone()) {
            return Err(CliError::Validation(
                "each document can appear only once in a payment allocation request".into(),
            ));
        }
        let issued = issued_document(company, &item.document_id)?;
        if issued.currency != payment.currency
            || party
                .as_ref()
                .is_some_and(|current| current != &issued.counterparty.id)
            || base.as_ref().is_some_and(|root| {
                root.control_account != issued.control_account
                    || root.kind != issued.kind
                    || root.counterparty.id != issued.counterparty.id
            })
        {
            return Err(DocumentError::IncompatibleSource.into());
        }
        ensure_item_capacity(company, &issued, on, item.amount_minor)?;
        sum += i128::from(item.amount_minor);
        base.get_or_insert(issued);
        planned.push(AllocationPlan {
            document_id: item.document_id,
            source_id: source,
            effective_on: on,
            amount: item.amount_minor,
            kind: AllocationKind::Payment,
        });
    }
    let document = base.ok_or(DocumentError::IncompatibleSource)?;
    ensure_source_account(company, source, &document)?;
    let usable = source_capacity(&entries, &document);
    let peak = peak_applied(company, CapacityFor::Source(source), on)?;
    validate_capacity(usable, peak, sum)?;
    let applied: i64 = company.read().query_row(
        "SELECT COALESCE(SUM(amount_minor), 0) FROM document_allocations
         WHERE company_slug=?1 AND source_id=?2 AND effective_on<=?3
           AND (reversed_on IS NULL OR reversed_on>?3)",
        params![company.slug(), source, on],
        |row| row.get(0),
    )?;
    let unapplied_minor =
        i64::try_from(usable - i128::from(applied) - sum).map_err(|_| DocumentError::OutOfRange)?;
    Ok(PaymentPlan {
        items: planned,
        link_party: party.is_none().then_some(document.counterparty.id),
        unapplied_minor,
    })
}

fn ensure_source_account(
    company: &Company<'_>,
    source: TransactionId,
    document: &IssuedDocument,
) -> Result<(), CliError> {
    let incompatible: bool = company.read().query_row(
        "SELECT EXISTS(
            SELECT 1 FROM document_allocations allocation
            JOIN book_documents doc ON doc.company_slug=allocation.company_slug AND doc.id=allocation.document_id
            WHERE allocation.company_slug=?1 AND allocation.source_id=?2 AND doc.control_account IS NOT ?3)",
        params![company.slug(), source, document.control_account.as_str()], |row| row.get(0),
    )?;
    if incompatible {
        Err(DocumentError::IncompatibleSource.into())
    } else {
        Ok(())
    }
}

fn source_capacity(entries: &[crate::db::EntryRow], document: &IssuedDocument) -> i128 {
    entries
        .iter()
        .filter(|entry| entry.account_code == document.control_account.as_str())
        .map(|entry| {
            let magnitude = i128::from(entry.amount);
            if entry.direction == opposite_side(document.kind.charge_side()) {
                magnitude
            } else {
                -magnitude
            }
        })
        .sum()
}

fn validate_capacity(capacity: i128, consumed: i128, requested: i128) -> Result<(), DocumentError> {
    if capacity <= 0 || consumed + requested > capacity {
        return Err(DocumentError::AllocationExceedsBalance);
    }
    Ok(())
}

fn issued_document(company: &Company<'_>, id: &DocumentId) -> Result<IssuedDocument, CliError> {
    find_document(company, id)?.issued.ok_or_else(|| {
        CliError::Validation("issue the document before recording allocations".into())
    })
}

fn is_document_charge(company: &Company<'_>, transaction: TransactionId) -> Result<bool, CliError> {
    Ok(company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM book_documents WHERE company_slug=?1 AND posting_id=?2)
           OR EXISTS(SELECT 1 FROM document_allocations WHERE company_slug=?1 AND source_id=?2 AND kind='credit')",
        params![company.slug(), transaction], |row| row.get(0),
    )?)
}

fn is_source_reversed(company: &Company<'_>, transaction: TransactionId) -> Result<bool, CliError> {
    Ok(company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM bookkeeping_audit WHERE company_slug=?1 AND entity='transaction'
           AND action='reverse' AND (entity_id=CAST(?2 AS TEXT) OR related_posting=?2))",
        params![company.slug(), transaction], |row| row.get(0),
    )?)
}

fn ensure_item_capacity(
    company: &Company<'_>,
    issued: &IssuedDocument,
    on: NaiveDate,
    amount: PositiveMinorUnits,
) -> Result<(), CliError> {
    if on < issued.issue_date {
        return Err(DocumentError::AllocationBeforeSource.into());
    }
    let applied = peak_applied(company, CapacityFor::Document(&issued.id), on)?;
    Ok(validate_capacity(
        i128::from(issued.total_minor),
        applied,
        i128::from(amount),
    )?)
}

/// A newly applied payment or credit and its audit receipt.
#[derive(Debug, Serialize, Deserialize)]
pub struct AllocationResult {
    /// The applied settlement.
    #[serde(flatten)]
    pub allocation: EffectiveAllocation,
    /// Audit row that records the allocation.
    pub receipt_id: ReceiptId,
}

fn save_allocation(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    plan: AllocationPlan,
    credit_line: Option<u32>,
) -> Result<AllocationResult, CliError> {
    let history = AllocationHistory {
        allocation: EffectiveAllocation {
            id: AllocationId::generate(),
            document_id: plan.document_id,
            source_id: plan.source_id,
            effective_on: plan.effective_on,
            amount_minor: plan.amount,
            kind: plan.kind,
        },
        reversed_on: None,
        credit_line,
    };
    let a = &history.allocation;
    let receipt = persist_event(
        tx,
        actor,
        company.slug(),
        (
            AuditEntity::DocumentAllocation,
            AuditAction::DocumentAllocate,
        ),
        &a.id,
        (None, &history),
    )?;
    tx.connection().execute(
        "INSERT INTO document_allocations(company_slug,id,document_id,source_id,effective_on,amount_minor,kind,credit_line,receipt)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![company.slug(), a.id, a.document_id, a.source_id, a.effective_on,
            a.amount_minor, a.kind.code(), credit_line, receipt],
    )?;
    Ok(AllocationResult {
        allocation: history.allocation,
        receipt_id: receipt,
    })
}

pub(super) fn prepare_credit(
    company: &Company<'_>,
    input: CreditInput,
) -> Result<ReviewedMutation, CliError> {
    let document = issued_document(company, &input.document_id)?;
    let Some(original) = document.vat.iter().find(|line| line.number == input.line) else {
        return Ok(ReviewedMutation::Credit(input));
    };
    let credited = credited_gross(company, &input)?;
    let vat = original.credit(credited, input.amount_minor)?;
    Ok(ReviewedMutation::VatCredit {
        input,
        vat: Box::new(vat),
    })
}

fn credited_gross(company: &Company<'_>, input: &CreditInput) -> Result<i64, CliError> {
    Ok(company.read().query_row(
        "SELECT COALESCE(SUM(amount_minor),0) FROM document_allocations
         WHERE company_slug=?1 AND document_id=?2 AND credit_line=?3",
        params![company.slug(), input.document_id, input.line],
        |row| row.get(0),
    )?)
}

pub(super) fn credit(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    input: CreditInput,
    expected_vat: Option<TaxedLine>,
) -> Result<DocumentOutcome, CliError> {
    periods::ensure_open(company.read(), company.slug(), input.date)?;
    let document = issued_document(company, &input.document_id)?;
    ensure_item_capacity(company, &document, input.date, input.amount_minor)?;
    let line = input
        .line
        .checked_sub(1)
        .and_then(|index| document.lines.get(index as usize))
        .ok_or_else(|| CliError::Validation("unknown document line number".into()))?;
    let credited = credited_gross(company, &input)?;
    let original_vat = document.vat.iter().find(|tax| tax.number == input.line);
    let gross = original_vat.map_or(line.subtotal_minor.minor_units(), |tax| {
        tax.amounts.gross_minor
    });
    if i128::from(credited) + i128::from(input.amount_minor) > i128::from(gross) {
        return Err(DocumentError::AllocationExceedsBalance.into());
    }
    let vat = original_vat
        .map(|original| original.credit(credited, input.amount_minor))
        .transpose()?;
    if vat != expected_vat {
        return Err(TaxError::StaleReview.into());
    }
    let entries = credit_entries(&document, line, input.amount_minor, vat.as_ref())?;
    let source = match post_in_audited_scope(
        tx,
        &PostTransactionParams {
            company_slug: company.slug(),
            description: &format!("Credit {} for {}", input.number, document.number),
            metadata: None,
            correlate: None,
            reference: Some(&format!("doc:credit:{}", input.number)),
            currency: document.currency,
            date: input.date,
            entries: &entries,
            on_conflict: ConflictStrategy::Error,
            actor,
        },
    )? {
        PostResult::Created(id) => id,
        PostResult::Skipped { .. } => {
            return Err(CliError::Database(
                "document credit posting was not created".into(),
            ));
        }
    };
    // Credits belong to the frozen issued party even if the inactive directory
    // record now prevents issuing a new invoice.
    counterparties::associate_issued(
        tx.connection(),
        actor,
        company,
        document.counterparty.id,
        source,
    )?;
    let vat_evidence = VatEvidence {
        origin: VatOrigin::Credit,
        document_id: input.document_id.clone(),
        posting_id: source,
        effective_on: input.date,
        kind: document.kind,
        lines: vat.into_iter().collect(),
    };
    crate::db::audit::tax_codes::persist_evidence(tx, actor, company, &vat_evidence)?;
    let result = save_allocation(
        tx,
        actor,
        company,
        AllocationPlan {
            document_id: input.document_id,
            source_id: source,
            effective_on: input.date,
            amount: input.amount_minor,
            kind: AllocationKind::Credit,
        },
        Some(input.line),
    )?;
    Ok(DocumentOutcome::Allocations {
        items: vec![result],
        unapplied_minor: None,
    })
}

fn credit_entries(
    document: &IssuedDocument,
    line: &IssuedLine,
    gross: PositiveMinorUnits,
    vat: Option<&TaxedLine>,
) -> Result<Vec<PostEntryParams>, CliError> {
    let leg = |account: &AccountCode, direction: DebitOrCredit, amount| PostEntryParams {
        account_code: account.as_str().to_owned(),
        direction,
        amount,
        memo: Some(line.line.description.to_string()),
        tax_category: None,
    };
    let charge = document.kind.charge_side();
    let mut entries = vec![leg(&document.control_account, opposite_side(charge), gross)];
    let mut ordinary = gross.minor_units();
    if let Some(tax) = vat
        && tax.amounts.vat_minor > 0
        && let Some(control) = tax.control(document.kind)
    {
        entries.push(leg(
            control,
            charge,
            tax_codes::positive(tax.amounts.vat_minor)?,
        ));
        ordinary -= tax.amounts.vat_minor;
    }
    if ordinary > 0 {
        entries.push(leg(
            &line.line.account,
            charge,
            tax_codes::positive(ordinary)?,
        ));
    }
    Ok(entries)
}

pub(super) fn revoke(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    id: &AllocationId,
    date: NaiveDate,
) -> Result<DocumentOutcome, CliError> {
    let before = read_allocation(company, id)?;
    if before.reversed_on == Some(date) {
        return Ok(DocumentOutcome::Revoked {
            allocation: before,
            receipt_id: None,
        });
    }
    if before.reversed_on.is_some() || before.allocation.kind != AllocationKind::Payment {
        return Err(CliError::Validation(
            "only an active payment allocation can be revoked".into(),
        ));
    }
    if date < before.allocation.effective_on {
        return Err(DocumentError::AllocationBeforeSource.into());
    }
    periods::ensure_open(company.read(), company.slug(), date)?;
    let document = issued_document(company, &before.allocation.document_id)?;
    if has_reconciled_entries(tx.connection(), before.allocation.source_id)?
        || has_reconciled_entries(tx.connection(), document.posting_id)?
    {
        return Err(CliError::Validation(
            "a reconciled invoice, bill, or payment cannot be reallocated".into(),
        ));
    }
    let mut after = before.clone();
    after.reversed_on = Some(date);
    let receipt = persist_event(
        tx,
        actor,
        company.slug(),
        (
            AuditEntity::DocumentAllocation,
            AuditAction::DocumentUnallocate,
        ),
        id,
        (Some(&before), &after),
    )?;
    tx.connection().execute(
        "UPDATE document_allocations SET reversed_on=?1, receipt=?2 WHERE company_slug=?3 AND id=?4 AND reversed_on IS NULL",
        params![date, receipt, company.slug(), id],
    )?;
    Ok(DocumentOutcome::Revoked {
        allocation: after,
        receipt_id: Some(receipt),
    })
}
