//! Financial statements: the income statement and the balance sheet.
//!
//! Unlike the trial balance (a flat list of debit/credit totals), these
//! reports group accounts into sections, show one signed amount per account
//! on its normal-balance side, and carry subtotals plus a bottom line (net
//! income, or the accounting-equation check).
//!
//! The balance sheet includes a computed equity line,
//! [`COMPUTED_EARNINGS_NAME`], equal to cumulative revenue minus expenses up
//! to the as-of date. Beankeeper has no closing entries, so without this line
//! any revenue or expense activity would leave the sheet unbalanced.
//!
//! The statement is built once ([`build_income_statement`],
//! [`build_balance_sheet`]) and then rendered as a table, JSON, or CSV so all
//! three formats carry the same structure.

use comfy_table::{Cell, CellAlignment};
use serde::Serialize;

use super::json::{Meta, envelope_ok};
use super::table::{
    bold_style, cyan_style, format_amount, green_style, new_table, red_bold_style, styled,
};
use crate::db::BalanceRow;
use crate::error::CliError;

/// Display name of the computed retained-earnings line on the balance sheet.
pub const COMPUTED_EARNINGS_NAME: &str = "Retained Earnings (computed)";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A single line of a statement section.
#[derive(Debug, Clone, Serialize)]
pub struct StatementLine {
    /// Account code, or `None` for a computed line.
    pub code: Option<String>,
    /// Account (or computed line) name.
    pub name: String,
    /// Lowercase account type (e.g. `"asset"`).
    #[serde(rename = "type")]
    pub account_type: String,
    /// Signed balance on the account's normal side, in minor units.
    pub amount: i64,
    /// `true` for lines derived by the report rather than read from an account.
    pub computed: bool,
}

/// A titled group of statement lines with a subtotal.
#[derive(Debug, Clone, Serialize)]
pub struct StatementSection {
    /// Stable machine key (`"revenue"`, `"expenses"`, `"assets"`, ...).
    pub key: &'static str,
    /// Human-readable heading.
    pub title: &'static str,
    /// Normal-balance side of the accounts in this section.
    pub normal_balance: &'static str,
    /// Lines in this section.
    pub accounts: Vec<StatementLine>,
    /// Sum of `amount` over all lines.
    pub total: i64,
}

impl StatementSection {
    fn new(
        key: &'static str,
        title: &'static str,
        normal_balance: &'static str,
        accounts: Vec<StatementLine>,
    ) -> Self {
        let total = accounts
            .iter()
            .fold(0i64, |acc, l| acc.saturating_add(l.amount));
        Self {
            key,
            title,
            normal_balance,
            accounts,
            total,
        }
    }

    /// Label used for this section's subtotal row.
    fn total_label(&self) -> String {
        format!("Total {}", self.title)
    }
}

/// An income statement for a period.
#[derive(Debug, Clone)]
pub struct IncomeStatement {
    /// Inclusive start date of the period, if any.
    pub from: Option<String>,
    /// Inclusive end date of the period, if any.
    pub to: Option<String>,
    /// Revenue accounts (credit-normal).
    pub revenue: StatementSection,
    /// Expense accounts (debit-normal).
    pub expenses: StatementSection,
    /// Total revenue minus total expenses; negative means a net loss.
    pub net_income: i64,
}

/// A balance sheet as of a date.
#[derive(Debug, Clone)]
pub struct BalanceSheet {
    /// Inclusive as-of date, if any.
    pub as_of: Option<String>,
    /// Asset accounts.
    pub assets: StatementSection,
    /// Liability accounts.
    pub liabilities: StatementSection,
    /// Equity accounts plus the computed retained-earnings line.
    pub equity: StatementSection,
    /// Cumulative revenue minus expenses up to `as_of`.
    pub computed_earnings: i64,
    /// Total liabilities plus total equity.
    pub total_liabilities_and_equity: i64,
}

impl BalanceSheet {
    /// Total assets minus total liabilities and equity (zero when balanced).
    #[must_use]
    pub fn difference(&self) -> i64 {
        self.assets
            .total
            .saturating_sub(self.total_liabilities_and_equity)
    }

    /// Whether Assets = Liabilities + Equity.
    #[must_use]
    pub fn balanced(&self) -> bool {
        self.difference() == 0
    }
}

/// Normal-balance side for a lowercase account type.
fn normal_side(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "debit",
        _ => "credit",
    }
}

/// Signed balance of a row on its account type's normal side.
fn signed_amount(row: &BalanceRow) -> i64 {
    if normal_side(&row.account_type) == "debit" {
        row.debit_total.saturating_sub(row.credit_total)
    } else {
        row.credit_total.saturating_sub(row.debit_total)
    }
}

