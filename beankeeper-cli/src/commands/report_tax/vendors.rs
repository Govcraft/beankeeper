//! Vendor totals: each annotated transaction contributes its full debit total.

use std::collections::BTreeMap;

use beankeeper::types::Currency;
use comfy_table::Table;
use serde::Serialize;
use serde_json::Value;

use super::{TaxEntry, TaxReport, add, csv_error, decimal, finish_csv};
use crate::error::CliError;

#[derive(Serialize)]
pub(super) struct VendorSummary {
    threshold: i64,
    vendors: Vec<VendorRow>,
}

#[derive(Serialize)]
struct VendorRow {
    payee: String,
    tin: Option<String>,
    payee_address: Option<String>,
    payment_count: i64,
    total: i64,
    meets_threshold: bool,
}

pub(super) fn build(entries: &[TaxEntry], threshold: i64) -> Result<VendorSummary, CliError> {
    let mut vendors = BTreeMap::new();
    // Input is ordered by transaction ID. Count split debit legs only once.
    let mut previous_payment = None;
    for entry in entries.iter().filter(|entry| entry.direction == "debit") {
        let Some(metadata) = entry
            .metadata
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        else {
            continue;
        };
        let Some(payee) = metadata
            .get("payee")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let tin = optional_detail(&metadata, "tin", payee)?;
        let address = optional_detail(&metadata, "payee_address", payee)?;
        let row = vendors
            .entry(payee.to_owned())
            .or_insert_with(|| VendorRow {
                payee: payee.into(),
                tin: None,
                payee_address: None,
                payment_count: 0,
                total: 0,
                meets_threshold: false,
            });
        merge_detail(&mut row.tin, tin, "tin", payee)?;
        merge_detail(&mut row.payee_address, address, "payee_address", payee)?;
        add(&mut row.total, entry.amount)?;
        if previous_payment != Some(entry.transaction_id) {
            add(&mut row.payment_count, 1)?;
        }
        previous_payment = Some(entry.transaction_id);
    }
    Ok(VendorSummary {
        threshold,
        vendors: vendors
            .into_values()
            .filter_map(|mut row| {
                row.meets_threshold = row.total >= threshold;
                row.meets_threshold.then_some(row)
            })
            .collect(),
    })
}

fn optional_detail<'a>(
    metadata: &'a Value,
    key: &str,
    payee: &str,
) -> Result<Option<&'a str>, CliError> {
    match metadata.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok((!value.trim().is_empty()).then_some(value)),
        Some(_) => Err(CliError::Validation(format!(
            "{key} metadata for payee '{payee}' must be a string"
        ))),
    }
}

fn merge_detail(
    stored: &mut Option<String>,
    value: Option<&str>,
    key: &str,
    payee: &str,
) -> Result<(), CliError> {
    if let Some(value) = value {
        match stored {
            Some(existing) if existing != value => {
                return Err(CliError::Validation(format!(
                    "conflicting {key} metadata for payee '{payee}'; correct the payment metadata before exporting"
                )));
            }
            None => *stored = Some(value.into()),
            Some(_) => (),
        }
    }
    Ok(())
}

pub(super) fn render_table(report: &TaxReport<VendorSummary>, currency: Currency) -> String {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.set_header([
        "Payee",
        "TIN",
        "Address",
        "Payments",
        "Total",
        "Meets threshold",
    ]);
    for row in &report.summary.vendors {
        table.add_row([
            row.payee.clone(),
            row.tin.clone().unwrap_or_default(),
            row.payee_address.clone().unwrap_or_default(),
            row.payment_count.to_string(),
            decimal(row.total, currency),
            row.meets_threshold.to_string(),
        ]);
    }
    format!(
        "1099 vendor book summary ({} to {}) ({})\nThreshold: {} {}\nNot tax advice; not a filed form.\n\n{table}",
        report.from,
        report.to,
        report.currency,
        decimal(report.summary.threshold, currency),
        report.currency
    )
}

pub(super) fn render_csv(report: &TaxReport<VendorSummary>) -> Result<String, CliError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record([
            "payee",
            "tin",
            "payee_address",
            "payment_count",
            "total",
            "meets_threshold",
            "threshold",
            "currency",
            "from",
            "to",
        ])
        .map_err(|error| csv_error(&error))?;
    for row in &report.summary.vendors {
        writer
            .write_record([
                &row.payee,
                row.tin.as_deref().unwrap_or(""),
                row.payee_address.as_deref().unwrap_or(""),
                &row.payment_count.to_string(),
                &row.total.to_string(),
                &row.meets_threshold.to_string(),
                &report.summary.threshold.to_string(),
                &report.currency,
                &report.from,
                &report.to,
            ])
            .map_err(|error| csv_error(&error))?;
    }
    finish_csv(writer)
}
