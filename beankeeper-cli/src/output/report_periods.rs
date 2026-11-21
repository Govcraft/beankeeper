//! Table, JSON and CSV rendering for multi-column reports (issue #22).
//!
//! See [`crate::commands::report_periods`] for how the report is computed.
//! Two layouts are supported:
//!
//! - **Periods**: one amount column per period followed by a `Total` column.
//! - **Compare**: current and prior columns followed by `Change` and
//!   `Change %` (percent is `n/a` / `null` when the prior amount is zero).
//!
//! Amounts are net amounts on each account's normal side. JSON and CSV carry
//! integer minor units, like the single-column reports.

use std::collections::BTreeMap;

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use serde::Serialize;

use crate::commands::report_periods::{Column, Layout, MultiColumnReport, ReportColumns};
use crate::error::CliError;
use crate::output::json::{Meta, envelope_ok};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub use crate::percent::{Percent, change_percent};

/// Sum a row of amounts (the Total column of the periods layout).
fn row_total(amounts: &[i64]) -> i64 {
    amounts.iter().fold(0i64, |acc, a| acc.saturating_add(*a))
}

/// `(current, prior)` amounts of a compare-layout row.
fn current_prior(amounts: &[i64]) -> (i64, i64) {
    (
        amounts.first().copied().unwrap_or(0),
        amounts.get(1).copied().unwrap_or(0),
    )
}

fn normal_balance_for(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "debit",
        "liability" | "equity" | "revenue" => "credit",
        _ => "unknown",
    }
}

/// Format minor units with thousands separators and decimal places.
pub(crate) fn format_amount(minor_units: impl Into<i128>, decimal_places: u8) -> String {
    let minor_units = minor_units.into();
    let abs_units = minor_units.unsigned_abs();
    let divisor = 10u128.pow(u32::from(decimal_places));
    let whole = (abs_units / divisor).to_string();
    let frac = abs_units % divisor;

    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }

    let body = if decimal_places == 0 {
        grouped
    } else {
        format!(
            "{grouped}.{frac:0>width$}",
            width = usize::from(decimal_places)
        )
    };
    if minor_units < 0 {
        format!("-{body}")
    } else {
        body
    }
}

fn format_percent(pct: Option<Percent>) -> String {
    pct.map_or_else(|| "n/a".to_string(), |p| format!("{}%", p.to_fixed(1)))
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
    let mu = report.currency.minor_units();
    let mut cells: Vec<String> = amounts.iter().map(|a| format_amount(*a, mu)).collect();
    match report.layout() {
        Layout::Periods(_) => cells.push(format_amount(row_total(amounts), mu)),
        Layout::Compare(_) => {
            let (cur, prior) = current_prior(amounts);
            cells.push(format_amount(cur.saturating_sub(prior), mu));
            cells.push(format_percent(change_percent(cur, prior)));
        }
    }
    cells
}

