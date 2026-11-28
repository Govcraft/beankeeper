//! Effective-dated tax configuration, issued VAT evidence and journal plans.

mod model;
mod report;

use std::collections::BTreeMap;

use beankeeper::types::{AccountCode, AccountType, Currency};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub use self::model::*;
pub use self::report::{VatRegister, read_vat_register};

use crate::{
    db::{
        accounts,
        amounts::PositiveMinorUnits,
        companies::Company,
        documents::{
            DocumentDraft, DocumentId, DocumentKind, JournalPlan, PlannedLeg, opposite_side,
        },
        ids::TransactionId,
    },
    error::CliError,
};

pub(in crate::db) fn read_effective(
    company: &Company<'_>,
    code: &TaxCodeId,
    date: NaiveDate,
) -> Result<RateRevision, CliError> {
    let serialized: Option<String> = company.read().query_row(
        "SELECT payload FROM vat_rate_revisions WHERE company_slug=?1 AND code_id=?2 AND effective_from<=?3
         ORDER BY effective_from DESC LIMIT 1",
        params![company.slug(), code, date], |row| row.get(0),
    ).optional()?;
    let serialized = serialized.ok_or_else(|| TaxError::MissingRate(code.clone()))?;
    let effective: RateRevision = decode(&serialized)?;
    effective.terms.validate()?;
    Ok(effective)
}

/// List the configured revisions. An older revision remains available for audit.
///
/// # Errors
/// Returns database errors if revisions cannot be read.
pub fn list_rates(company: &Company<'_>) -> Result<Vec<RateRevision>, CliError> {
    let mut statement = company.read().prepare_cached(
        "SELECT revision.payload FROM vat_rate_revisions revision
         JOIN vat_codes code ON code.company_slug=revision.company_slug AND code.id=revision.code_id
         WHERE revision.company_slug=?1 ORDER BY code.name, revision.effective_from DESC",
    )?;
    let rows = statement.query_map([company.slug()], |row| row.get::<_, String>(0))?;
    rows.map(|row| decode(&row?)).collect()
}

pub(in crate::db) fn decode<T: serde::de::DeserializeOwned>(
    serialized: &str,
) -> Result<T, CliError> {
    serde_json::from_str(serialized)
        .map_err(|error| CliError::Database(format!("invalid stored VAT payload: {error}")))
}

pub(in crate::db) fn prepare_tax(
    company: &Company<'_>,
    draft: &DocumentDraft,
    plan: &JournalPlan,
    currency: Currency,
    date: NaiveDate,
) -> Result<Vec<TaxedLine>, CliError> {
    let mut revisions = BTreeMap::<TaxCodeId, RateRevision>::new();
    let mut applied = Vec::new();
    for (index, line) in plan.lines.iter().enumerate() {
        let Some(tax) = &line.line.tax else {
            continue;
        };
        let revision = match revisions.entry(tax.code.clone()) {
            std::collections::btree_map::Entry::Occupied(revision) => revision.get().clone(),
            std::collections::btree_map::Entry::Vacant(revision) => revision
                .insert(read_effective(company, &tax.code, date)?)
                .clone(),
        };
        let evidence = TaxedLine {
            number: u32::try_from(index + 1).map_err(|_| TaxError::Amount)?,
            amounts: calculate_document_tax(
                line.subtotal_minor,
                currency,
                revision.terms.rate,
                tax.price_mode,
            )?,
            revision,
            price_mode: tax.price_mode,
        };
        if let Some(control) = evidence.control(draft.kind) {
            let valid_type = match draft.kind {
                DocumentKind::Invoice => AccountType::Liability,
                DocumentKind::Bill => AccountType::Asset,
            };
            validate_control(company, control, valid_type)?;
            if control == &draft.control_account
                || draft.lines.iter().any(|line| &line.account == control)
            {
                return Err(TaxError::InvalidControl(control.clone()).into());
            }
        }
        applied.push(evidence);
    }
    Ok(applied)
}

pub(in crate::db) fn validate_control(
    company: &Company<'_>,
    code: &AccountCode,
    required: AccountType,
) -> Result<(), CliError> {
    if accounts::get_active_account(company, code.as_str())?.account_type != required {
        return Err(TaxError::InvalidControl(code.clone()).into());
    }
    Ok(())
}

pub(in crate::db) fn with_issue_tax(
    mut plan: JournalPlan,
    draft: &DocumentDraft,
    evidence: &[TaxedLine],
) -> Result<JournalPlan, TaxError> {
    if evidence.is_empty() {
        return Ok(plan);
    }
    let mut amounts = BTreeMap::<AccountCode, i64>::new();
    let mut tagged = evidence.iter().peekable();
    let mut total = 0_i64;
    for (index, line) in plan.lines.iter().enumerate() {
        let (gross, separated_vat) = match tagged.next_if(|e| e.number as usize == index + 1) {
            Some(tax) => {
                let control = tax.control(draft.kind);
                if let Some(control) = control {
                    accumulate(&mut amounts, control, tax.amounts.vat_minor)?;
                }
                (
                    tax.amounts.gross_minor,
                    control.map_or(0, |_| tax.amounts.vat_minor),
                )
            }
            None => (line.subtotal_minor.minor_units(), 0),
        };
        accumulate(&mut amounts, &line.line.account, gross - separated_vat)?;
        total = total.checked_add(gross).ok_or(TaxError::Amount)?;
    }
    if tagged.next().is_some() {
        return Err(TaxError::Amount);
    }
    let direction = draft.kind.charge_side();
    plan.total = positive(total)?;
    plan.legs = vec![PlannedLeg {
        account: draft.control_account.clone(),
        direction,
        amount: plan.total,
    }];
    for (account, amount) in amounts {
        plan.legs.push(PlannedLeg {
            account,
            direction: opposite_side(direction),
            amount: positive(amount)?,
        });
    }
    Ok(plan)
}

fn accumulate(
    values: &mut BTreeMap<AccountCode, i64>,
    code: &AccountCode,
    amount: i64,
) -> Result<(), TaxError> {
    if amount > 0 {
        let value = values.entry(code.clone()).or_default();
        *value = value.checked_add(amount).ok_or(TaxError::Amount)?;
    }
    Ok(())
}

pub(in crate::db) fn positive(amount: i64) -> Result<PositiveMinorUnits, TaxError> {
    amount.try_into().map_err(|_| TaxError::Amount)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VatOrigin {
    Issue,
    Credit,
}

impl VatOrigin {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Credit => "credit",
        }
    }

    pub(crate) const fn sign(self) -> i64 {
        match self {
            Self::Issue => 1,
            Self::Credit => -1,
        }
    }
}

/// Source evidence for the VAT register and control-account reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VatEvidence {
    pub origin: VatOrigin,
    pub document_id: DocumentId,
    pub posting_id: TransactionId,
    pub effective_on: NaiveDate,
    pub kind: DocumentKind,
    pub lines: Vec<TaxedLine>,
}
