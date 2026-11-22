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
//! Compute the statements through [`compute_income_statement`] and
//! [`compute_balance_sheet`], then render the same typed result as a table,
//! JSON, or CSV.
//!
//! [`compute_income_statement`]: crate::commands::report::compute_income_statement
//! [`compute_balance_sheet`]: crate::commands::report::compute_balance_sheet

use beankeeper::types::{AccountType, Amount, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use comfy_table::{Cell, CellAlignment};
use serde::{Serialize, Serializer};

use super::amounts::{MoneyField, format_money_grouped};
use super::json::{Meta, envelope_ok};
use super::table::{bold_style, cyan_style, green_style, new_table, red_bold_style, styled};
use crate::commands::report::DateRange;
use crate::db::BalanceRow;
use crate::error::CliError;

/// Display name of the computed retained-earnings line on the balance sheet.
pub const COMPUTED_EARNINGS_NAME: &str = "Retained Earnings (computed)";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A single line of a statement section.
#[derive(Debug, Clone)]
pub struct StatementLine {
    /// Account code, or `None` for the computed earnings line.
    code: Option<String>,
    /// Account or computed line name.
    name: String,
    /// Type of the represented account.
    account_type: AccountType,
    /// Signed balance on the normal side.
    amount: Money,
}

impl StatementLine {
    /// Account code for a ledger row.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Account or computed line name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Type of the represented account.
    #[must_use]
    pub const fn account_type(&self) -> AccountType {
        self.account_type
    }

    /// Signed account balance in the report currency.
    #[must_use]
    pub const fn amount(&self) -> Money {
        self.amount
    }

    /// Whether the line was computed by the report.
    #[must_use]
    pub const fn is_computed(&self) -> bool {
        self.code.is_none()
    }
}

#[derive(Debug, Clone, Copy)]
enum SectionKind {
    Assets,
    Liabilities,
    Equity,
    Revenue,
    Expenses,
}

impl SectionKind {
    const fn account_type(self) -> AccountType {
        match self {
            Self::Assets => AccountType::Asset,
            Self::Liabilities => AccountType::Liability,
            Self::Equity => AccountType::Equity,
            Self::Revenue => AccountType::Revenue,
            Self::Expenses => AccountType::Expense,
        }
    }

    const fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Assets => ("assets", "Assets"),
            Self::Liabilities => ("liabilities", "Liabilities"),
            Self::Equity => ("equity", "Equity"),
            Self::Revenue => ("revenue", "Revenue"),
            Self::Expenses => ("expenses", "Expenses"),
        }
    }
}

/// Accounts and subtotal for one statement section.
#[derive(Debug, Clone)]
pub struct StatementSection {
    kind: SectionKind,
    /// Lines in this section.
    accounts: Vec<StatementLine>,
    /// Subtotal in the statement currency.
    total: Money,
}

impl StatementSection {
    fn new(
        kind: SectionKind,
        currency: Currency,
        accounts: Vec<StatementLine>,
    ) -> Result<Self, CliError> {
        let total = sum_amounts(accounts.iter().map(|line| line.amount.amount()))?;
        Ok(Self {
            kind,
            accounts,
            total: report_money(total, currency)?,
        })
    }

    fn from_rows(
        kind: SectionKind,
        currency: Currency,
        rows: Vec<BalanceRow>,
    ) -> Result<Self, CliError> {
        Self::new(
            kind,
            currency,
            account_lines(kind.account_type(), currency, rows)?,
        )
    }

    /// Account type represented by this section.
    #[must_use]
    pub const fn kind(&self) -> AccountType {
        self.kind.account_type()
    }

    /// Stable key used in JSON and CSV.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.kind.labels().0
    }

    /// Heading suitable for presenting the section.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        self.kind.labels().1
    }

    /// Ledgers and computed lines in this section.
    #[must_use]
    pub fn accounts(&self) -> &[StatementLine] {
        &self.accounts
    }

    /// Sum of this section's lines.
    #[must_use]
    pub const fn total(&self) -> Money {
        self.total
    }

    /// Direction that increases the section's balance.
    #[must_use]
    pub const fn normal_balance(&self) -> DebitOrCredit {
        self.kind().normal_balance()
    }

    fn total_label(&self) -> String {
        format!("Total {}", self.title())
    }
}