fn account_lines(rows: &[BalanceRow]) -> Vec<StatementLine> {
    rows.iter()
        .map(|r| StatementLine {
            code: Some(r.code.clone()),
            name: r.name.clone(),
            account_type: r.account_type.clone(),
            amount: signed_amount(r),
            computed: false,
        })
        .collect()
}

fn sum_signed(rows: &[BalanceRow]) -> i64 {
    rows.iter()
        .fold(0i64, |acc, r| acc.saturating_add(signed_amount(r)))
}

/// Build an income statement from revenue and expense balances for a period.
#[must_use]
pub fn build_income_statement(
    from: Option<&str>,
    to: Option<&str>,
    revenue: &[BalanceRow],
    expenses: &[BalanceRow],
) -> IncomeStatement {
    let revenue = StatementSection::new("revenue", "Revenue", "credit", account_lines(revenue));
    let expenses = StatementSection::new("expenses", "Expenses", "debit", account_lines(expenses));
    let net_income = revenue.total.saturating_sub(expenses.total);
    IncomeStatement {
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        revenue,
        expenses,
        net_income,
    }
}

/// Inputs for [`build_balance_sheet`]: cumulative balances up to the as-of date.
pub struct BalanceSheetInputs<'a> {
    /// Inclusive as-of date, if any.
    pub as_of: Option<&'a str>,
    /// Asset account balances.
    pub assets: &'a [BalanceRow],
    /// Liability account balances.
    pub liabilities: &'a [BalanceRow],
    /// Equity account balances.
    pub equity: &'a [BalanceRow],
    /// Revenue account balances (used for computed retained earnings).
    pub revenue: &'a [BalanceRow],
    /// Expense account balances (used for computed retained earnings).
    pub expenses: &'a [BalanceRow],
}

/// Build a balance sheet, adding a computed retained-earnings equity line equal
/// to cumulative revenue minus expenses.
#[must_use]
pub fn build_balance_sheet(inputs: &BalanceSheetInputs<'_>) -> BalanceSheet {
    let computed_earnings = sum_signed(inputs.revenue).saturating_sub(sum_signed(inputs.expenses));

    let mut equity_lines = account_lines(inputs.equity);
    equity_lines.push(StatementLine {
        code: None,
        name: COMPUTED_EARNINGS_NAME.to_string(),
        account_type: "equity".to_string(),
        amount: computed_earnings,
        computed: true,
    });

    let assets = StatementSection::new("assets", "Assets", "debit", account_lines(inputs.assets));
    let liabilities = StatementSection::new(
        "liabilities",
        "Liabilities",
        "credit",
        account_lines(inputs.liabilities),
    );
    let equity = StatementSection::new("equity", "Equity", "credit", equity_lines);
    let total_liabilities_and_equity = liabilities.total.saturating_add(equity.total);

    BalanceSheet {
        as_of: inputs.as_of.map(str::to_string),
        assets,
        liabilities,
        equity,
        computed_earnings,
        total_liabilities_and_equity,
    }
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

fn push_section_rows(
    table: &mut comfy_table::Table,
    section: &StatementSection,
    minor_units: u8,
    use_color: bool,
) {
    table.add_row(vec![
        Cell::new(""),
        Cell::new(styled(section.title, bold_style(), use_color)),
        Cell::new(""),
    ]);
    for line in &section.accounts {
        let code = line.code.as_deref().unwrap_or("");
        table.add_row(vec![
            Cell::new(styled(code, cyan_style(), use_color)),
            Cell::new(format!("  {}", line.name)),
            Cell::new(format_amount(line.amount, minor_units)).set_alignment(CellAlignment::Right),
        ]);
    }
    push_total_row(
        table,
        &section.total_label(),
        section.total,
        minor_units,
        use_color,
    );
}

fn push_total_row(
    table: &mut comfy_table::Table,
    label: &str,
    amount: i64,
    minor_units: u8,
    use_color: bool,
) {
    table.add_row(vec![
        Cell::new(""),
        Cell::new(styled(label, bold_style(), use_color)),
        Cell::new(styled(
            &format_amount(amount, minor_units),
            bold_style(),
            use_color,
        ))
        .set_alignment(CellAlignment::Right),
    ]);
}

fn new_statement_table(use_color: bool) -> comfy_table::Table {
    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Account", bold_style(), use_color)),
        Cell::new(styled("Amount", bold_style(), use_color)),
    ]);
    table
}

