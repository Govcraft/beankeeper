//! Table, JSON and CSV rendering for multi-column reports (issue #22).
//!
//! See [`crate::commands::report_periods`] for how the report is computed.
//! One amount column per period followed by a Total. Amounts use each
//! account's normal side. JSON and CSV carry integer minor units.

use std::collections::BTreeMap;

use beankeeper::types::{Currency, Money};
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use serde::{Serialize, Serializer};

use crate::commands::report_periods::{Column, MultiColumnReport};
use crate::error::CliError;
use crate::output::amounts::{MoneyField, MoneySequence, format_minor_grouped};
use crate::output::json::{Meta, envelope_ok};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub use crate::percent::{Percent, change_percent};

/// Sum a row of amounts (the Total column of the periods layout).
fn row_total(amounts: &[i64]) -> i128 {
    amounts.iter().copied().map(i128::from).sum()
}

pub(crate) fn validate_totals(report: &MultiColumnReport) -> Result<(), CliError> {
    let account_amounts = report.accounts.iter().map(|line| &line.amounts);
    let summary_amounts = report.summary.iter().map(|line| &line.amounts);
    for amounts in account_amounts.chain(summary_amounts) {
        i64::try_from(row_total(amounts)).map_err(|_| {
            CliError::Validation("period total exceeds the supported minor-unit range".into())
        })?;
    }
    Ok(())
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// The amount cells that follow the fixed columns for one row.
fn value_cells(report: &MultiColumnReport, amounts: &[i64]) -> Vec<String> {
    let mut cells: Vec<String> = amounts
        .iter()
        .map(|a| format_minor_grouped(*a, report.currency))
        .collect();
    cells.push(format_minor_grouped(row_total(amounts), report.currency));
    cells
}

/// Header labels for the amount columns.
fn value_headers(report: &MultiColumnReport) -> Vec<String> {
    let mut headers: Vec<String> = report.columns.iter().map(|c| c.label.clone()).collect();
    headers.push("Total".to_string());
    headers
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

/// Report title, e.g. `Income Statement by Month (2026-01-01 to 2026-03-31) (USD)`.
#[must_use]
pub fn title(report: &MultiColumnReport) -> String {
    let base = report.kind.title();
    let currency = report.currency.code();
    format!(
        "{base} by {} ({} to {}) ({currency})",
        capitalize_first(report.period.as_str()),
        report.from,
        report.to
    )
}

/// Render a multi-column report as a styled table.
#[must_use]
pub fn render_table(report: &MultiColumnReport, use_color: bool) -> String {
    let bold = anstyle::Style::new().bold();
    let cyan = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);

    let mut header = vec![
        Cell::new(styled("Code", bold, use_color)),
        Cell::new(styled("Account", bold, use_color)),
        Cell::new(styled("Type", bold, use_color)),
    ];
    header.extend(
        value_headers(report)
            .iter()
            .map(|h| Cell::new(styled(h, bold, use_color)).set_alignment(CellAlignment::Right)),
    );
    table.set_header(header);

    for line in &report.accounts {
        let mut row = vec![
            Cell::new(styled(&line.code, cyan, use_color)),
            Cell::new(&line.name),
            Cell::new(line.account_type.to_string()),
        ];
        row.extend(
            value_cells(report, &line.amounts)
                .into_iter()
                .map(|v| Cell::new(v).set_alignment(CellAlignment::Right)),
        );
        table.add_row(row);
    }

    for line in &report.summary {
        let mut row = vec![
            Cell::new(""),
            Cell::new(styled(line.label, bold, use_color)),
            Cell::new(""),
        ];
        row.extend(
            value_cells(report, &line.amounts).into_iter().map(|v| {
                Cell::new(styled(&v, bold, use_color)).set_alignment(CellAlignment::Right)
            }),
        );
        table.add_row(row);
    }

    format!("{}\n\n{table}", styled(&title(report), bold, use_color))
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ColumnJson {
    label: String,
    from: String,
    to: String,
}

impl From<&Column> for ColumnJson {
    fn from(c: &Column) -> Self {
        Self {
            label: c.label.clone(),
            from: c.from.to_string(),
            to: c.to.to_string(),
        }
    }
}

struct PeriodValuesJson<'a> {
    amounts: &'a [i64],
    currency: Currency,
}

impl Serialize for PeriodValuesJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            #[serde(flatten)]
            amounts: MoneySequence<'a>,
            #[serde(flatten)]
            total: MoneyField,
        }

        let money = |units| Money::from_minor(i128::from(units), self.currency);
        let amounts: Vec<_> = self.amounts.iter().copied().map(money).collect();
        Fields {
            amounts: MoneySequence::new("amounts", &amounts),
            total: MoneyField::from_minor("total", row_total(self.amounts), self.currency),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct AccountJson<'a> {
    code: String,
    name: String,
    r#type: String,
    normal_balance: &'static str,
    #[serde(flatten)]
    values: PeriodValuesJson<'a>,
}

#[derive(Serialize)]
struct ReportJson<'a> {
    report: &'static str,
    mode: &'static str,
    period: &'static str,
    from: String,
    to: String,
    currency: String,
    columns: Vec<ColumnJson>,
    accounts: Vec<AccountJson<'a>>,
    totals: BTreeMap<&'static str, PeriodValuesJson<'a>>,
}