/// Header labels for the amount columns.
fn value_headers(report: &MultiColumnReport) -> Vec<String> {
    let mut headers: Vec<String> = report.columns().iter().map(|c| c.label.clone()).collect();
    match report.layout() {
        Layout::Periods(_) => headers.push("Total".to_string()),
        Layout::Compare(_) => {
            headers.push("Change".to_string());
            headers.push("Change %".to_string());
        }
    }
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
    match report.report_columns() {
        ReportColumns::Periods(p, _) => format!(
            "{base} by {} ({} to {}) ({currency})",
            capitalize_first(p.as_str()),
            report.from,
            report.to
        ),
        ReportColumns::Compare(_, [_, prior]) => format!(
            "{base} ({} to {} vs. {} to {}) ({currency})",
            report.from, report.to, prior.from, prior.to,
        ),
    }
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
            Cell::new(capitalize_first(&line.account_type)),
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

#[derive(Serialize)]
struct PeriodValuesJson {
    amounts: Vec<i64>,
    total: i64,
}

#[derive(Serialize)]
struct CompareValuesJson {
    current: i64,
    prior: i64,
    change: i64,
    change_percent: Option<f64>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ValuesJson {
    Periods(PeriodValuesJson),
    Compare(CompareValuesJson),
}

#[derive(Serialize)]
struct AccountJson {
    code: String,
    name: String,
    r#type: String,
    normal_balance: &'static str,
    #[serde(flatten)]
    values: ValuesJson,
}

#[derive(Serialize)]
struct ReportJson {
    report: &'static str,
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare: Option<&'static str>,
    from: String,
    to: String,
    currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    columns: Option<Vec<ColumnJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<ColumnJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior: Option<ColumnJson>,
    accounts: Vec<AccountJson>,
    totals: BTreeMap<&'static str, ValuesJson>,
}

fn values_json(layout: Layout, amounts: &[i64]) -> ValuesJson {
    match layout {
        Layout::Periods(_) => ValuesJson::Periods(PeriodValuesJson {
            amounts: amounts.to_vec(),
            total: row_total(amounts),
        }),
        Layout::Compare(_) => {
            let (current, prior) = current_prior(amounts);
            ValuesJson::Compare(CompareValuesJson {
                current,
                prior,
                change: current.saturating_sub(prior),
                change_percent: change_percent(current, prior).and_then(Percent::to_json_number),
            })
        }
    }
}

/// Render a multi-column report inside the standard JSON envelope.
/// # Errors
/// Returns `CliError::General` if serialisation fails.
pub fn render_json(report: &MultiColumnReport, meta: Meta) -> Result<String, CliError> {
    envelope_ok(meta, report_json(report))
}

pub(crate) fn report_json(report: &MultiColumnReport) -> impl Serialize {
    let accounts = report
        .accounts
        .iter()
        .map(|a| AccountJson {
            code: a.code.clone(),
            name: a.name.clone(),
            r#type: a.account_type.clone(),
            normal_balance: normal_balance_for(&a.account_type),
            values: values_json(report.layout(), &a.amounts),
        })
        .collect();

    let totals = report
        .summary
        .iter()
        .map(|line| (line.key, values_json(report.layout(), &line.amounts)))
        .collect();

    let (mode, period, compare, columns, current, prior) = match report.report_columns() {
        ReportColumns::Periods(p, columns) => (
            "periods",
            Some(p.as_str()),
            None,
            Some(columns.iter().map(ColumnJson::from).collect()),
            None,
            None,
        ),
        ReportColumns::Compare(c, [current, prior]) => (
            "compare",
            None,
            Some(c.as_str()),
            None,
            Some(ColumnJson::from(current)),
            Some(ColumnJson::from(prior)),
        ),
    };

    ReportJson {
        report: report.kind.as_str(),
        mode,
        period,
        compare,
        from: report.from.to_string(),
        to: report.to.to_string(),
        currency: report.currency.code().into(),
        columns,
        current,
        prior,
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
fn csv_values(layout: Layout, amounts: &[i64]) -> Vec<String> {
    let mut cells: Vec<String> = amounts.iter().map(ToString::to_string).collect();
    match layout {
        Layout::Periods(_) => cells.push(row_total(amounts).to_string()),
        Layout::Compare(_) => {
            let (cur, prior) = current_prior(amounts);
            cells.push(cur.saturating_sub(prior).to_string());
            cells.push(
                change_percent(cur, prior).map_or_else(|| "n/a".to_string(), |p| p.to_fixed(2)),
            );
        }
    }
    cells
}

/// Render a multi-column report as CSV.
///
/// Periods columns: `code`, `name`, `type`, one column per period label,
/// `total`. Compare columns: `code`, `name`, `type`, `current`, `prior`,
/// `change`, `change_percent`. Amounts are integer minor units. Summary rows
/// (e.g. `net_income`) follow the account rows with an empty `code`, the
/// summary key in `name` and `type` set to `total`.
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_csv(report: &MultiColumnReport) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());

    let mut header: Vec<String> = vec!["code".into(), "name".into(), "type".into()];
    match report.layout() {
        Layout::Periods(_) => {
            header.extend(report.columns().iter().map(|c| c.label.clone()));
            header.push("total".into());
        }
        Layout::Compare(_) => {
            header.extend(["current", "prior", "change", "change_percent"].map(String::from));
        }
    }
    wtr.write_record(&header).map_err(|e| csv_err(&e))?;

    for line in &report.accounts {
        let mut record = vec![
            line.code.clone(),
            line.name.clone(),
            line.account_type.clone(),
        ];
        record.extend(csv_values(report.layout(), &line.amounts));
        wtr.write_record(&record).map_err(|e| csv_err(&e))?;
    }
    for line in &report.summary {
        let mut record = vec![String::new(), line.key.to_string(), "total".to_string()];
        record.extend(csv_values(report.layout(), &line.amounts));
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
    fn amounts_are_grouped() {
        assert_eq!(format_amount(123_456_789, 2), "1,234,567.89");
        assert_eq!(format_amount(-5, 2), "-0.05");
        assert_eq!(format_amount(1000, 0), "1,000");
    }
}