/// Render an income statement as a sectioned table followed by net income.
#[must_use]
pub fn render_income_statement_table(
    stmt: &IncomeStatement,
    title: &str,
    minor_units: u8,
    use_color: bool,
) -> String {
    let mut table = new_statement_table(use_color);
    push_section_rows(&mut table, &stmt.revenue, minor_units, use_color);
    push_section_rows(&mut table, &stmt.expenses, minor_units, use_color);

    let net_str = format_amount(stmt.net_income.saturating_abs(), minor_units);
    let bottom = if stmt.net_income >= 0 {
        styled(&format!("Net Income: {net_str}"), green_style(), use_color)
    } else {
        styled(&format!("Net Loss: {net_str}"), red_bold_style(), use_color)
    };

    [
        styled(title, bold_style(), use_color),
        String::new(),
        table.to_string(),
        String::new(),
        bottom,
    ]
    .join("\n")
}

/// Render a balance sheet as a sectioned table followed by the balance check.
#[must_use]
pub fn render_balance_sheet_table(
    sheet: &BalanceSheet,
    title: &str,
    minor_units: u8,
    use_color: bool,
) -> String {
    let mut table = new_statement_table(use_color);
    push_section_rows(&mut table, &sheet.assets, minor_units, use_color);
    push_section_rows(&mut table, &sheet.liabilities, minor_units, use_color);
    push_section_rows(&mut table, &sheet.equity, minor_units, use_color);
    push_total_row(
        &mut table,
        "Total Liabilities + Equity",
        sheet.total_liabilities_and_equity,
        minor_units,
        use_color,
    );

    let status = if sheet.balanced() {
        styled(
            "[ok] BALANCED (Assets = Liabilities + Equity)",
            green_style(),
            use_color,
        )
    } else {
        let diff_str = format_amount(sheet.difference().saturating_abs(), minor_units);
        styled(
            &format!("[!!] UNBALANCED (difference: {diff_str})"),
            red_bold_style(),
            use_color,
        )
    };

    [
        styled(title, bold_style(), use_color),
        String::new(),
        table.to_string(),
        String::new(),
        status,
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IncomeStatementJson<'a> {
    from: Option<&'a str>,
    to: Option<&'a str>,
    sections: [&'a StatementSection; 2],
    total_revenue: i64,
    total_expenses: i64,
    net_income: i64,
}

#[derive(Serialize)]
struct BalanceSheetJson<'a> {
    as_of: Option<&'a str>,
    sections: [&'a StatementSection; 3],
    total_assets: i64,
    total_liabilities: i64,
    total_equity: i64,
    computed_earnings: i64,
    total_liabilities_and_equity: i64,
    difference: i64,
    balanced: bool,
}

/// Render an income statement as an enveloped JSON response.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_income_statement_json(
    stmt: &IncomeStatement,
    meta: Meta,
) -> Result<String, CliError> {
    envelope_ok(meta, income_statement_json(stmt))
}

pub(crate) fn income_statement_json(stmt: &IncomeStatement) -> impl Serialize + '_ {
    IncomeStatementJson {
        from: stmt.from.as_deref(),
        to: stmt.to.as_deref(),
        sections: [&stmt.revenue, &stmt.expenses],
        total_revenue: stmt.revenue.total,
        total_expenses: stmt.expenses.total,
        net_income: stmt.net_income,
    }
}

