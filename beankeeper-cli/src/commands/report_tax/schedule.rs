//! Static Schedule C mapping and lossless unmatched activity summaries.

use std::collections::BTreeMap;
use std::fmt::Write;

use beankeeper::types::Currency;
use comfy_table::Table;
use serde::Serialize;

use super::{TaxEntry, TaxReport, add, csv_error, decimal, finish_csv, overflow};
use crate::error::CliError;

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

#[derive(Serialize)]
pub(super) struct ScheduleSummary {
    lines: Vec<ScheduleRow>,
    unmapped: Vec<ScheduleRow>,
    untagged: Vec<ScheduleRow>,
}

#[derive(Serialize)]
struct ScheduleRow {
    line: Option<&'static str>,
    title: String,
    tax_category: Option<String>,
    account_code: Option<String>,
    account_type: Option<String>,
    debit_total: i64,
    credit_total: i64,
    net: i64,
}

impl ScheduleRow {
    fn add(&mut self, entry: &TaxEntry, sign: BalanceSign) -> Result<(), CliError> {
        match entry.direction.as_str() {
            "debit" => add(&mut self.debit_total, entry.amount)?,
            "credit" => add(&mut self.credit_total, entry.amount)?,
            _ => {
                return Err(CliError::Validation(
                    "invalid entry direction in tax report".into(),
                ));
            }
        }
        self.net = match sign {
            CreditPositive => self.credit_total.checked_sub(self.debit_total),
            DebitPositive => self.debit_total.checked_sub(self.credit_total),
        }
        .ok_or_else(overflow)?;
        Ok(())
    }
}

pub(super) fn build(entries: &[TaxEntry]) -> Result<ScheduleSummary, CliError> {
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
            let row = lines.entry(index).or_insert_with(|| ScheduleRow {
                line: Some(id),
                title: title.into(),
                tax_category: Some(format!("sched-c:{id}")),
                account_code: None,
                account_type: None,
                debit_total: 0,
                credit_total: 0,
                net: 0,
            });
            row.add(entry, sign)?;
        } else if entry.tax_category.is_some()
            || matches!(entry.account_type.as_str(), "revenue" | "expense")
        {
            let groups = if entry.tax_category.is_some() {
                &mut unmapped
            } else {
                &mut untagged
            };
            let row = groups
                .entry((entry.tax_category.clone(), entry.account_code.clone()))
                .or_insert_with(|| ScheduleRow {
                    line: None,
                    title: entry.account_name.clone(),
                    tax_category: entry.tax_category.clone(),
                    account_code: Some(entry.account_code.clone()),
                    account_type: Some(entry.account_type.clone()),
                    debit_total: 0,
                    credit_total: 0,
                    net: 0,
                });
            let sign = match entry.account_type.as_str() {
                "revenue" | "liability" | "equity" => CreditPositive,
                _ => DebitPositive,
            };
            row.add(entry, sign)?;
        }
    }
    Ok(ScheduleSummary {
        lines: lines.into_values().collect(),
        unmapped: unmapped.into_values().collect(),
        untagged: untagged.into_values().collect(),
    })
}

pub(super) fn render_table(report: &TaxReport<ScheduleSummary>, currency: Currency) -> String {
    let mut rendered = format!(
        "Schedule C book summary ({} to {}) ({})\nNot tax advice; not a filed form.\n",
        report.from, report.to, report.currency
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
                row.line.unwrap_or("").into(),
                row.title.clone(),
                row.tax_category.clone().unwrap_or_default(),
                row.account_code.clone().unwrap_or_default(),
                row.account_type.clone().unwrap_or_default(),
                decimal(row.debit_total, currency),
                decimal(row.credit_total, currency),
                decimal(row.net, currency),
            ]);
        }
        let _ = write!(rendered, "\n{section}\n{table}\n");
    }
    rendered
}

pub(super) fn render_csv(report: &TaxReport<ScheduleSummary>) -> Result<String, CliError> {
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
                    row.line.unwrap_or(""),
                    &row.title,
                    row.tax_category.as_deref().unwrap_or(""),
                    row.account_code.as_deref().unwrap_or(""),
                    row.account_type.as_deref().unwrap_or(""),
                    &row.debit_total.to_string(),
                    &row.credit_total.to_string(),
                    &row.net.to_string(),
                    &report.currency,
                    &report.from,
                    &report.to,
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