/// An income statement for one calendar range and currency.
#[derive(Debug, Clone)]
pub struct IncomeStatement {
    dates: DateRange,
    currency: Currency,
    /// Revenue accounts on their normal side.
    revenue: StatementSection,
    /// Expense accounts on their normal side.
    expenses: StatementSection,
    /// Total revenue minus total expenses, negative for a net loss.
    net_income: Money,
}

impl IncomeStatement {
    /// Revenue accounts on their normal side.
    #[must_use]
    pub const fn revenue(&self) -> &StatementSection {
        &self.revenue
    }

    /// Expense accounts on their normal side.
    #[must_use]
    pub const fn expenses(&self) -> &StatementSection {
        &self.expenses
    }

    /// Total revenue minus total expenses.
    #[must_use]
    pub const fn net_income(&self) -> Money {
        self.net_income
    }

    /// Inclusive bounds of the statement.
    #[must_use]
    pub const fn dates(&self) -> DateRange {
        self.dates
    }

    /// Currency of the statement and its lines.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }
}

/// A balance sheet as of one date and in one currency.
#[derive(Debug, Clone)]
pub struct BalanceSheet {
    /// Latest date included.
    as_of: NaiveDate,
    currency: Currency,
    /// Asset accounts.
    assets: StatementSection,
    /// Liability accounts.
    liabilities: StatementSection,
    /// Equity accounts and the computed retained earnings line.
    equity: StatementSection,
    /// Cumulative revenue minus expenses through the report date.
    computed_earnings: Money,
    /// Total liabilities plus total equity.
    total_liabilities_and_equity: Money,
    difference: Money,
}

impl BalanceSheet {
    /// Latest date included.
    #[must_use]
    pub const fn as_of(&self) -> NaiveDate {
        self.as_of
    }

    /// Asset accounts.
    #[must_use]
    pub const fn assets(&self) -> &StatementSection {
        &self.assets
    }

    /// Liability accounts.
    #[must_use]
    pub const fn liabilities(&self) -> &StatementSection {
        &self.liabilities
    }

    /// Equity accounts and the computed retained earnings line.
    #[must_use]
    pub const fn equity(&self) -> &StatementSection {
        &self.equity
    }

    /// Cumulative revenue less expenses.
    #[must_use]
    pub const fn computed_earnings(&self) -> Money {
        self.computed_earnings
    }

    /// Total liabilities plus total equity.
    #[must_use]
    pub const fn total_liabilities_and_equity(&self) -> Money {
        self.total_liabilities_and_equity
    }

    /// Currency of the statement and its lines.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Total assets minus total liabilities and equity.
    #[must_use]
    pub const fn difference(&self) -> Money {
        self.difference
    }

    /// Whether Assets = Liabilities + Equity.
    #[must_use]
    pub fn balanced(&self) -> bool {
        self.difference.amount().minor_units() == 0
    }
}

fn overflow() -> CliError {
    CliError::Validation("financial statement exceeds the supported minor-unit range".into())
}

fn report_money(amount: Amount, currency: Currency) -> Result<Money, CliError> {
    i64::try_from(amount.minor_units()).map_err(|_| overflow())?;
    Ok(Money::new(amount, currency))
}

fn signed_amount(kind: AccountType, row: &BalanceRow) -> Amount {
    let debit = i128::from(row.debit_total);
    let credit = i128::from(row.credit_total);
    Amount::new(if kind.is_debit_normal() {
        debit - credit
    } else {
        credit - debit
    })
}

fn sum_amounts(amounts: impl IntoIterator<Item = Amount>) -> Result<Amount, CliError> {
    amounts
        .into_iter()
        .try_fold(Amount::new(0), |total, amount| {
            total.checked_add(amount).ok_or_else(overflow)
        })
}

fn account_lines(
    kind: AccountType,
    currency: Currency,
    rows: Vec<BalanceRow>,
) -> Result<Vec<StatementLine>, CliError> {
    rows.into_iter()
        .map(|row| {
            let amount = report_money(signed_amount(kind, &row), currency)?;
            Ok(StatementLine {
                code: Some(row.code),
                name: row.name,
                account_type: kind,
                amount,
            })
        })
        .collect()
}

