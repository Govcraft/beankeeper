//! Static Schedule C mapping and lossless unmatched activity summaries.

use std::collections::BTreeMap;
use std::fmt::Write;

use beankeeper::types::{Currency, DebitOrCredit, Money};
use comfy_table::Table;
use serde::Serialize;

use super::{TaxEntry, TaxReport, add, csv_error, finish_csv, overflow};
use crate::error::CliError;
use crate::output::amounts::MoneyField;

#[derive(Clone, Copy)]
enum BalanceSign {
    CreditPositive,
    DebitPositive,
}

use BalanceSign::{CreditPositive, DebitPositive};

// Input lines only, excluding computed subtotals. Based on the 2026 IRS form.
const LINES: &[(&str, &str, BalanceSign)] = &[
    ("1", "Gross receipts or sales", CreditPositive),
    ("2", "Returns and allowances", DebitPositive),
    ("4", "Cost of goods sold", DebitPositive),
    ("6", "Other income", CreditPositive),
    ("8", "Advertising", DebitPositive),
    ("9", "Car and truck expenses", DebitPositive),
    ("10", "Commissions and fees", DebitPositive),
    ("11", "Contract labor", DebitPositive),
    ("12", "Depletion", DebitPositive),
    (
        "13",
        "Depreciation and section 179 expense deduction",
        DebitPositive,
    ),
    ("14", "Employee benefit programs", DebitPositive),
    ("15", "Insurance (other than health)", DebitPositive),
    ("16a", "Mortgage interest", DebitPositive),
    ("16b", "Vehicle loan interest", DebitPositive),
    ("16c", "Other interest", DebitPositive),
    ("17", "Legal and professional services", DebitPositive),
    ("18", "Office expense", DebitPositive),
    ("19", "Pension and profit-sharing plans", DebitPositive),
    (
        "20a",
        "Rent or lease: vehicles, machinery, and equipment",
        DebitPositive,
    ),
    (
        "20b",
        "Rent or lease: other business property",
        DebitPositive,
    ),
    ("21", "Repairs and maintenance", DebitPositive),
    ("22", "Supplies", DebitPositive),
    ("23", "Taxes and licenses", DebitPositive),
    ("24a", "Travel", DebitPositive),
    ("24b", "Deductible meals", DebitPositive),
    ("25", "Utilities", DebitPositive),
    ("26", "Wages", DebitPositive),
    (
        "27a",
        "Energy efficient commercial buildings deduction",
        DebitPositive,
    ),
    ("27b", "Other expenses", DebitPositive),
];

/// Schedule C lines and unmatched bookkeeping activity.
#[derive(Serialize)]
pub struct ScheduleSummary {
    lines: Vec<ScheduleRow>,
    unmapped: Vec<ScheduleRow>,
    untagged: Vec<ScheduleRow>,
}

impl ScheduleSummary {
    /// Activity mapped to Schedule C lines, in form order.
    #[must_use]
    pub fn lines(&self) -> &[ScheduleRow] {
        &self.lines
    }

    /// Tagged activity without a recognized Schedule C mapping.
    #[must_use]
    pub fn unmapped(&self) -> &[ScheduleRow] {
        &self.unmapped
    }

    /// Untagged expense and revenue account activity.
    #[must_use]
    pub fn untagged(&self) -> &[ScheduleRow] {
        &self.untagged
    }
}

#[derive(Serialize)]
struct ScheduleRowIdentity {
    line: Option<&'static str>,
    title: String,
    tax_category: Option<String>,
    account_code: Option<String>,
    account_type: Option<beankeeper::types::AccountType>,
}

/// One row in a Schedule C book summary.
#[derive(Serialize)]
pub struct ScheduleRow {
    #[serde(flatten)]
    identity: ScheduleRowIdentity,
    #[serde(flatten)]
    debit_total: MoneyField,
    #[serde(flatten)]
    credit_total: MoneyField,
    #[serde(flatten)]
    net: MoneyField,
}

impl ScheduleRow {
    /// IRS line code for recognized mappings.
    #[must_use]
    pub const fn line(&self) -> Option<&'static str> {
        self.identity.line
    }

    /// Form line title or account name.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.identity.title
    }

    /// Original or canonical tax category, if present.
    #[must_use]
    pub fn tax_category(&self) -> Option<&str> {
        self.identity.tax_category.as_deref()
    }

    /// Account code for an unmatched row.
    #[must_use]
    pub fn account_code(&self) -> Option<&str> {
        self.identity.account_code.as_deref()
    }

    /// Account type code for an unmatched row.
    #[must_use]
    pub fn account_type(&self) -> Option<&str> {
        self.identity
            .account_type
            .map(beankeeper::types::AccountType::code)
    }

    /// Total debits in the report currency.
    #[must_use]
    pub const fn debit_total(&self) -> Money {
        self.debit_total.money()
    }

    /// Total credits in the report currency.
    #[must_use]
    pub const fn credit_total(&self) -> Money {
        self.credit_total.money()
    }

    /// Net on the form line or account's normal side.
    #[must_use]
    pub const fn net(&self) -> Money {
        self.net.money()
    }
}

struct AccumulatedRow {
    identity: ScheduleRowIdentity,
    debit_total: i64,
    credit_total: i64,
    net: i64,
}

impl AccumulatedRow {
    fn new(identity: ScheduleRowIdentity) -> Self {
        Self {
            identity,
            debit_total: 0,
            credit_total: 0,
            net: 0,
        }
    }