/// Render a balance sheet as an enveloped JSON response.
///
/// # Errors
///
/// Returns `CliError::General` if JSON serialisation fails.
pub fn render_balance_sheet_json(sheet: &BalanceSheet, meta: Meta) -> Result<String, CliError> {
    envelope_ok(
        meta,
        BalanceSheetJson {
            as_of: sheet.as_of.as_deref(),
            sections: [&sheet.assets, &sheet.liabilities, &sheet.equity],
            total_assets: sheet.assets.total,
            total_liabilities: sheet.liabilities.total,
            total_equity: sheet.equity.total,
            computed_earnings: sheet.computed_earnings,
            total_liabilities_and_equity: sheet.total_liabilities_and_equity,
            difference: sheet.difference(),
            balanced: sheet.balanced(),
        },
    )
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// CSV header shared by both statements.
const CSV_HEADER: [&str; 6] = ["section", "line_type", "code", "name", "type", "amount"];

struct StatementCsv {
    wtr: csv::Writer<Vec<u8>>,
}

impl StatementCsv {
    fn new() -> Result<Self, CliError> {
        let mut wtr = csv::Writer::from_writer(Vec::new());
        wtr.write_record(CSV_HEADER).map_err(|e| csv_err(&e))?;
        Ok(Self { wtr })
    }

    /// Write one record in [`CSV_HEADER`] column order.
    fn row(&mut self, record: [&str; 6]) -> Result<(), CliError> {
        self.wtr.write_record(record).map_err(|e| csv_err(&e))
    }

    /// Write a `summary` row (no code or account type).
    fn summary(&mut self, line_type: &str, name: &str, amount: &str) -> Result<(), CliError> {
        self.row(["summary", line_type, "", name, "", amount])
    }

    fn section(&mut self, section: &StatementSection) -> Result<(), CliError> {
        for line in &section.accounts {
            let line_type = if line.computed { "computed" } else { "account" };
            self.row([
                section.key,
                line_type,
                line.code.as_deref().unwrap_or(""),
                &line.name,
                &line.account_type,
                &line.amount.to_string(),
            ])?;
        }
        self.row([
            section.key,
            "subtotal",
            "",
            &section.total_label(),
            "",
            &section.total.to_string(),
        ])
    }

    fn finish(self) -> Result<String, CliError> {
        let bytes = self
            .wtr
            .into_inner()
            .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
        String::from_utf8(bytes)
            .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
    }
}

fn csv_err(e: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {e}"))
}

/// Render an income statement as CSV.
///
/// Columns: `section`, `line_type`, `code`, `name`, `type`, `amount`.
/// `line_type` is `account` or `subtotal` within a section; the final
/// `summary` row has `line_type` `net_income`.
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_income_statement_csv(stmt: &IncomeStatement) -> Result<String, CliError> {
    let mut out = StatementCsv::new()?;
    out.section(&stmt.revenue)?;
    out.section(&stmt.expenses)?;
    let label = if stmt.net_income >= 0 {
        "Net Income"
    } else {
        "Net Loss"
    };
    out.summary("net_income", label, &stmt.net_income.to_string())?;
    out.finish()
}

/// Render a balance sheet as CSV.
///
/// Columns: `section`, `line_type`, `code`, `name`, `type`, `amount`.
/// `line_type` is `account`, `computed` (the retained-earnings line) or
/// `subtotal` within a section; `summary` rows carry `total`
/// (liabilities + equity), `difference` and `balanced` (`true`/`false`).
///
/// # Errors
///
/// Returns `CliError::General` if CSV serialisation fails.
pub fn render_balance_sheet_csv(sheet: &BalanceSheet) -> Result<String, CliError> {
    let mut out = StatementCsv::new()?;
    out.section(&sheet.assets)?;
    out.section(&sheet.liabilities)?;
    out.section(&sheet.equity)?;
    out.summary(
        "total",
        "Total Liabilities + Equity",
        &sheet.total_liabilities_and_equity.to_string(),
    )?;
    out.summary("difference", "Difference", &sheet.difference().to_string())?;
    out.summary(
        "balanced",
        "Balanced",
        if sheet.balanced() { "true" } else { "false" },
    )?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(code: &str, account_type: &str, debit: i64, credit: i64) -> BalanceRow {
        BalanceRow {
            code: code.to_string(),
            name: format!("Account {code}"),
            account_type: account_type.to_string(),
            debit_total: debit,
            credit_total: credit,
        }
    }

    #[test]
    fn income_statement_signs_amounts_on_normal_side() {
        let stmt = build_income_statement(
            None,
            None,
            &[row("4000", "revenue", 0, 10_000)],
            &[row("5000", "expense", 2_500, 0)],
        );
        assert_eq!(stmt.revenue.total, 10_000);
        assert_eq!(stmt.expenses.total, 2_500);
        assert_eq!(stmt.net_income, 7_500);
    }

    #[test]
    fn balance_sheet_balances_with_computed_earnings() {
        let sheet = build_balance_sheet(&BalanceSheetInputs {
            as_of: None,
            assets: &[row("1000", "asset", 10_000, 2_500)],
            liabilities: &[],
            equity: &[],
            revenue: &[row("4000", "revenue", 0, 10_000)],
            expenses: &[row("5000", "expense", 2_500, 0)],
        });
        assert_eq!(sheet.computed_earnings, 7_500);
        assert_eq!(sheet.equity.total, 7_500);
        assert!(sheet.balanced());
        assert!(sheet.equity.accounts.iter().any(|l| l.computed));
    }

    #[test]
    fn net_loss_is_negative() {
        let stmt = build_income_statement(None, None, &[], &[row("5000", "expense", 100, 0)]);
        assert_eq!(stmt.net_income, -100);
        let csv = render_income_statement_csv(&stmt).unwrap_or_default();
        assert!(csv.contains("summary,net_income,,Net Loss,,-100"));
    }
}
