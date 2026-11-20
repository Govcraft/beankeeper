//! Static Schedule C mapping and lossless unmatched activity summaries.

use std::collections::BTreeMap;
use std::fmt::Write;

use beankeeper::types::Currency;
use comfy_table::Table;
use serde::Serialize;

use super::{TaxEntry, TaxReport, add, csv_error, decimal, finish_csv, overflow};
use crate::error::CliError;

// Input lines only, excluding computed subtotals. Based on the 2026 IRS form.
// The final boolean marks credit-positive lines; line 2 is debit-positive.
const LINES: &[(&str, &str, bool)] = &[
    ("1", "Gross receipts or sales", true),
    ("2", "Returns and allowances", false),
    ("4", "Cost of goods sold", false),
    ("6", "Other income", true),
    ("8", "Advertising", false),
    ("9", "Car and truck expenses", false),
    ("10", "Commissions and fees", false),
    ("11", "Contract labor", false),
    ("12", "Depletion", false),
    (
        "13",
        "Depreciation and section 179 expense deduction",
        false,
    ),
    ("14", "Employee benefit programs", false),
    ("15", "Insurance (other than health)", false),
    ("16a", "Mortgage interest", false),
    ("16b", "Vehicle loan interest", false),
    ("16c", "Other interest", false),
    ("17", "Legal and professional services", false),
    ("18", "Office expense", false),
    ("19", "Pension and profit-sharing plans", false),
    (
        "20a",
        "Rent or lease: vehicles, machinery, and equipment",
        false,
    ),
    ("20b", "Rent or lease: other business property", false),
    ("21", "Repairs and maintenance", false),
    ("22", "Supplies", false),
    ("23", "Taxes and licenses", false),
    ("24a", "Travel", false),
    ("24b", "Deductible meals", false),
    ("25", "Utilities", false),
    ("26", "Wages", false),
    (
        "27a",
        "Energy efficient commercial buildings deduction",
        false,
    ),
    ("27b", "Other expenses", false),
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
    fn add(&mut self, entry: &TaxEntry, credit_positive: bool) -> Result<(), CliError> {
        match entry.direction.as_str() {
            "debit" => add(&mut self.debit_total, entry.amount)?,
            "credit" => add(&mut self.credit_total, entry.amount)?,
            _ => {
                return Err(CliError::Validation(
                    "invalid entry direction in tax report".into(),
                ));
            }
        }
        self.net = if credit_positive {
            self.credit_total.checked_sub(self.debit_total)
        } else {
            self.debit_total.checked_sub(self.credit_total)
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
            let (id, title, credit_positive) = LINES[index];
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
            row.add(entry, credit_positive)?;
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
            row.add(
                entry,
                matches!(
                    entry.account_type.as_str(),
                    "revenue" | "liability" | "equity"
                ),
            )?;
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