pub(crate) fn build_income_statement(
    dates: DateRange,
    currency: Currency,
    revenue: Vec<BalanceRow>,
    expenses: Vec<BalanceRow>,
) -> Result<IncomeStatement, CliError> {
    let revenue = StatementSection::from_rows(SectionKind::Revenue, currency, revenue)?;
    let expenses = StatementSection::from_rows(SectionKind::Expenses, currency, expenses)?;
    let net_income = revenue
        .total
        .amount()
        .checked_sub(expenses.total.amount())
        .ok_or_else(overflow)?;
    Ok(IncomeStatement {
        dates,
        currency,
        revenue,
        expenses,
        net_income: report_money(net_income, currency)?,
    })
}

pub(crate) struct BalanceSheetInputs {
    pub as_of: NaiveDate,
    pub currency: Currency,
    pub assets: Vec<BalanceRow>,
    pub liabilities: Vec<BalanceRow>,
    pub equity: Vec<BalanceRow>,
    pub revenue: Vec<BalanceRow>,
    pub expenses: Vec<BalanceRow>,
}

pub(crate) fn build_balance_sheet(inputs: BalanceSheetInputs) -> Result<BalanceSheet, CliError> {
    let currency = inputs.currency;
    let computed_earnings = sum_amounts(
        inputs
            .revenue
            .iter()
            .map(|row| signed_amount(AccountType::Revenue, row)),
    )?
    .checked_sub(sum_amounts(
        inputs
            .expenses
            .iter()
            .map(|row| signed_amount(AccountType::Expense, row)),
    )?)
    .ok_or_else(overflow)?;
    let computed_earnings = report_money(computed_earnings, currency)?;

    let mut equity_lines = account_lines(AccountType::Equity, currency, inputs.equity)?;
    equity_lines.push(StatementLine {
        code: None,
        name: COMPUTED_EARNINGS_NAME.into(),
        account_type: AccountType::Equity,
        amount: computed_earnings,
    });

    let assets = StatementSection::from_rows(SectionKind::Assets, currency, inputs.assets)?;
    let liabilities =
        StatementSection::from_rows(SectionKind::Liabilities, currency, inputs.liabilities)?;
    let equity = StatementSection::new(SectionKind::Equity, currency, equity_lines)?;
    let total_liabilities_and_equity = liabilities
        .total
        .amount()
        .checked_add(equity.total.amount())
        .ok_or_else(overflow)?;
    let difference = assets
        .total
        .amount()
        .checked_sub(total_liabilities_and_equity)
        .ok_or_else(overflow)?;

    Ok(BalanceSheet {
        as_of: inputs.as_of,
        currency,
        assets,
        liabilities,
        equity,
        computed_earnings,
        total_liabilities_and_equity: report_money(total_liabilities_and_equity, currency)?,
        difference: report_money(difference, currency)?,
    })
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

fn push_section_rows(table: &mut comfy_table::Table, section: &StatementSection, use_color: bool) {
    table.add_row(vec![
        Cell::new(""),
        Cell::new(styled(section.title(), bold_style(), use_color)),
        Cell::new(""),
    ]);
    for line in &section.accounts {
        let code = line.code.as_deref().unwrap_or("");
        table.add_row(vec![
            Cell::new(styled(code, cyan_style(), use_color)),
            Cell::new(format!("  {}", line.name)),
            Cell::new(format_money_grouped(line.amount)).set_alignment(CellAlignment::Right),
        ]);
    }
    push_total_row(table, &section.total_label(), section.total, use_color);
}

fn push_total_row(table: &mut comfy_table::Table, label: &str, amount: Money, use_color: bool) {
    table.add_row(vec![
        Cell::new(""),
        Cell::new(styled(label, bold_style(), use_color)),
        Cell::new(styled(
            &format_money_grouped(amount),
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
    use_color: bool,
) -> String {
    let mut table = new_statement_table(use_color);
    push_section_rows(&mut table, &stmt.revenue, use_color);
    push_section_rows(&mut table, &stmt.expenses, use_color);

    let net = stmt.net_income.amount().minor_units();
    let net_str = format_money_grouped(Money::from_minor(net.abs(), stmt.currency()));
    let bottom = if net >= 0 {
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
pub fn render_balance_sheet_table(sheet: &BalanceSheet, title: &str, use_color: bool) -> String {
    let mut table = new_statement_table(use_color);
    push_section_rows(&mut table, &sheet.assets, use_color);
    push_section_rows(&mut table, &sheet.liabilities, use_color);
    push_section_rows(&mut table, &sheet.equity, use_color);
    push_total_row(
        &mut table,
        "Total Liabilities + Equity",
        sheet.total_liabilities_and_equity,
        use_color,
    );

    let status = if sheet.balanced() {
        styled(
            "[ok] BALANCED (Assets = Liabilities + Equity)",
            green_style(),
            use_color,
        )
    } else {
        let difference = sheet.difference().amount().minor_units().abs();
        let diff_str = format_money_grouped(Money::from_minor(difference, sheet.currency()));
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

impl Serialize for StatementLine {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Line<'a> {
            code: Option<&'a str>,
            name: &'a str,
            #[serde(rename = "type")]
            account_type: &'static str,
            #[serde(flatten)]
            amount: MoneyField,
            computed: bool,
        }

        Line {
            code: self.code(),
            name: self.name(),
            account_type: self.account_type().code(),
            amount: MoneyField::new("amount", self.amount()),
            computed: self.is_computed(),
        }
        .serialize(serializer)
    }
}

impl Serialize for StatementSection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Section<'a> {
            key: &'static str,
            title: &'static str,
            normal_balance: &'static str,
            accounts: &'a [StatementLine],
            #[serde(flatten)]
            total: MoneyField,
        }

        Section {
            key: self.key(),
            title: self.title(),
            normal_balance: self.normal_balance().code(),
            accounts: self.accounts(),
            total: MoneyField::new("total", self.total()),
        }
        .serialize(serializer)
    }
}

impl Serialize for IncomeStatement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(flatten)]
            dates: DateRange,
            sections: [&'a StatementSection; 2],
            #[serde(flatten)]
            total_revenue: MoneyField,
            #[serde(flatten)]
            total_expenses: MoneyField,
            #[serde(flatten)]
            net_income: MoneyField,
        }

        Data {
            dates: self.dates(),
            sections: [self.revenue(), self.expenses()],
            total_revenue: MoneyField::new("total_revenue", self.revenue().total()),
            total_expenses: MoneyField::new("total_expenses", self.expenses().total()),
            net_income: MoneyField::new("net_income", self.net_income()),
        }
        .serialize(serializer)
    }
}

impl Serialize for BalanceSheet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Data<'a> {
            as_of: NaiveDate,
            sections: [&'a StatementSection; 3],
            #[serde(flatten)]
            total_assets: MoneyField,
            #[serde(flatten)]
            total_liabilities: MoneyField,
            #[serde(flatten)]
            total_equity: MoneyField,
            #[serde(flatten)]
            computed_earnings: MoneyField,
            #[serde(flatten)]
            total_liabilities_and_equity: MoneyField,
            #[serde(flatten)]
            difference: MoneyField,
            balanced: bool,
        }

        Data {
            as_of: self.as_of(),
            sections: [self.assets(), self.liabilities(), self.equity()],
            total_assets: MoneyField::new("total_assets", self.assets().total()),
            total_liabilities: MoneyField::new("total_liabilities", self.liabilities().total()),
            total_equity: MoneyField::new("total_equity", self.equity().total()),
            computed_earnings: MoneyField::new("computed_earnings", self.computed_earnings()),
            total_liabilities_and_equity: MoneyField::new(
                "total_liabilities_and_equity",
                self.total_liabilities_and_equity(),
            ),
            difference: MoneyField::new("difference", self.difference()),
            balanced: self.balanced(),
        }
        .serialize(serializer)
    }
}