fn values_json(amounts: &[i64], currency: Currency) -> PeriodValuesJson<'_> {
    PeriodValuesJson { amounts, currency }
}

/// Render a period report inside the standard JSON envelope.
///
/// # Errors
///
/// Returns a validation error if totals exceed the supported minor-unit range,
/// or `CliError::General` if serialisation fails.
pub fn render_json(report: &MultiColumnReport, meta: Meta) -> Result<String, CliError> {
    validate_totals(report)?;
    envelope_ok(meta, report_json(report))
}

pub(crate) fn report_json(report: &MultiColumnReport) -> impl Serialize + '_ {
    let accounts = report
        .accounts
        .iter()
        .map(|a| AccountJson {
            code: a.code.clone(),
            name: a.name.clone(),
            r#type: a.account_type.code().to_owned(),
            normal_balance: a.account_type.normal_balance().code(),
            values: values_json(&a.amounts, report.currency),
        })
        .collect();

    let totals = report
        .summary
        .iter()
        .map(|line| (line.key, values_json(&line.amounts, report.currency)))
        .collect();

    ReportJson {
        report: report.kind.as_str(),
        mode: "periods",
        period: report.period.as_str(),
        from: report.from.to_string(),
        to: report.to.to_string(),
        currency: report.currency.code().into(),
        columns: report.columns.iter().map(ColumnJson::from).collect(),
        accounts,
        totals,
    }
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

fn csv_err(e: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {e}"))
}

/// Raw (machine-readable) value cells for one CSV row.
fn csv_values(amounts: &[i64]) -> Vec<String> {
    let mut cells: Vec<String> = amounts.iter().map(ToString::to_string).collect();
    cells.push(row_total(amounts).to_string());
    cells
}

/// Render a multi-column report as CSV.
///
/// Columns: `code`, `name`, `type`, one column per period label,
/// `total`. Amounts are integer minor units. Summary rows
/// (e.g. `net_income`) follow the account rows with an empty `code`, the
/// summary key in `name` and `type` set to `total`.
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_csv(report: &MultiColumnReport) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());

    let mut header: Vec<String> = vec!["code".into(), "name".into(), "type".into()];
    header.extend(report.columns.iter().map(|c| c.label.clone()));
    header.push("total".into());
    wtr.write_record(&header).map_err(|e| csv_err(&e))?;

    for line in &report.accounts {
        let mut record = vec![
            line.code.clone(),
            line.name.clone(),
            line.account_type.code().to_owned(),
        ];
        record.extend(csv_values(&line.amounts));
        wtr.write_record(&record).map_err(|e| csv_err(&e))?;
    }
    for line in &report.summary {
        let mut record = vec![String::new(), line.key.to_string(), "total".to_string()];
        record.extend(csv_values(&line.amounts));
        wtr.write_record(&record).map_err(|e| csv_err(&e))?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(current: i64, prior: i64, decimals: u8) -> Option<String> {
        change_percent(current, prior).map(|p| p.to_fixed(decimals))
    }

    #[test]
    fn percent_change() {
        assert_eq!(fixed(150, 100, 2).as_deref(), Some("50.00"));
        assert_eq!(fixed(50, 100, 2).as_deref(), Some("-50.00"));
        assert_eq!(fixed(-50, -100, 2).as_deref(), Some("50.00"));
        assert_eq!(fixed(100, 0, 2), None);
        assert_eq!(
            change_percent(150, 100).and_then(Percent::to_json_number),
            Some(50.0)
        );
    }

    #[test]
    fn percent_rounds_once_half_away_from_zero() {
        assert_eq!(fixed(4, 3, 2).as_deref(), Some("33.33"));
        assert_eq!(fixed(5, 3, 2).as_deref(), Some("66.67"));
        assert_eq!(fixed(2, 3, 1).as_deref(), Some("-33.3"));
        // -87.5% rounds away from zero.
        assert_eq!(fixed(1, 8, 0).as_deref(), Some("-88"));
        // 12.345% to one decimal is 12.3 (no double rounding via 12.35).
        assert_eq!(fixed(112_345, 100_000, 1).as_deref(), Some("12.3"));
        // A change that rounds to zero carries no sign.
        assert_eq!(fixed(99_999, 100_000, 1).as_deref(), Some("0.0"));
        // Extremes do not overflow.
        assert_eq!(
            fixed(i64::MAX, 1, 0).map(|s| s.len()),
            Some(i64::MAX.to_string().len() + 2)
        );
        assert_eq!(fixed(i64::MIN, i64::MAX, 2).as_deref(), Some("-200.00"));
    }

    #[test]
    fn period_values_project_signed_money_without_an_envelope() -> serde_json::Result<()> {
        let values = values_json(&[1234, 0, -25], Currency::BHD);
        assert_eq!(
            serde_json::to_value(values)?,
            serde_json::json!({
                "amounts": [1234, 0, -25],
                "amounts_display": ["1.234", "0.000", "-0.025"],
                "total": 1209,
                "total_display": "1.209",
            }),
        );
        Ok(())
    }
}