    fn in_currency(self, currency: Currency) -> ScheduleRow {
        let money =
            |key, minor| MoneyField::new(key, Money::from_minor(i128::from(minor), currency));
        ScheduleRow {
            identity: self.identity,
            debit_total: money("debit_total", self.debit_total),
            credit_total: money("credit_total", self.credit_total),
            net: money("net", self.net),
        }
    }

    fn add(&mut self, entry: &TaxEntry, sign: BalanceSign) -> Result<(), CliError> {
        match entry.direction {
            DebitOrCredit::Debit => add(&mut self.debit_total, entry.amount)?,
            DebitOrCredit::Credit => add(&mut self.credit_total, entry.amount)?,
        }
        self.net = match sign {
            CreditPositive => self.credit_total.checked_sub(self.debit_total),
            DebitPositive => self.debit_total.checked_sub(self.credit_total),
        }
        .ok_or_else(overflow)?;
        Ok(())
    }
}

pub(super) fn build(entries: &[TaxEntry], currency: Currency) -> Result<ScheduleSummary, CliError> {
    let mut lines = BTreeMap::new();
    let mut unmapped = BTreeMap::new();
    let mut untagged = BTreeMap::new();
    for entry in entries {
        let mapped = entry.tax_category.as_deref().and_then(|category| {
            let normalized = category.to_ascii_lowercase();
            normalized
                .strip_prefix("sched-c:")
                .and_then(|id| LINES.iter().position(|line| line.0 == id))
        });
        if let Some(index) = mapped {
            let (id, title, sign) = LINES[index];
            let row = lines.entry(index).or_insert_with(|| {
                AccumulatedRow::new(ScheduleRowIdentity {
                    line: Some(id),
                    title: title.into(),
                    tax_category: Some(format!("sched-c:{id}")),
                    account_code: None,
                    account_type: None,
                })
            });
            row.add(entry, sign)?;
        } else if entry.tax_category.is_some()
            || matches!(
                entry.account_type,
                beankeeper::types::AccountType::Revenue | beankeeper::types::AccountType::Expense
            )
        {
            let groups = if entry.tax_category.is_some() {
                &mut unmapped
            } else {
                &mut untagged
            };
            let row = groups
                .entry((entry.tax_category.clone(), entry.account_code.clone()))
                .or_insert_with(|| {
                    AccumulatedRow::new(ScheduleRowIdentity {
                        line: None,
                        title: entry.account_name.clone(),
                        tax_category: entry.tax_category.clone(),
                        account_code: Some(entry.account_code.clone()),
                        account_type: Some(entry.account_type),
                    })
                });
            let sign = if entry.account_type.is_debit_normal() {
                DebitPositive
            } else {
                CreditPositive
            };
            row.add(entry, sign)?;
        }
    }
    let in_currency = |row: AccumulatedRow| row.in_currency(currency);
    Ok(ScheduleSummary {
        lines: lines.into_values().map(in_currency).collect(),
        unmapped: unmapped.into_values().map(in_currency).collect(),
        untagged: untagged.into_values().map(in_currency).collect(),
    })
}

pub(super) fn render_table(report: &TaxReport<ScheduleSummary>) -> String {
    let mut rendered = format!(
        "Schedule C book summary ({} to {}) ({})\nNot tax advice; not a filed form.\n",
        report.period().from(),
        report.period().to(),
        report.currency()
    );
    for (section, rows) in sections(&report.summary) {
        let mut table = Table::new();
        table.load_preset(comfy_table::presets::UTF8_FULL);
        table.set_header([
            "Line",
            "Title / Account",
            "Tax Category",
            "Account",
            "Type",
            "Debit",
            "Credit",
            "Net",
        ]);
        for row in rows {
            table.add_row([
                row.line().unwrap_or("").into(),
                row.title().into(),
                row.tax_category().unwrap_or_default().into(),
                row.account_code().unwrap_or_default().into(),
                row.account_type().unwrap_or_default().into(),
                row.debit_total.decimal(),
                row.credit_total.decimal(),
                row.net.decimal(),
            ]);
        }
        let _ = write!(rendered, "\n{section}\n{table}\n");
    }
    rendered
}

/// Export a typed Schedule C book summary using the CLI's CSV columns.
///
/// # Errors
/// Returns [`CliError`] if CSV serialization fails.
pub fn render_csv(report: &TaxReport<ScheduleSummary>) -> Result<String, CliError> {
    let from = report.period().from().to_string();
    let to = report.period().to().to_string();
    let currency = report.currency();
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record([
            "section",
            "line",
            "title",
            "tax_category",
            "account_code",
            "account_type",
            "debit_total",
            "credit_total",
            "net",
            "currency",
            "from",
            "to",
        ])
        .map_err(|error| csv_error(&error))?;
    for (section, rows) in sections(&report.summary) {
        for row in rows {
            writer
                .write_record([
                    section,
                    row.line().unwrap_or(""),
                    row.title(),
                    row.tax_category().unwrap_or(""),
                    row.account_code().unwrap_or(""),
                    row.account_type().unwrap_or(""),
                    &row.debit_total.minor_units().to_string(),
                    &row.credit_total.minor_units().to_string(),
                    &row.net.minor_units().to_string(),
                    currency.code(),
                    &from,
                    &to,
                ])
                .map_err(|error| csv_error(&error))?;
        }
    }
    finish_csv(writer)
}

fn sections(summary: &ScheduleSummary) -> [(&str, &[ScheduleRow]); 3] {
    [
        ("lines", &summary.lines),
        ("unmapped", &summary.unmapped),
        ("untagged", &summary.untagged),
    ]
}