/// Render an income statement as an enveloped JSON response.
///
/// # Errors
/// Returns [`CliError`] if JSON serialization fails.
pub fn render_income_statement_json(
    stmt: &IncomeStatement,
    meta: Meta,
) -> Result<String, CliError> {
    envelope_ok(meta, stmt)
}

/// Render a balance sheet as an enveloped JSON response.
///
/// # Errors
/// Returns [`CliError`] if JSON serialization fails.
pub fn render_balance_sheet_json(sheet: &BalanceSheet, meta: Meta) -> Result<String, CliError> {
    envelope_ok(meta, sheet)
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
            let line_type = if line.is_computed() {
                "computed"
            } else {
                "account"
            };
            self.row([
                section.key(),
                line_type,
                line.code.as_deref().unwrap_or(""),
                &line.name,
                line.account_type.code(),
                &line.amount.amount().minor_units().to_string(),
            ])?;
        }
        self.row([
            section.key(),
            "subtotal",
            "",
            &section.total_label(),
            "",
            &section.total.amount().minor_units().to_string(),
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
    let label = if stmt.net_income.amount().minor_units() >= 0 {
        "Net Income"
    } else {
        "Net Loss"
    };
    out.summary(
        "net_income",
        label,
        &stmt.net_income.amount().minor_units().to_string(),
    )?;
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
        &sheet
            .total_liabilities_and_equity
            .amount()
            .minor_units()
            .to_string(),
    )?;
    out.summary(
        "difference",
        "Difference",
        &sheet.difference().amount().minor_units().to_string(),
    )?;
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

    fn row(
        code: &str,
        account_type: beankeeper::types::AccountType,
        debit: i64,
        credit: i64,
    ) -> BalanceRow {
        BalanceRow {
            code: code.to_string(),
            name: format!("Account {code}"),
            account_type,
            debit_total: debit,
            credit_total: credit,
        }
    }

    fn dates() -> Result<DateRange, CliError> {
        DateRange::new(NaiveDate::default(), NaiveDate::default())
    }

    #[test]
    fn income_statement_signs_amounts_on_normal_side() -> Result<(), CliError> {
        let stmt = build_income_statement(
            dates()?,
            Currency::USD,
            vec![row(
                "4000",
                beankeeper::types::AccountType::Revenue,
                0,
                10_000,
            )],
            vec![row(
                "5000",
                beankeeper::types::AccountType::Expense,
                2_500,
                0,
            )],
        )?;
        assert_eq!(stmt.revenue().total(), Money::usd(10_000));
        assert_eq!(stmt.expenses().total(), Money::usd(2_500));
        assert_eq!(stmt.net_income(), Money::usd(7_500));
        Ok(())
    }

    #[test]
    fn balance_sheet_balances_with_computed_earnings() -> Result<(), CliError> {
        let sheet = build_balance_sheet(BalanceSheetInputs {
            as_of: NaiveDate::default(),
            currency: Currency::USD,
            assets: vec![row(
                "1000",
                beankeeper::types::AccountType::Asset,
                10_000,
                2_500,
            )],
            liabilities: Vec::new(),
            equity: Vec::new(),
            revenue: vec![row(
                "4000",
                beankeeper::types::AccountType::Revenue,
                0,
                10_000,
            )],
            expenses: vec![row(
                "5000",
                beankeeper::types::AccountType::Expense,
                2_500,
                0,
            )],
        })?;
        assert_eq!(sheet.computed_earnings(), Money::usd(7_500));
        assert_eq!(sheet.equity().total(), Money::usd(7_500));
        assert!(sheet.balanced());
        assert!(
            sheet
                .equity()
                .accounts()
                .iter()
                .any(StatementLine::is_computed)
        );
        Ok(())
    }

    #[test]
    fn net_loss_is_negative() -> Result<(), CliError> {
        let stmt = build_income_statement(
            dates()?,
            Currency::USD,
            Vec::new(),
            vec![row("5000", beankeeper::types::AccountType::Expense, 100, 0)],
        )?;
        assert_eq!(stmt.net_income(), Money::usd(-100));
        let csv = render_income_statement_csv(&stmt)?;
        assert!(csv.contains("summary,net_income,,Net Loss,,-100"));
        Ok(())
    }

    #[test]
    fn income_statement_rejects_unrepresentable_subtotals() -> Result<(), CliError> {
        let result = build_income_statement(
            dates()?,
            Currency::USD,
            vec![
                row("4000", beankeeper::types::AccountType::Revenue, 0, i64::MAX),
                row("4001", beankeeper::types::AccountType::Revenue, 0, 1),
            ],
            Vec::new(),
        );
        assert!(matches!(result, Err(CliError::Validation(_))));
        Ok(())
    }
}
