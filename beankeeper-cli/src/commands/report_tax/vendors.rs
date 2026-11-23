//! Vendor totals: each annotated transaction contributes its full debit total.

use std::collections::BTreeMap;

use beankeeper::types::{Currency, Money};
use comfy_table::Table;
use serde::Serialize;
use serde_json::Value;

use super::{TaxEntry, TaxReport, add, csv_error, finish_csv};
use crate::error::CliError;
use crate::output::amounts::MoneyField;

/// Nonnegative minimum paid to a vendor, in one currency.
#[derive(Debug, Clone, Copy)]
pub struct VendorThreshold {
    minor: i64,
    currency: Currency,
}

impl VendorThreshold {
    /// Validate a vendor report threshold.
    ///
    /// # Errors
    /// Returns [`CliError::Validation`] for a negative amount or for an amount
    /// outside the supported minor-unit range.
    pub fn new(money: Money) -> Result<Self, CliError> {
        let minor = i64::try_from(money.amount().minor_units()).map_err(|_| {
            CliError::Validation("vendor threshold exceeds the minor-unit range".into())
        })?;
        if minor < 0 {
            return Err(CliError::Validation(
                "--threshold must be nonnegative".into(),
            ));
        }
        Ok(Self {
            minor,
            currency: money.currency(),
        })
    }

    /// The validated threshold and its currency.
    #[must_use]
    pub fn money(self) -> Money {
        Money::from_minor(i128::from(self.minor), self.currency)
    }
}

impl TryFrom<(super::VendorMinimum, Currency)> for VendorThreshold {
    type Error = CliError;

    fn try_from((amount, currency): (super::VendorMinimum, Currency)) -> Result<Self, Self::Error> {
        let minor = amount.0.minor_units(currency)?;
        Ok(Self { minor, currency })
    }
}

/// Vendor payments meeting the requested threshold.
#[derive(Serialize)]
pub struct VendorSummary {
    #[serde(flatten)]
    threshold: MoneyField,
    vendors: Vec<VendorRow>,
}

impl VendorSummary {
    /// Minimum included total in the report currency.
    #[must_use]
    pub const fn threshold(&self) -> Money {
        self.threshold.money()
    }

    /// Included vendors, ordered by payee.
    #[must_use]
    pub fn vendors(&self) -> &[VendorRow] {
        &self.vendors
    }
}

/// Payments attributed to one recorded payee.
#[derive(Serialize)]
pub struct VendorRow {
    /// Payee name from recorded transaction metadata.
    pub payee: String,
    /// Taxpayer identification number, when recorded.
    pub tin: Option<String>,
    /// Mailing address, when recorded.
    pub payee_address: Option<String>,
    /// Number of payments counted for the payee.
    pub payment_count: i64,
    #[serde(flatten)]
    total: MoneyField,
    meets_threshold: bool,
}

impl VendorRow {
    /// Total paid in the report currency.
    #[must_use]
    pub const fn total(&self) -> Money {
        self.total.money()
    }
}

struct AccumulatedVendor {
    payee: String,
    tin: Option<String>,
    payee_address: Option<String>,
    payment_count: i64,
    total: i64,
}

pub(super) fn build(
    entries: &[TaxEntry],
    threshold: VendorThreshold,
) -> Result<VendorSummary, CliError> {
    let mut vendors = BTreeMap::new();
    // Input is ordered by transaction ID. Count split debit legs only once.
    let mut previous_payment = None;
    for entry in entries.iter().filter(|entry| entry.direction.is_debit()) {
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
            .or_insert_with(|| AccumulatedVendor {
                payee: payee.into(),
                tin: None,
                payee_address: None,
                payment_count: 0,
                total: 0,
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
        threshold: MoneyField::new("threshold", threshold.money()),
        vendors: vendors
            .into_values()
            .filter(|row| row.total >= threshold.minor)
            .map(|row| VendorRow {
                payee: row.payee,
                tin: row.tin,
                payee_address: row.payee_address,
                payment_count: row.payment_count,
                total: MoneyField::new(
                    "total",
                    Money::from_minor(i128::from(row.total), threshold.currency),
                ),
                meets_threshold: true,
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

pub(super) fn render_table(report: &TaxReport<VendorSummary>) -> String {
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
            row.total.decimal(),
            row.meets_threshold.to_string(),
        ]);
    }
    format!(
        "1099 vendor book summary ({} to {}) ({})\nThreshold: {} {}\nNot tax advice; not a filed form.\n\n{table}",
        report.period().from(),
        report.period().to(),
        report.currency(),
        report.summary.threshold.decimal(),
        report.currency()
    )
}

/// Export a typed vendor book summary using the CLI's CSV columns.
///
/// # Errors
/// Returns [`CliError`] if CSV serialization fails.
pub fn render_csv(report: &TaxReport<VendorSummary>) -> Result<String, CliError> {
    let from = report.period().from().to_string();
    let to = report.period().to().to_string();
    let currency = report.currency();
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
                &row.total.minor_units().to_string(),
                &row.meets_threshold.to_string(),
                &report.summary.threshold.minor_units().to_string(),
                currency.code(),
                &from,
                &to,
            ])
            .map_err(|error| csv_error(&error))?;
    }
    finish_csv(writer)
}
