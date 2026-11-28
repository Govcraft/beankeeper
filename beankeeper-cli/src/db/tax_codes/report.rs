//! VAT evidence and control account movements for the same date window.

use std::collections::BTreeMap;

use beankeeper::types::AccountCode;
use chrono::NaiveDate;
use rusqlite::params;
use serde::Serialize;

use super::{RateRevision, TaxError, TaxRateRevisionId, VatEvidence, decode, list_rates};
use crate::{
    db::{companies::Company, documents::DocumentKind, ids::TransactionId},
    error::CliError,
};

#[derive(Debug, Default, Serialize)]
pub struct VatTotals {
    #[serde(rename = "net_minor")]
    net: i64,
    #[serde(rename = "vat_minor")]
    vat: i64,
    #[serde(rename = "gross_minor")]
    gross: i64,
    #[serde(rename = "recovered_vat_minor")]
    recovered_vat: i64,
}

impl VatTotals {
    fn observe(&mut self, evidence: &VatEvidence, line: &super::TaxedLine) -> Result<(), TaxError> {
        let sign = evidence.origin.sign();
        add(&mut self.net, line.amounts.net_minor * sign)?;
        add(&mut self.vat, line.amounts.vat_minor * sign)?;
        add(&mut self.gross, line.amounts.gross_minor * sign)?;
        if evidence.kind == DocumentKind::Bill && line.control(evidence.kind).is_some() {
            add(&mut self.recovered_vat, line.amounts.vat_minor * sign)?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct TotalsByCode {
    kind: DocumentKind,
    rate: RateRevision,
    #[serde(flatten)]
    totals: VatTotals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlRole {
    Payable,
    Recoverable,
}

impl ControlRole {
    const fn normal(self) -> &'static str {
        match self {
            Self::Payable => "credit",
            Self::Recoverable => "debit",
        }
    }
}

#[derive(Debug, Serialize)]
struct Difference {
    posting_id: TransactionId,
    expected_minor: i64,
    posted_minor: i64,
}

#[derive(Debug, Serialize)]
struct ControlReconciliation {
    account: AccountCode,
    role: ControlRole,
    expected_minor: i64,
    posted_minor: i64,
    differences: Vec<Difference>,
}

/// Book VAT movements and their reconciliation to payable and recoverable accounts.
#[derive(Debug, Serialize)]
pub struct VatRegister {
    jurisdiction: &'static str,
    rounding: &'static str,
    currency: &'static str,
    from: NaiveDate,
    to: NaiveDate,
    sales: VatTotals,
    purchases: VatTotals,
    net_control_vat_minor: i64,
    by_code: Vec<TotalsByCode>,
    evidence: Vec<VatEvidence>,
    controls: Vec<ControlReconciliation>,
}

/// Read the configured GBP VAT register from one ledger snapshot.
///
/// # Errors
/// Returns validation errors if the date range is reversed, or database or
/// range errors if evidence or accounting movements cannot be read.
pub fn read_vat_register(
    company: &Company<'_>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<VatRegister, CliError> {
    if from > to {
        return Err(CliError::Validation(
            "VAT report start date must not follow its end date".into(),
        ));
    }
    let evidence = evidence_in(company, from, to)?;
    let mut report = VatRegister {
        jurisdiction: "uk_invoice_vat",
        rounding: "nearest_penny_per_line",
        currency: "GBP",
        from,
        to,
        sales: VatTotals::default(),
        purchases: VatTotals::default(),
        net_control_vat_minor: 0,
        by_code: Vec::new(),
        evidence,
        controls: Vec::new(),
    };
    let mut codes = BTreeMap::<(TaxRateRevisionId, &str), TotalsByCode>::new();
    let controls = controls_for(company)?;
    let mut expected = BTreeMap::<AccountCode, BTreeMap<TransactionId, i64>>::new();
    for event in &report.evidence {
        let category = match event.kind {
            DocumentKind::Invoice => &mut report.sales,
            DocumentKind::Bill => &mut report.purchases,
        };
        for line in &event.lines {
            category.observe(event, line)?;
            codes
                .entry((line.revision.id.clone(), event.kind.code()))
                .or_insert_with(|| TotalsByCode {
                    kind: event.kind,
                    rate: line.revision.clone(),
                    totals: VatTotals::default(),
                })
                .totals
                .observe(event, line)?;
            if let Some(account) = line.control(event.kind) {
                let wanted = expected
                    .entry(account.clone())
                    .or_default()
                    .entry(event.posting_id)
                    .or_default();
                add(wanted, line.amounts.vat_minor * event.origin.sign())?;
            }
        }
    }
    report.net_control_vat_minor = report
        .sales
        .vat_minor
        .checked_sub(report.purchases.recovered_vat)
        .ok_or(TaxError::Amount)?;
    report.by_code = codes.into_values().collect();
    for (account, role) in controls {
        let wanted = expected.remove(&account).unwrap_or_default();
        let posted = journal_movements(company, &account, from, to, role)?;
        report
            .controls
            .push(reconcile(account, role, wanted, posted)?);
    }
    Ok(report)
}

fn evidence_in(
    company: &Company<'_>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<VatEvidence>, CliError> {
    let mut statement = company.read().prepare_cached(
        "SELECT payload FROM vat_evidence WHERE company_slug=?1 AND effective_on BETWEEN ?2 AND ?3
         ORDER BY effective_on, posting_id",
    )?;
    let rows = statement.query_map(params![company.slug(), from, to], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| decode(&row?)).collect()
}

fn controls_for(company: &Company<'_>) -> Result<BTreeMap<AccountCode, ControlRole>, CliError> {
    let mut controls = BTreeMap::new();
    for revision in list_rates(company)? {
        if let Some(account) = revision.terms.output_account {
            controls.insert(account, ControlRole::Payable);
        }
        if let Some(account) = revision.terms.input_account {
            controls.insert(account, ControlRole::Recoverable);
        }
    }
    Ok(controls)
}

fn journal_movements(
    company: &Company<'_>,
    account: &AccountCode,
    from: NaiveDate,
    to: NaiveDate,
    role: ControlRole,
) -> Result<BTreeMap<TransactionId, i64>, CliError> {
    let mut statement = company.read().prepare_cached(
        "SELECT txn.id, CASE entry.direction WHEN ?5 THEN entry.amount ELSE -entry.amount END
         FROM posted_transactions txn JOIN entries entry ON entry.transaction_id=txn.id
         WHERE txn.company_slug=?1 AND txn.currency='GBP' AND txn.date BETWEEN ?2 AND ?3
           AND entry.account_code=?4",
    )?;
    let rows = statement.query_map(
        params![company.slug(), from, to, account.as_str(), role.normal()],
        |row| Ok((row.get::<_, TransactionId>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let mut movements = BTreeMap::new();
    for row in rows {
        let (id, amount) = row?;
        add(movements.entry(id).or_default(), amount)?;
    }
    Ok(movements)
}

fn reconcile(
    account: AccountCode,
    role: ControlRole,
    expected: BTreeMap<TransactionId, i64>,
    mut actual: BTreeMap<TransactionId, i64>,
) -> Result<ControlReconciliation, TaxError> {
    let mut result = ControlReconciliation {
        account,
        role,
        expected_minor: 0,
        posted_minor: 0,
        differences: Vec::new(),
    };
    for (id, wanted) in expected {
        let posted = actual.remove(&id).unwrap_or(0);
        add(&mut result.expected_minor, wanted)?;
        add(&mut result.posted_minor, posted)?;
        if wanted != posted {
            result.differences.push(Difference {
                posting_id: id,
                expected_minor: wanted,
                posted_minor: posted,
            });
        }
    }
    for (id, posted) in actual {
        if posted != 0 {
            add(&mut result.posted_minor, posted)?;
            result.differences.push(Difference {
                posting_id: id,
                expected_minor: 0,
                posted_minor: posted,
            });
        }
    }
    result
        .differences
        .sort_unstable_by_key(|row| row.posting_id);
    Ok(result)
}

fn add(total: &mut i64, amount: i64) -> Result<(), TaxError> {
    *total = total.checked_add(amount).ok_or(TaxError::Amount)?;
    Ok(())
}

impl VatRegister {
    pub(crate) fn export_rows(&self) -> Vec<[String; 13]> {
        let mut rows = Vec::new();
        for event in &self.evidence {
            for line in &event.lines {
                let sign = event.origin.sign();
                rows.push([
                    event.origin.as_str().into(),
                    event.kind.code().into(),
                    event.effective_on.to_string(),
                    event.document_id.to_string(),
                    event.posting_id.to_string(),
                    line.revision.name.to_string(),
                    line.revision.id.to_string(),
                    line.control(event.kind)
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    (line.amounts.net_minor * sign).to_string(),
                    (line.amounts.vat_minor * sign).to_string(),
                    (line.amounts.gross_minor * sign).to_string(),
                    String::new(),
                    String::new(),
                ]);
            }
        }
        for control in &self.controls {
            for difference in &control.differences {
                rows.push([
                    "difference".into(),
                    control.role.normal().into(),
                    String::new(),
                    String::new(),
                    difference.posting_id.to_string(),
                    String::new(),
                    String::new(),
                    control.account.to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                    difference.expected_minor.to_string(),
                    difference.posted_minor.to_string(),
                ]);
            }
        }
        rows
    }
}
